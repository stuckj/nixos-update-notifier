//! Non-destructive update check.
//!
//! Strategy (nothing here downloads a substitute):
//!   1. Copy the flake repo into a throwaway working copy under XDG cache (never touch
//!      the user's real repo during a check).
//!   2. Evaluate the *current* toplevel `.drvPath` from that copy (unmodified lock) —
//!      this is the exact baseline a `nixos-rebuild` right now would produce, isolating
//!      the comparison to "effect of advancing inputs" and avoiding false positives from
//!      uncommitted config edits.
//!   3. `nix flake update <inputs>` in the copy (honoring the include/exclude lists).
//!   4. Evaluate the *candidate* toplevel `.drvPath` from the copy.
//!   5. If the two `.drv` paths match → no updates. Otherwise diff their derivation
//!      closures offline and resolve changelogs.
//!
//! The candidate `flake.lock` produced in step 3 is what "Apply" later copies into the
//! real repo — so we hand its path back in the outcome.

use crate::changelog;
use crate::config::Config;
use crate::diff::{self, PackageChange};
use crate::nix;
use crate::pkgs;
use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub updates_available: bool,
    pub changes: Vec<PackageChange>,
    /// Path to the candidate `flake.lock` (inside the working copy) to apply on approval.
    pub candidate_lock: PathBuf,
    /// Inputs that were actually advanced.
    pub advanced_inputs: Vec<String>,
    /// The candidate toplevel `.drvPath` — identifies this pending update set (used for
    /// dismiss tracking and as the build target for an exact diff).
    pub candidate_drv: String,
    /// Inputs that could not be advanced, with the reason (e.g. a local fork whose repo has
    /// been moved away). The check still reports whatever the remaining inputs produced,
    /// rather than failing outright.
    pub failed_inputs: Vec<(String, String)>,
}

impl CheckOutcome {
    pub fn count(&self) -> usize {
        self.changes
            .iter()
            .filter(|c| !matches!(c.kind, diff::ChangeKind::Removed))
            .count()
    }
}

fn candidate_base() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("org", "nixos", "nixos-update-notifier")
        .context("could not determine XDG cache directory")?;
    Ok(dirs.cache_dir().join("candidate"))
}

/// Per-process working directory. Keying on the PID means overlapping checks in different
/// processes (e.g. a manual `check` while the daemon is checking) never delete each other's
/// working copy mid-flight. Checks within one process are serialized, so reuse is safe.
fn workdir() -> Result<PathBuf> {
    Ok(candidate_base()?.join(std::process::id().to_string()))
}

/// Where this process's candidate `flake.lock` is kept once the working copy is discarded.
fn candidate_lock_path() -> Result<PathBuf> {
    Ok(candidate_base()?.join(format!("{}.flake.lock", std::process::id())))
}

/// Copy the candidate lock out of the working copy so the copy itself can be deleted.
async fn save_candidate_lock(work: &Path) -> Result<PathBuf> {
    let dst = candidate_lock_path()?;
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    tokio::fs::copy(work.join("flake.lock"), &dst)
        .await
        .with_context(|| format!("saving candidate lock to {}", dst.display()))?;
    Ok(dst)
}

/// Remove this process's leftovers. Called on daemon shutdown so a clean exit leaves
/// nothing behind, rather than waiting for some later run to notice the PID is gone.
pub async fn cleanup_own_artifacts() {
    if let Ok(dir) = workdir() {
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
    if let Ok(lock) = candidate_lock_path() {
        let _ = tokio::fs::remove_file(&lock).await;
    }
}

/// Best-effort removal of working dirs left behind by processes that are no longer running
/// (Linux: judged by `/proc/<pid>`). Keeps the cache from accumulating after crashes.
async fn cleanup_stale_workdirs(base: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(base).await else {
        return;
    };
    let me = std::process::id();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Both shapes are keyed by PID: `<pid>` working copies and `<pid>.flake.lock`
        // saved candidates. A crashed run leaves one of each behind.
        let pid = name.split('.').next().and_then(|s| s.parse::<u32>().ok());
        let Some(pid) = pid else { continue };
        if pid == me || Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        let path = entry.path();
        let _ = if path.is_dir() {
            tokio::fs::remove_dir_all(&path).await
        } else {
            tokio::fs::remove_file(&path).await
        };
    }
}

/// Build a scratch flake containing ONLY `flake.nix` and `flake.lock`, for advancing
/// inputs.
///
/// This used to copy the entire repo — hundreds of MB for a real config, rewritten on
/// every check. It isn't needed: `nix flake update` only reads the flake's `inputs`
/// section, so two files are enough, and the resulting lock can then be applied to the
/// real repo at evaluation time via `--reference-lock-file`. That keeps the user's repo
/// untouched while costing a few KB instead of a whole second checkout.
async fn scratch_flake(src: &Path) -> Result<PathBuf> {
    let dst = workdir()?;
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
        cleanup_stale_workdirs(parent).await;
    }
    if dst.exists() {
        tokio::fs::remove_dir_all(&dst)
            .await
            .with_context(|| format!("clearing stale scratch dir {}", dst.display()))?;
    }
    tokio::fs::create_dir_all(&dst)
        .await
        .with_context(|| format!("creating scratch dir {}", dst.display()))?;

    for f in ["flake.nix", "flake.lock"] {
        tokio::fs::copy(src.join(f), dst.join(f))
            .await
            .with_context(|| format!("copying {f} into the scratch flake"))?;
    }
    Ok(dst)
}

