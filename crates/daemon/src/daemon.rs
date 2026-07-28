//! The long-lived tray daemon: owns the SNI tray, exposes the D-Bus service interface,
//! runs periodic checks, reacts to menu/D-Bus commands, fires notifications, and launches
//! the independent GTK client for the windows.

use crate::dbus::Updater;
use crate::notify;
use crate::state::{Command, Shared, SharedState, Status};
use crate::tray::NixTray;
use anyhow::{Context, Result};
use ksni::TrayMethods;
use nun_core::check;
use nun_core::config::Config;
use nun_core::diff::PackageChange;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Run the daemon until "Quit". `config_path` is retained so we can hot-reload settings
/// changes between checks and pass it to the settings window.
pub async fn run(config_path: PathBuf) -> Result<()> {
    let mut cfg = Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    let (tx, mut rx) = mpsc::unbounded_channel::<Command>();
    let shared: SharedState = Arc::new(Mutex::new(Shared::default()));

    // Bring up the tray.
    let tray_handle = NixTray::new(cfg.icons.clone(), tx.clone())
        .spawn()
        .await
        .context("registering StatusNotifierItem (is a StatusNotifierHost running?)")?;

    // Export the D-Bus service so an independently-launched GTK client can drive us.
    // Requesting the well-known name also enforces a single daemon instance.
    let _conn = zbus::connection::Builder::session()
        .context("connecting to session bus")?
        .name(nun_core::ipc::BUS_NAME)
        .context("requesting D-Bus name (is another daemon already running?)")?
        .serve_at(
            nun_core::ipc::OBJECT_PATH,
            Updater {
                shared: shared.clone(),
                tx: tx.clone(),
                allowed_exes: allowed_caller_exes(),
            },
        )
        .context("exporting D-Bus object")?
        .build()
        .await
        .context("building D-Bus connection")?;

    // Periodic check timer + SIGUSR1 (external "check now" trigger, e.g. a systemd timer).
    let mut ticker = tokio::time::interval(cfg.interval());
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sigusr1 = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
        .context("installing SIGUSR1 handler")?;

    // Kick off an initial check shortly after startup.
    let _ = tx.send(Command::CheckNow);

    loop {
        let cmd = tokio::select! {
            _ = ticker.tick() => Command::CheckNow,
            _ = sigusr1.recv() => Command::CheckNow,
            maybe = rx.recv() => match maybe {
                Some(c) => c,
                None => break,
            },
        };

        match cmd {
            Command::CheckNow => {
                set_status(&shared, &tray_handle, Status::Checking).await;

                // Hot-reload config so Settings changes take effect (except interval,
                // which is bound to the ticker created at startup).
                if let Ok(reloaded) = Config::load(&config_path) {
                    cfg = reloaded;
                    tray_handle
                        .update({
                            let icons = cfg.icons.clone();
                            move |t: &mut NixTray| t.icons = icons.clone()
                        })
                        .await;
                }

                match check::run(&cfg).await {
                    Ok(outcome) if outcome.updates_available => {
                        let count = outcome.count();
                        set_status(&shared, &tray_handle, Status::UpdatesAvailable(count)).await;

                        let (is_new, dismissed) = {
                            let s = shared.lock().await;
                            (
                                s.last_notified_drv.as_deref() != Some(&outcome.candidate_drv),
                                s.dismissed_drv.as_deref() == Some(&outcome.candidate_drv),
                            )
                        };

                        let notified = if cfg.notify && is_new && !dismissed {
                            let (title, body) = if count == 0 {
                                (
                                    "NixOS system update available".to_string(),
                                    "Input revisions advanced; no package version changes \
                                     detected."
                                        .to_string(),
                                )
                            } else {
                                (
                                    format!("{count} NixOS update(s) available"),
                                    summarize(&outcome.changes),
                                )
                            };
                            if let Err(e) =
                                notify::notify(&title, &body, &cfg.icons.updates_available).await
                            {
                                tracing::warn!("notification failed: {e:#}");
                            }
                            true
                        } else {
                            false
                        };

                        let mut s = shared.lock().await;
                        s.changes = outcome.changes.clone();
                        s.candidate_lock = Some(outcome.candidate_lock.clone());
                        s.candidate_drv = Some(outcome.candidate_drv.clone());
                        if notified {
                            s.last_notified_drv = Some(outcome.candidate_drv.clone());
                        }
                    }
                    Ok(_up_to_date) => {
                        set_status(&shared, &tray_handle, Status::Idle).await;
                        let mut s = shared.lock().await;
                        s.changes.clear();
                        s.candidate_lock = None;
                        s.candidate_drv = None;
                        s.dismissed_drv = None;
                    }
                    Err(e) => {
                        tracing::error!("update check failed: {e:#}");
                        set_status(&shared, &tray_handle, Status::Error).await;
                        // Drop the previous candidate. The working copy is wiped at the
                        // start of every check, so after a failure the retained lock path
                        // may be gone or hold the unmodified lock — applying it would
                        // rebuild from the wrong input. Clearing also stops `GetUpdates`
                        // from serving a stale list alongside an error status.
                        // `dismissed_drv`/`last_notified_drv` are deliberately left alone
                        // so a transient failure doesn't re-notify an already-seen set.
                        let mut s = shared.lock().await;
                        s.changes.clear();
                        s.candidate_lock = None;
                        s.candidate_drv = None;
                    }
                }
            }

            Command::ViewUpdates => {
                if let Err(e) = spawn_gtk(&["updates"]) {
                    tracing::warn!("could not open updates window: {e:#}");
                }
            }

            Command::Settings => {
                if let Err(e) = spawn_gtk(&["settings", "--config", &config_path.to_string_lossy()])
                {
                    tracing::warn!("could not open settings window: {e:#}");
                }
            }

            Command::Apply => {
                let lock = shared.lock().await.candidate_lock.clone();
                if let Some(lock) = lock {
                    apply_flow(&cfg, &lock, &shared, &tray_handle).await;
                    // Re-check after applying so the tray reflects the new baseline.
                    let _ = tx.send(Command::CheckNow);
                }
            }

            Command::Dismiss => {
                let mut s = shared.lock().await;
                s.dismissed_drv = s.candidate_drv.clone();
            }

            Command::Quit => break,
        }
    }

    Ok(())
}

