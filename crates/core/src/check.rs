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
    /// The repo, with its own lock, describes a different system from the one running.
    pub running_differs: bool,
    /// The repo's `flake.lock` the candidate was made from. Installing the candidate over a
    /// lock that has changed since would silently move inputs back to these revisions.
    pub base_lock: String,
    /// …and with the newest `flake.lock` backup instead, it describes the running system.
    pub revert_matches_running: bool,
    /// With nothing newer upstream but the repo not applied: what differs from the running
    /// system.
    pub unapplied: Option<crate::ipc::UnappliedDiff>,
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

/// The scratch flakes a process uses: `check` for update checks, `upgrade` for release
/// upgrade previews. Both can run at once, so each has its own directory. `accepted` holds
/// the lock of the check result on display (see [`accept_candidate_lock`]).
const SCRATCH_KINDS: [&str; 3] = ["check", "upgrade", "accepted"];

/// Copy a check's candidate lock to where Apply reads it from. Every check rewrites its own
/// candidate file as soon as it has locked, including one whose result is later discarded,
/// so applying straight from it could install an update set other than the one shown.
pub fn accept_candidate_lock(candidate: &Path) -> Result<PathBuf> {
    let dst = candidate_path("accepted", "flake.lock")?;
    std::fs::copy(candidate, &dst)
        .with_context(|| format!("keeping the candidate lock at {}", dst.display()))?;
    Ok(dst)
}

/// Per-process working directory. Keying on the PID means overlapping checks in different
/// processes (e.g. a manual `check` while the daemon is checking) never delete each other's
/// working copy mid-flight. Checks within one process are serialized, so reuse is safe.
/// Names start with the PID either way, which is what stale-directory cleanup keys on.
fn workdir(kind: &str) -> Result<PathBuf> {
    let pid = std::process::id();
    Ok(candidate_base()?.join(match kind {
        "check" => pid.to_string(),
        _ => format!("{pid}.{kind}"),
    }))
}

/// Where this process's candidate `file` (`flake.lock`, `flake.nix`) is kept once the
/// working copy is discarded.
fn candidate_path(kind: &str, file: &str) -> Result<PathBuf> {
    let pid = std::process::id();
    Ok(candidate_base()?.join(match kind {
        "check" => format!("{pid}.{file}"),
        _ => format!("{pid}.{kind}.{file}"),
    }))
}

/// Copy the candidate lock out of the working copy so the copy itself can be deleted.
async fn save_candidate_lock(work: &Path) -> Result<PathBuf> {
    save_file(work, "check", "flake.lock").await
}

/// Copy the candidate `flake.lock` and `flake.nix` out of the working copy.
pub(crate) async fn save_candidate(work: &Path, kind: &str) -> Result<(PathBuf, PathBuf)> {
    Ok((
        save_file(work, kind, "flake.lock").await?,
        save_file(work, kind, "flake.nix").await?,
    ))
}

async fn save_file(work: &Path, kind: &str, file: &str) -> Result<PathBuf> {
    let dst = candidate_path(kind, file)?;
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    tokio::fs::copy(work.join(file), &dst)
        .await
        .with_context(|| format!("saving candidate {file} to {}", dst.display()))?;
    Ok(dst)
}

