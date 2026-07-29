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

/// The generation `/run/current-system` currently points at, canonicalised.
///
/// Sampled either side of a rebuild to answer a question the exit status cannot: did the
/// new configuration actually go live? `switch-to-configuration` updates this link partway
/// through activation, so a rebuild can fail *after* the new system is running — a late
/// activation snippet erroring out, for instance. Rolling the lock back in that case would
/// leave the repo describing a system that is no longer the one booted.
pub fn current_system() -> Option<PathBuf> {
    std::fs::canonicalize("/run/current-system").ok()
}

/// Whether a reboot is warranted after a switch, judged by comparing the booted system
/// to the newly-activated current system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebootNeeded(pub bool);

/// Where the rebuild's output is written so the GUI can show it live.
///
/// A plain file rather than a pipe: the writer is a root process and the reader is an
/// unprivileged GTK client that may be opened, closed and reopened while the rebuild runs.
pub fn apply_log_path() -> PathBuf {
    runtime_dir().join("apply.log")
}

/// Touched by the daemon to ask the privileged rebuild to stop; polled by that rebuild.
///
/// The unprivileged daemon cannot signal a process that pkexec has turned into root, so
/// cancellation is cooperative. The privileged side only ever tests for existence — it
/// never writes or deletes — so this cannot become a destructive action performed as root.
pub fn cancel_flag_path() -> PathBuf {
    runtime_dir().join("cancel-apply")
}

fn runtime_dir() -> PathBuf {
    directories::ProjectDirs::from("org", "nixos", "nixos-update-notifier")
        .map(|d| d.cache_dir().to_path_buf())
        .unwrap_or_else(std::env::temp_dir)
}

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
pub async fn rebuild_privileged(
    repo: &Path,
    host: &str,
    cancel_flag: Option<&Path>,
) -> Result<RebootNeeded> {
    let flake_target = format!("{}#{}", repo.display(), host);
    println!("running: nixos-rebuild switch --flake {flake_target}");

    // Lead its own process group, so cancelling can take down the whole build tree.
    // `nixos-rebuild` is a thin wrapper: the actual work happens in the `nix build` it
    // spawns. Signalling only the wrapper leaves that child orphaned and still downloading.
    let mut child = Command::new("nixos-rebuild")
        .arg("switch")
        .arg("--flake")
        .arg(&flake_target)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
        .context("spawning nixos-rebuild")?;
    let pgid = child.id().context("nixos-rebuild has no pid")? as i32;

    // Cancellation has to be cooperative. pkexec fully becomes root, so the unprivileged
    // daemon cannot signal this process; instead it drops a flag file and we stop our own
    // child. We only ever READ that path — never write or delete it — so a caller-supplied
    // path cannot become a destructive action performed as root.
    let status = loop {
        tokio::select! {
            status = child.wait() => break status.context("waiting for nixos-rebuild")?,
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                if !cancel_flag.map(|p| p.exists()).unwrap_or(false) {
                    continue;
                }
                // Cancellation is always honoured — refusing would be worse than the risk.
                // What differs is the consequence, so report which phase we stopped in.
                //
                // Building and downloading are pure and interrupt cleanly. Setting the
                // system profile is an atomic symlink swap. But activation
                // (switch-to-configuration) is a SEQUENCE of imperative steps — bootloader
                // install, activation scripts, unit restarts — so stopping partway can
                // leave services stopped whose replacements never started. Recoverable by
                // re-running the switch or rebooting into the previous generation, which is
                // untouched, but worth saying out loud rather than pretending otherwise.
                let during_activation = activation_started();
                println!("cancel requested; stopping nixos-rebuild");
                terminate_group(pgid, &mut child).await;
                if during_activation {
                    anyhow::bail!(
                        "apply cancelled DURING activation. Some services may be stopped \
                         mid-switch; re-run the update or reboot (the previous generation is \
                         still in the boot menu) to get back to a consistent state."
                    );
                }
                anyhow::bail!("apply cancelled before activation; the system was not changed");
            }
        }
    };

    anyhow::ensure!(
        status.success(),
        "nixos-rebuild switch failed with {status}"
    );

    Ok(RebootNeeded(current_vs_booted_differs().await))
}

/// Whether the switch has reached activation, after which interrupting is unsafe.
///
/// The system profile is repointed as part of activation, so a profile that no longer
/// matches the running system means activation has at least begun.
/// Stop a cancelled rebuild and everything it spawned.
///
/// `nixos-rebuild` is a wrapper around `nix build`, so signalling only the direct child
/// leaves the build orphaned — still downloading, still writing to the log, still holding
/// the store lock — long after the UI has said "cancelled". Signalling the whole process
/// group is what actually stops the work.
///
/// SIGTERM first, deliberately: nix handles it, releasing its store lock and removing the
/// partial `.tmp-*` build directories it created. Going straight to SIGKILL would leave
/// that debris behind for the user to clean up. SIGKILL is only the fallback for a build
/// that ignores SIGTERM, so a cancel can never hang.
async fn terminate_group(pgid: i32, child: &mut tokio::process::Child) {
    const GRACE: std::time::Duration = std::time::Duration::from_secs(10);

    // SAFETY: killpg(2) on a group we created via process_group(0). A negative or zero
    // pgid would broadcast far more widely, so refuse anything that is not a real group.
    if pgid > 1 {
        unsafe { libc::killpg(pgid, libc::SIGTERM) };
    } else {
        let _ = child.start_kill();
    }

    if tokio::time::timeout(GRACE, child.wait()).await.is_ok() {
        return;
    }

    tracing::warn!("rebuild did not exit within {GRACE:?} of SIGTERM; sending SIGKILL");
    println!("build did not stop within 10s; forcing it down");
    if pgid > 1 {
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
    } else {
        let _ = child.start_kill();
    }
    let _ = child.wait().await;
}