/// Verify the candidate lock leaves every `exclude_inputs` entry untouched, and log what it
/// *does* advance. Errors carry a user-presentable message.
async fn verify_pins(cfg: &Config, candidate_lock: &Path) -> Result<()> {
    let current = tokio::fs::read_to_string(cfg.flake_path.join("flake.lock"))
        .await
        .context("reading the repo's current flake.lock")?;
    let candidate = tokio::fs::read_to_string(candidate_lock)
        .await
        .context("reading the candidate flake.lock")?;

    let changed =
        nun_core::lock::ensure_pinned_unchanged(&current, &candidate, &cfg.exclude_inputs)?;
    tracing::info!(?changed, "candidate advances these inputs");
    Ok(())
}

/// Update both the shared state (for D-Bus readers) and the tray icon.
async fn set_status(shared: &SharedState, handle: &ksni::Handle<NixTray>, status: Status) {
    shared.lock().await.status = status;
    handle.update(move |t| t.status = status).await;
}

/// Short multi-line summary for a notification body (first few changes).
fn summarize(changes: &[PackageChange]) -> String {
    const MAX: usize = 6;
    let mut lines: Vec<String> = changes.iter().take(MAX).map(|c| c.render_line()).collect();
    if changes.len() > MAX {
        lines.push(format!("… and {} more", changes.len() - MAX));
    }
    lines.join("\n")
}

fn self_exe() -> Result<PathBuf> {
    std::env::current_exe().context("locating own executable path")
}

/// Executables allowed to call the state-changing D-Bus methods: our GTK client and the
/// daemon itself. Canonicalised so the comparison against `/proc/<pid>/exe` (which is
/// already fully resolved) is like-for-like.
fn allowed_caller_exes() -> Vec<PathBuf> {
    [gtk_client_exe(), self_exe().unwrap_or_default()]
        .into_iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect()
}

/// Locate the GTK client binary (installed next to the daemon), falling back to PATH.
fn gtk_client_exe() -> PathBuf {
    const NAME: &str = "nixos-update-notifier-gtk";
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(NAME);
            if sibling.exists() {
                return sibling;
            }
        }
    }
    PathBuf::from(NAME)
}

