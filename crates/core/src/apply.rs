//! Applying updates: privileged rebuild + reboot detection.
//!
//! The unprivileged daemon never runs `nixos-rebuild` itself. Instead it invokes this
//! binary's hidden `apply-privileged` subcommand through `pkexec`, so the user gets an
//! explicit polkit auth prompt. We NEVER call `sudo` silently or auto-apply.
//!
//! Flow (privileged side):
//!   1. Back up the real repo's `flake.lock` to `flake.lock.bak.<timestamp>`.
//!   2. Copy the candidate lock over it.
//!   3. `nixos-rebuild switch --flake <repo>#<host>` (streaming output).
//!   4. Report whether a reboot is warranted (kernel / initrd / systemd changed).
//!
//! On rebuild failure, the backed-up lock is restored.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

/// Whether a reboot is warranted after a switch, judged by comparing the booted system
/// to the newly-activated current system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebootNeeded(pub bool);

fn timestamp() -> String {
    // Seconds since the Unix epoch — enough to make each backup name unique, without
    // pulling in chrono for calendar formatting.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("bak.{secs}")
}

/// Privileged entry point (runs as root via pkexec). Streams rebuild output to stdout so
/// the calling daemon can surface progress.
pub async fn apply_privileged(
    repo: &Path,
    host: &str,
    candidate_lock: &Path,
    rebuild_extra_args: &[String],
) -> Result<RebootNeeded> {
    let real_lock = repo.join("flake.lock");
    anyhow::ensure!(
        real_lock.exists(),
        "no flake.lock in target repo {}",
        repo.display()
    );
    anyhow::ensure!(
        candidate_lock.exists(),
        "candidate lock {} does not exist",
        candidate_lock.display()
    );

    // Snapshot the current system's kernel/initrd BEFORE switching (for the debug log
    // below). The actual reboot decision compares booted vs current after activation, in
    // `current_vs_booted_differs()`.
    let before = boot_signature().await;

    // 1. Back up the current lock.
    let backup: PathBuf = repo.join(format!("flake.lock.{}", timestamp()));
    tokio::fs::copy(&real_lock, &backup)
        .await
        .with_context(|| format!("backing up {} -> {}", real_lock.display(), backup.display()))?;
    println!("backed up flake.lock to {}", backup.display());

    // 2. Install the candidate lock.
    tokio::fs::copy(candidate_lock, &real_lock)
        .await
        .with_context(|| format!("installing candidate lock into {}", real_lock.display()))?;

    // 3. Rebuild.
    let flake_target = format!("{}#{}", repo.display(), host);
    println!("running: nixos-rebuild switch --flake {flake_target}");
    let mut cmd = Command::new("nixos-rebuild");
    cmd.arg("switch")
        .arg("--flake")
        .arg(&flake_target)
        .args(rebuild_extra_args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = cmd.status().await.context("spawning nixos-rebuild")?;

    if !status.success() {
        // Restore the original lock so a failed apply leaves the repo untouched.
        eprintln!("nixos-rebuild failed ({status}); restoring previous flake.lock");
        tokio::fs::copy(&backup, &real_lock).await.ok();
        anyhow::bail!("nixos-rebuild switch failed with {status}");
    }

    // 4. Decide whether a reboot is warranted.
    let after = boot_signature().await;
    // If the booted signature no longer matches the (new) current signature, a reboot is
    // needed to run the new kernel/initrd. We compare the freshly-activated system's
    // signature against what is actually booted.
    let needs = current_vs_booted_differs().await;
    tracing::debug!(?before, ?after, needs, "reboot decision");

    Ok(RebootNeeded(needs))
}

/// A stable signature of the security-relevant boot artefacts of a system profile.
async fn signature_of(system: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for artefact in ["kernel", "initrd", "kernel-modules", "systemd"] {
        let p = system.join(artefact);
        if let Ok(target) = tokio::fs::read_link(&p).await {
            out.push((artefact.to_string(), target.display().to_string()));
        }
    }
    out
}

async fn boot_signature() -> Vec<(String, String)> {
    signature_of(Path::new("/run/current-system")).await
}

/// True if `/run/current-system` (just activated) differs from `/run/booted-system` in
/// kernel/initrd/kernel-modules/systemd — the standard NixOS "reboot needed" heuristic.
/// Public so the unprivileged daemon can decide whether to prompt for reboot after an
/// apply (these profile symlinks are world-readable).
pub async fn current_vs_booted_differs() -> bool {
    let current = signature_of(Path::new("/run/current-system")).await;
    let booted = signature_of(Path::new("/run/booted-system")).await;
    current != booted
}