fn activation_started() -> bool {
    generations_differ(
        Path::new("/nix/var/nix/profiles/system"),
        Path::new("/run/current-system"),
    )
}

/// Whether two system symlinks point at different generations.
///
/// Must CANONICALISE, not `read_link`. The two links are written in different styles:
/// the profile is relative (`system-134-link`) while `/run/current-system` is an absolute
/// store path. Comparing the raw link targets therefore always reported a difference, so
/// every cancellation claimed to have happened "during activation" and told the user their
/// services might be half-switched when nothing had been activated at all.
///
/// Unresolvable links mean we cannot tell, and the safe answer to "is it too late" is no —
/// an unnecessary scary warning is its own harm.
fn generations_differ(profile: &Path, current: &Path) -> bool {
    match (std::fs::canonicalize(profile), std::fs::canonicalize(current)) {
        (Ok(p), Ok(c)) => p != c,
        _ => false,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Scratch dir + the two system symlinks, mimicking how NixOS writes them:
    /// the profile link RELATIVE, `/run/current-system` ABSOLUTE.
    fn generation_links(tag: &str, profile_gen: &str, current_gen: &str) -> (PathBuf, PathBuf, PathBuf) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("nun-gen-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(root.join(profile_gen)).expect("gen dir");
        std::fs::create_dir_all(root.join(current_gen)).expect("gen dir");

        let profile = root.join("system");
        let current = root.join("current-system");
        // Relative target, exactly like /nix/var/nix/profiles/system.
        std::os::unix::fs::symlink(profile_gen, &profile).expect("profile link");
        // Absolute target, exactly like /run/current-system.
        std::os::unix::fs::symlink(root.join(current_gen), &current).expect("current link");
        (root, profile, current)
    }

    /// Regression test for a false "cancelled DURING activation" warning.
    ///
    /// Both links point at the SAME generation — nothing has been activated — but one is
    /// written relative and the other absolute. Comparing raw link targets makes them look
    /// different, which is what made every cancellation report a half-switched system.
    #[test]
    fn same_generation_is_not_activation_even_when_link_styles_differ() {
        let (root, profile, current) = generation_links("same", "gen-1", "gen-1");
        assert!(
            !generations_differ(&profile, &current),
            "identical generations must not be reported as activation in progress"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// The real signal: the profile has been swapped to a new generation but
    /// `/run/current-system` has not caught up, i.e. activation is genuinely under way.
    #[test]
    fn a_swapped_profile_means_activation_started() {
        let (root, profile, current) = generation_links("diff", "gen-2", "gen-1");
        assert!(
            generations_differ(&profile, &current),
            "a profile pointing at a newer generation means activation has begun"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// A cancelled rebuild must take its whole build tree down with it.
    ///
    /// This is a regression test for a bug that was invisible in manual testing: cancel
    /// killed `nixos-rebuild` in ~350ms and the UI said "cancelled", but the `nix build`
    /// it had spawned was merely orphaned and kept downloading for another 32 seconds.
    ///
    /// `sh` here stands in for `nixos-rebuild` and the `sleep` for the `nix build` it
    /// wraps: kill only the parent and the sleep survives. Needs nothing but a shell.
    #[tokio::test]
    async fn cancelling_kills_the_whole_process_group() {
        // Parent exits immediately after reporting the grandchild's pid, so a test that
        // only reaped the direct child would pass while the grandchild lived on.
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 300 & echo $!; wait")
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("spawning sh");

        let pgid = child.id().expect("pid") as i32;

        let mut out = String::new();
        {
            use tokio::io::AsyncReadExt;
            let mut stdout = child.stdout.take().expect("piped stdout");
            // Read just the pid line; the pipe stays open until the group dies.
            let mut buf = [0u8; 32];
            let n = stdout.read(&mut buf).await.expect("reading grandchild pid");
            out.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        let grandchild: i32 = out.trim().parse().expect("grandchild pid");

        // Precondition: signal 0 tests existence without delivering anything.
        assert_eq!(
            unsafe { libc::kill(grandchild, 0) },
            0,
            "grandchild should be running before cancel"
        );

        terminate_group(pgid, &mut child).await;

        // The grandchild is not our child, so it is never reaped by us and cannot be
        // mistaken for a zombie that still answers to signal 0.
        let mut gone = false;
        for _ in 0..50 {
            if unsafe { libc::kill(grandchild, 0) } != 0 {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            gone,
            "grandchild {grandchild} survived cancellation — the build would keep running"
        );
    }
}