/// Launch the independent GTK client, which connects back over D-Bus.
fn spawn_gtk(args: &[&str]) -> Result<()> {
    std::process::Command::new(gtk_client_exe())
        .args(args)
        .spawn()
        .context("spawning GTK client")?;
    Ok(())
}

/// Run the privileged apply via pkexec, then notify the outcome and prompt for reboot if
/// warranted.
async fn apply_flow(
    cfg: &Config,
    candidate_lock: &Path,
    shared: &SharedState,
    handle: &ksni::Handle<NixTray>,
) {
    set_status(shared, handle, Status::Checking).await;

    let exe = match self_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("apply aborted: {e:#}");
            return;
        }
    };

    // Last-chance safety net before anything is written: verify this candidate does not
    // advance a pinned input. check.rs already restricts `nix flake update` to the
    // configured set, so this only fires if that logic is wrong — which is exactly when you
    // want it, since silently advancing e.g. a rev-pinned kernel input can leave the
    // machine unbootable.
    if let Err(e) = verify_pins(cfg, candidate_lock).await {
        tracing::error!("apply aborted: {e:#}");
        if cfg.notify {
            let _ = notify::notify("NixOS update blocked", &e.to_string(), &cfg.icons.error).await;
        }
        set_status(shared, handle, Status::Error).await;
        return;
    }

    // Install the candidate lock ourselves, UNPRIVILEGED — it's the user's own file, and
    // keeping root away from user-writable paths is a deliberate security property (see
    // nun_core::apply docs). Root only ever activates the result.
    let backup =
        match nun_core::apply::install_candidate_lock(&cfg.flake_path, candidate_lock).await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("could not install candidate lock: {e:#}");
                if cfg.notify {
                    let _ = notify::notify(
                        "NixOS update failed",
                        "Could not stage the new flake.lock; nothing was changed.",
                        &cfg.icons.error,
                    )
                    .await;
                }
                set_status(shared, handle, Status::Error).await;
                return;
            }
        };

    // pkexec <self> apply-privileged --repo <path> --host <host>
    // No lock path and no pass-through rebuild args cross the privilege boundary.
    let mut cmd = tokio::process::Command::new("pkexec");
    cmd.arg(&exe)
        .arg("apply-privileged")
        .arg("--repo")
        .arg(&cfg.flake_path)
        .arg("--host")
        .arg(&cfg.host_attr);

    if cfg.notify {
        let _ = notify::notify(
            "Applying NixOS updates…",
            "Running nixos-rebuild switch (you may be prompted to authenticate).",
            &cfg.icons.checking,
        )
        .await;
    }

    match cmd.status().await {
        Ok(status) if status.success() => {
            let reboot = nun_core::apply::current_vs_booted_differs().await;
            let (summary, body) = if reboot {
                (
                    "NixOS updated — reboot recommended",
                    "The kernel/initrd changed. Reboot to finish applying updates.",
                )
            } else {
                ("NixOS updated", "Updates applied successfully.")
            };
            if cfg.notify {
                let icon = if reboot {
                    &cfg.icons.updates_available
                } else {
                    &cfg.icons.idle
                };
                let _ = notify::notify(summary, body, icon).await;
            }
        }
        Ok(status) => {
            // Covers a failed rebuild and a cancelled/denied polkit prompt alike: put the
            // previous lock back so a failed apply leaves the repo exactly as it was.
            tracing::error!("apply failed: pkexec exited with {status}");
            nun_core::apply::restore_lock(&cfg.flake_path, &backup).await;
            if cfg.notify {
                let _ = notify::notify(
                    "NixOS update failed",
                    "nixos-rebuild did not complete (or was not authorized). The previous \
                     flake.lock was restored.",
                    &cfg.icons.error,
                )
                .await;
            }
            set_status(shared, handle, Status::Error).await;
        }
        Err(e) => {
            tracing::error!("could not launch pkexec: {e:#}");
            nun_core::apply::restore_lock(&cfg.flake_path, &backup).await;
            if cfg.notify {
                let _ = notify::notify(
                    "NixOS update failed",
                    "Could not launch the privileged helper (pkexec).",
                    &cfg.icons.error,
                )
                .await;
            }
            set_status(shared, handle, Status::Error).await;
        }
    }
}
