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
use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::path::{Path, PathBuf};
use tokio::process::Command;

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
}

impl CheckOutcome {
    pub fn count(&self) -> usize {
        self.changes
            .iter()
            .filter(|c| !matches!(c.kind, diff::ChangeKind::Removed))
            .count()
    }
}

fn workdir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("org", "nixos", "nixos-update-notifier")
        .context("could not determine XDG cache directory")?;
    Ok(dirs.cache_dir().join("candidate"))
}

/// Copy the flake repo into a fresh working directory, excluding `.git` (nix treats the
/// copy as a plain path flake and uses the working tree directly).
async fn refresh_working_copy(src: &Path) -> Result<PathBuf> {
    let dst = workdir()?;
    if dst.exists() {
        tokio::fs::remove_dir_all(&dst)
            .await
            .with_context(|| format!("clearing stale working copy {}", dst.display()))?;
    }
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }

    // `cp -a` preserves symlinks/perms; we then drop `.git` to avoid dirty-tree noise and
    // to keep the copy a plain path flake.
    let status = Command::new("cp")
        .arg("-a")
        .arg("--reflink=auto")
        .arg(src)
        .arg(&dst)
        .status()
        .await
        .context("spawning cp to make working copy")?;
    anyhow::ensure!(status.success(), "cp of flake repo failed");

    let git_dir = dst.join(".git");
    if git_dir.exists() {
        tokio::fs::remove_dir_all(&git_dir).await.ok();
    }
    Ok(dst)
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

    let work = refresh_working_copy(&cfg.flake_path).await?;
    let work_ref = work.display().to_string();

    // Baseline: current lock, from the copy.
    let current_drv = nix::toplevel_drv_path(&work_ref, &cfg.host_attr)
        .await
        .context("evaluating current toplevel drvPath")?;

    // Advance only the configured inputs.
    nix::flake_update_inputs(&work, &inputs)
        .await
        .context("running nix flake update in working copy")?;

    // Candidate: updated lock, from the same copy.
    let candidate_drv = nix::toplevel_drv_path(&work_ref, &cfg.host_attr)
        .await
        .context("evaluating candidate toplevel drvPath")?;

    let candidate_lock = work.join("flake.lock");

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
        });
    }

    // Offline closure diff over the two .drv paths.
    let mut changes = diff::diff_closures(&current_drv, &candidate_drv)
        .await
        .context("diffing derivation closures")?;

    // Best-effort changelog enrichment.
    changelog::enrich(cfg, &mut changes).await;

    Ok(CheckOutcome {
        updates_available: true,
        changes,
        candidate_lock,
        advanced_inputs: inputs,
        candidate_drv,
    })
}