/// Mark which changes affect software actually installed on the running system.
///
/// Diffing *derivation* closures is what keeps a check download-free, but it necessarily
/// includes BUILD-TIME dependencies: `go` appeared as a new package on a machine with no
/// Go installed, because something in the config is built with it. Comparing against the
/// running system's realised closure separates the two, and costs one local store query.
///
/// (Supersession needs no special handling: comparing version *sets* per package makes
/// `zfs-user 2.4.2, 2.4.3 -> 2.4.3` read correctly on its own.)
async fn annotate_runtime(changes: &mut [PackageChange]) {
    let installed = nix::closure_names("/run/current-system").await;
    if installed.is_empty() {
        // Could not read the running system (unusual). Leave everything as-is rather than
        // mislabel every entry as build-time.
        tracing::warn!("could not read the running system's closure; skipping annotation");
        return;
    }

    for c in changes.iter_mut() {
        c.runtime = installed.contains_package(&c.name);
    }

    let build_only = changes.iter().filter(|c| !c.runtime).count();
    tracing::debug!(
        total = changes.len(),
        build_only,
        "annotated changes against the running system"
    );
}

/// Run a full check. Returns `Ok(outcome)`; the caller decides how to surface it.
pub async fn run(cfg: &Config) -> Result<CheckOutcome> {
    anyhow::ensure!(
        cfg.flake_path.join("flake.nix").exists(),
        "no flake.nix at {}",
        cfg.flake_path.display()
    );

    // Resolve the effective input set from what the flake actually declares.
    let available = nix::flake_input_names(&cfg.flake_path)
        .await
        .context("listing flake inputs")?;
    let inputs = cfg.effective_update_inputs(&available);
    anyhow::ensure!(
        !inputs.is_empty(),
        "no inputs to update (available: {:?}, update_inputs: {:?}, exclude_inputs: {:?})",
        available,
        cfg.update_inputs,
        cfg.exclude_inputs
    );

    let repo_ref = cfg.flake_path.display().to_string();
    let scratch = scratch_flake(&cfg.flake_path).await?;

    // Baseline: the real repo, evaluated with its own lock. Both sides evaluate the same
    // tree, so the comparison isolates the effect of advancing inputs even if the repo has
    // uncommitted edits.
    let current_drv = nix::toplevel_drv_path(&repo_ref, &cfg.host_attr, None)
        .await
        .context("evaluating current toplevel drvPath")?;

    // Advance only the configured inputs, in the scratch flake — the user's repo is never
    // written to. A failure here is per-input and non-fatal: we report which ones could not
    // move and continue with the rest, so one broken input cannot hide pending updates
    // from all the others.
    let failed_inputs = nix::flake_update_inputs(&scratch, &inputs)
        .await
        .context("running nix flake update in the scratch flake")?;
    anyhow::ensure!(
        failed_inputs.len() < inputs.len(),
        "no inputs could be advanced: {}",
        failed_inputs
            .iter()
            .map(|(n, r)| format!("{n}: {r}"))
            .collect::<Vec<_>>()
            .join("; ")
    );

    // Candidate: the REAL repo again, but evaluated against the scratch flake's updated
    // lock. Same tree, different inputs — which is exactly the question being asked.
    let scratch_lock = scratch.join("flake.lock");
    let candidate_drv = nix::toplevel_drv_path(&repo_ref, &cfg.host_attr, Some(&scratch_lock))
        .await
        .context("evaluating candidate toplevel drvPath")?;

    // Keep the candidate lock (a few KB) and drop the scratch dir.
    let candidate_lock = save_candidate_lock(&scratch).await?;
    if let Err(e) = tokio::fs::remove_dir_all(&scratch).await {
        tracing::warn!("could not remove scratch dir {}: {e}", scratch.display());
    }

    tracing::debug!(
        current = %current_drv,
        candidate = %candidate_drv,
        advanced = ?inputs,
        "evaluated current vs candidate toplevel"
    );

    if current_drv == candidate_drv {
        return Ok(CheckOutcome {
            updates_available: false,
            changes: Vec::new(),
            candidate_lock,
            advanced_inputs: inputs,
            candidate_drv,
            failed_inputs,
        });
    }

    // Offline diff over the two derivation closures, using nix's own pname/version
    // metadata rather than parsing `diff-closures`' rendered output (see `pkgs`).
    let mut changes = pkgs::diff(&current_drv, &candidate_drv)
        .await
        .context("diffing derivation closures")?;

    annotate_runtime(&mut changes).await;

    // Best-effort changelog enrichment.
    changelog::enrich(cfg, &mut changes).await;

    Ok(CheckOutcome {
        updates_available: true,
        changes,
        candidate_lock,
        advanced_inputs: inputs,
        candidate_drv,
        failed_inputs,
    })
}