/// Remove this process's leftovers. Called on daemon shutdown so a clean exit leaves
/// nothing behind, rather than waiting for some later run to notice the PID is gone.
pub async fn cleanup_own_artifacts() {
    for kind in SCRATCH_KINDS {
        if let Ok(dir) = workdir(kind) {
            let _ = tokio::fs::remove_dir_all(&dir).await;
        }
        for file in ["flake.lock", "flake.nix"] {
            if let Ok(p) = candidate_path(kind, file) {
                let _ = tokio::fs::remove_file(&p).await;
            }
        }
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
pub(crate) async fn scratch_flake(src: &Path, kind: &str) -> Result<PathBuf> {
    let dst = workdir(kind)?;
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
pub(crate) async fn annotate_runtime(changes: &mut [PackageChange]) {
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
    let scratch = scratch_flake(&cfg.flake_path, "check").await?;
    let base_lock = tokio::fs::read_to_string(scratch.join("flake.lock"))
        .await
        .context("reading the repo's flake.lock")?;

    // Baseline: the real repo, evaluated with its own lock. Both sides evaluate the same
    // tree, so the comparison isolates the effect of advancing inputs even if the repo has
    // uncommitted edits.
    let current_drv = nix::toplevel_drv_path(&repo_ref, &cfg.host_attr, None, &[])
        .await
        .context("evaluating current toplevel drvPath")?;
    let running_differs = repo_differs_from_running(&current_drv).await;

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
    let candidate_drv = nix::toplevel_drv_path(&repo_ref, &cfg.host_attr, Some(&scratch_lock), &[])
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
        // Revert is offered only in this state (nothing newer, system not applied), so the
        // extra evaluation is spent only here.
        let revert_matches_running = running_differs && backup_matches_running(cfg).await;
        let unapplied = if running_differs {
            Some(unapplied_diff(cfg, &current_drv).await)
        } else {
            None
        };
        return Ok(CheckOutcome {
            updates_available: false,
            changes: Vec::new(),
            candidate_lock,
            advanced_inputs: inputs,
            candidate_drv,
            failed_inputs,
            running_differs,
            revert_matches_running,
            unapplied,
            base_lock,
        });
    }

    // Offline diff over the two derivation closures, using nix's own pname/version
    // metadata rather than parsing `diff-closures`' rendered output (see `pkgs`).
    let mut changes = pkgs::diff(&current_drv, &candidate_drv)
        .await
        .context("diffing derivation closures")?;

    annotate_runtime(&mut changes).await;

    // Best-effort changelog enrichment, against the nixpkgs the candidate would install.
    changelog::enrich(cfg, &candidate_lock, &mut changes).await;

    Ok(CheckOutcome {
        updates_available: true,
        changes,
        candidate_lock,
        advanced_inputs: inputs,
        candidate_drv,
        failed_inputs,
        running_differs,
        revert_matches_running: false,
        unapplied: None,
        base_lock,
    })
}

/// What the running system lacks compared with the repo: the same download-free diff a
/// check makes, from the running system's derivation (`nix derivation show` resolves an
/// output path to the derivation that built it) to the repo's. A failure to list it does
/// not fail the check; it is reported instead, since the state itself is already known.
async fn unapplied_diff(cfg: &Config, repo_drv: &str) -> crate::ipc::UnappliedDiff {
    let unavailable = |why: String| crate::ipc::UnappliedDiff {
        changes: Vec::new(),
        error: Some(why),
    };
    let Some(running) = crate::apply::current_system() else {
        return unavailable("could not resolve /run/current-system".into());
    };
    let base = diff_base(&running, Path::new("/nix/var/nix/profiles/system"));
    // The running system is part of the key even when the diff starts from its generation:
    // which packages count as installed is judged against what is running.
    let key = (
        running.display().to_string(),
        base.display().to_string(),
        repo_drv.to_string(),
    );
    if let Some((k, cached)) = UNAPPLIED_CACHE.lock().ok().and_then(|c| c.clone()) {
        if k == key {
            return cached;
        }
    }
    if !nix::deriver_present(&key.1).await {
        return unavailable(
            "the running system's derivation is not in the local store (it was built \
             elsewhere, or garbage-collected), so what differs cannot be listed"
                .into(),
        );
    }
    let result = diff_running(cfg, &key.1, repo_drv).await;
    if result.error.is_none() {
        if let Ok(mut c) = UNAPPLIED_CACHE.lock() {
            *c = Some((key, result.clone()));
        }
    }
    result
}

/// The last listed difference, by (running system, diff base, repo derivation). The not-applied state
/// can last for days (a config edit kept unapplied) and every check would otherwise read
/// both closures again to produce the same list.
type UnappliedKey = (String, String, String);
static UNAPPLIED_CACHE: std::sync::Mutex<Option<(UnappliedKey, crate::ipc::UnappliedDiff)>> =
    std::sync::Mutex::new(None);

/// The system to diff the repo's toplevel against. Booted or switched into a
/// specialisation, `/run/current-system` is that specialisation, while the repo's toplevel
/// is the whole generation (base plus every specialisation); the generation it belongs to
/// is the like-for-like comparison.
fn diff_base(running: &Path, profile: &Path) -> PathBuf {
    match std::fs::canonicalize(profile) {
        Ok(gen) if gen != running && is_system_or_specialisation(&gen, running) => gen,
        _ => running.to_path_buf(),
    }
}

async fn diff_running(cfg: &Config, running: &str, repo_drv: &str) -> crate::ipc::UnappliedDiff {
    match pkgs::diff(running, repo_drv).await {
        Ok(mut changes) => {
            annotate_runtime(&mut changes).await;
            // Installed-ness is judged against the running system, where a package the
            // repo adds cannot be yet; without building the repo's system there is no
            // telling an added program from an added build input, so it is listed plainly.
            for c in changes.iter_mut() {
                if c.kind == diff::ChangeKind::Added {
                    c.runtime = true;
                }
            }
            changelog::enrich(cfg, &cfg.flake_path.join("flake.lock"), &mut changes).await;
            crate::ipc::UnappliedDiff {
                changes,
                error: None,
            }
        }
        Err(e) => {
            tracing::warn!("could not diff the running system against the repo: {e:#}");
            crate::ipc::UnappliedDiff {
                changes: Vec::new(),
                error: Some(format!("{e:#}")),
            }
        }
    }
}

/// Whether restoring the newest `flake.lock` backup would make the repo describe exactly
/// the running system — the case where the lock was advanced without a switch. Any other
/// backup (the lock before an update that did go live, say) would only move the repo
/// further from what is running, so it is not offered.
async fn backup_matches_running(cfg: &Config) -> bool {
    let Some(backup) = crate::apply::revertable_lock_backup(&cfg.flake_path).await else {
        return false;
    };
    let repo_ref = cfg.flake_path.display().to_string();
    match nix::toplevel_drv_path(&repo_ref, &cfg.host_attr, Some(&backup), &[]).await {
        Ok(drv) => is_running_system(&drv).await == Some(true),
        Err(e) => {
            tracing::debug!("could not evaluate with {}: {e:#}", backup.display());
            false
        }
    }
}

/// Whether the system the repo describes (with its own lock) is not the one running.
///
/// "No updates" only compares the repo's lock with a newer one. If `flake.lock` was
/// advanced but never switched to — an apply still running, or killed before it could
/// restore the lock, or a hand-run `nix flake update` — the repo is current and the machine
/// is not. Comparing the baseline's output path with `/run/current-system` catches that. It
/// equally reports configuration edits that have not been rebuilt yet, which is also a
/// system that differs from its flake.
///
/// `false` when either side cannot be determined: a false "not applied" would offer a
/// rebuild for no reason.
async fn repo_differs_from_running(current_drv: &str) -> bool {
    is_running_system(current_drv).await == Some(false)
}

/// Whether `drv` builds the system at `/run/current-system` — the system itself or one of
/// its specialisations, which is what runs after booting or switching into one. `None` if
/// either side cannot be determined.
async fn is_running_system(drv: &str) -> Option<bool> {
    let running = crate::apply::current_system()?;
    match nix::drv_out_path(drv).await {
        Ok(out) => Some(is_system_or_specialisation(Path::new(&out), &running)),
        Err(e) => {
            tracing::warn!("could not resolve the output path of {drv}: {e:#}");
            None
        }
    }
}

/// `running` (canonical) is `toplevel` or one of its `specialisation/*` links' targets.
fn is_system_or_specialisation(toplevel: &Path, running: &Path) -> bool {
    if toplevel == running {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(toplevel.join("specialisation")) else {
        return false;
    };
    entries
        .flatten()
        .any(|e| std::fs::canonicalize(e.path()).is_ok_and(|p| p == running))
}

#[cfg(test)]
mod tests {
    use super::{diff_base, is_system_or_specialisation};

    /// Running a specialisation, the diff starts from the generation it belongs to (what
    /// the repo's toplevel is comparable with); otherwise from the running system itself.
    #[test]
    fn the_diff_starts_from_the_generation_of_a_running_specialisation() {
        let root = crate::testutil::scratch_dir("diff-base");
        let gen = root.join("aaa-nixos-system");
        let gaming = root.join("bbb-nixos-system-gaming");
        let older = root.join("ccc-nixos-system");
        for d in [&gen, &gaming, &older] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::create_dir_all(gen.join("specialisation")).unwrap();
        std::os::unix::fs::symlink(&gaming, gen.join("specialisation/gaming")).unwrap();
        let profile = root.join("system");
        std::os::unix::fs::symlink(&gen, &profile).unwrap();

        let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
        assert_eq!(diff_base(&canon(&gaming), &profile), canon(&gen));
        assert_eq!(diff_base(&canon(&gen), &profile), canon(&gen));
        // Running something the current profile does not contain (an older generation
        // switched to by hand): diff from what is running.
        assert_eq!(diff_base(&canon(&older), &profile), canon(&older));
    }

    /// Mirrors the store layout: a specialisation is its own store path, linked from
    /// `<toplevel>/specialisation/<name>`, and `/run/current-system` points at it directly.
    #[test]
    fn a_running_specialisation_is_the_running_system() {
        let root = crate::testutil::scratch_dir("spec");
        let toplevel = root.join("aaa-nixos-system");
        let gaming = root.join("bbb-nixos-system-gaming");
        let other = root.join("ccc-nixos-system");
        for d in [&toplevel, &gaming, &other] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::create_dir_all(toplevel.join("specialisation")).unwrap();
        std::os::unix::fs::symlink(&gaming, toplevel.join("specialisation/gaming")).unwrap();

        let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap();
        let top = canon(&toplevel);
        assert!(is_system_or_specialisation(&top, &top));
        assert!(is_system_or_specialisation(&top, &canon(&gaming)));
        assert!(!is_system_or_specialisation(&top, &canon(&other)));
    }
}
