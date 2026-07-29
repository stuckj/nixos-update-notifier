//! Applying updates: unprivileged lock handling + the one privileged operation.
//!
//! Exactly one step of an apply genuinely needs root: activating the new system
//! (`nixos-rebuild switch` writes a new system profile generation, runs activation
//! scripts, and updates the bootloader). Everything else — backing up `flake.lock`,
//! installing the candidate lock, restoring it if the rebuild fails — operates on files
//! in the user's own repo and is done by the UNPRIVILEGED daemon.
//!
//! That split is deliberate and is a security property, not a stylistic one: root never
//! opens a path inside a user-writable directory, which removes an entire class of
//! symlink/TOCTOU problems (a `flake.lock` symlinked at `/etc/shadow` would otherwise let
//! root's copy leak or clobber arbitrary files). There is no validation to get subtly
//! wrong because root does no file I/O on those paths at all.
//!
//! Flow:
//!   1. (unprivileged) back up `flake.lock` to `flake.lock.bak.<epoch>`.
//!   2. (unprivileged) copy the candidate lock over it.
//!   3. (root, via pkexec) `nixos-rebuild switch --flake <repo>#<host>`.
//!   4. (unprivileged) restore the backup if the rebuild failed; report reboot need.
//!
//! We NEVER call `sudo` silently or auto-apply — step 3 always goes through polkit.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

/// Whether a reboot is warranted after a switch, judged by comparing the booted system
/// to the newly-activated current system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebootNeeded(pub bool);

/// How many `flake.lock.bak.<epoch>` files to keep in the repo.
///
/// Every apply writes one. Without a bound they accumulate in the user's config repo
/// forever — small individually, but unbounded and untracked clutter in a git working
/// tree. A few is enough to recover from a bad update; older ones are what git is for.
const KEEP_BACKUPS: usize = 3;

/// Delete all but the newest `keep` lock backups. Best-effort: never fails an apply.
async fn prune_old_backups(repo: &Path, keep: usize) {
    let Ok(mut entries) = tokio::fs::read_dir(repo).await else {
        return;
    };
    let mut backups: Vec<(u64, PathBuf)> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // `flake.lock.bak.<epoch>` — sort by the epoch in the name rather than by mtime,
        // which a checkout or a copy can rewrite.
        if let Some(stamp) = name.strip_prefix("flake.lock.bak.") {
            if let Ok(epoch) = stamp.parse::<u64>() {
                backups.push((epoch, entry.path()));
            }
        }
    }
    if backups.len() <= keep {
        return;
    }
    backups.sort_by_key(|(epoch, _)| std::cmp::Reverse(*epoch));
    for (_, path) in backups.into_iter().skip(keep) {
        match tokio::fs::remove_file(&path).await {
            Ok(()) => tracing::info!("pruned old lock backup {}", path.display()),
            Err(e) => tracing::warn!("could not prune {}: {e}", path.display()),
        }
    }
}

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

/// Back up the repo's current `flake.lock` and install the candidate over it, returning
/// the backup path so the caller can restore it on failure.
///
/// UNPRIVILEGED on purpose — these are the user's own files. See the module docs.
pub async fn install_candidate_lock(repo: &Path, candidate_lock: &Path) -> Result<PathBuf> {
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

    let backup: PathBuf = repo.join(format!("flake.lock.{}", timestamp()));
    tokio::fs::copy(&real_lock, &backup)
        .await
        .with_context(|| format!("backing up {} -> {}", real_lock.display(), backup.display()))?;
    tracing::info!("backed up flake.lock to {}", backup.display());

    prune_old_backups(repo, KEEP_BACKUPS).await;

    tokio::fs::copy(candidate_lock, &real_lock)
        .await
        .with_context(|| format!("installing candidate lock into {}", real_lock.display()))?;

    Ok(backup)
}

/// Restore a lock backup taken by [`install_candidate_lock`], leaving the repo as it was.
/// Best-effort: a failure here is logged, not propagated, since it runs on an error path.
pub async fn restore_lock(repo: &Path, backup: &Path) {
    let real_lock = repo.join("flake.lock");
    match tokio::fs::copy(backup, &real_lock).await {
        Ok(_) => tracing::info!("restored previous flake.lock from {}", backup.display()),
        Err(e) => tracing::error!(
            "could not restore {} -> {}: {e}",
            backup.display(),
            real_lock.display()
        ),
    }
}

/// Privileged entry point (runs as root via pkexec): the ONE operation that requires root.
///
/// It takes no file paths to write and no pass-through arguments — only the flake ref to
/// activate. Streams rebuild output to stdout so the caller can surface progress.
pub async fn rebuild_privileged(repo: &Path, host: &str) -> Result<RebootNeeded> {
    let flake_target = format!("{}#{}", repo.display(), host);
    println!("running: nixos-rebuild switch --flake {flake_target}");

    let status = Command::new("nixos-rebuild")
        .arg("switch")
        .arg("--flake")
        .arg(&flake_target)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("spawning nixos-rebuild")?;

    anyhow::ensure!(
        status.success(),
        "nixos-rebuild switch failed with {status}"
    );

    Ok(RebootNeeded(current_vs_booted_differs().await))
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

/// True if `/run/current-system` (just activated) differs from `/run/booted-system` in
/// kernel/initrd/kernel-modules/systemd — the standard NixOS "reboot needed" heuristic.
/// Public so the unprivileged daemon can decide whether to prompt for reboot after an
/// apply (these profile symlinks are world-readable).
pub async fn current_vs_booted_differs() -> bool {
    let current = signature_of(Path::new("/run/current-system")).await;
    let booted = signature_of(Path::new("/run/booted-system")).await;
    current != booted
}
