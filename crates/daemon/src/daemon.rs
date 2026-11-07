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
use nun_core::failure::Failure;
use nun_core::ipc::{ApplyInfo, ApplyState, UpgradeInfo, UpgradeState};
use nun_core::release::{self, ReleaseOffer, UpgradePreview, Version};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{mpsc, Mutex};

/// A finished check: the update check itself, and the release lookup made alongside it.
type CheckResult = (Result<check::CheckOutcome>, Result<Option<ReleaseOffer>>);

/// What an apply installs before the rebuild.
enum ApplyPlan {
    /// The candidate lock from the last check.
    Update { lock: PathBuf, base: String },
    /// Nothing: rebuild from the repo as it is, to finish an unfinished switch or to apply
    /// a lock that was advanced without one.
    Rebuild,
    /// A previewed release upgrade: a rewritten `flake.nix` and its lock.
    Upgrade(UpgradePreview),
}

impl ApplyPlan {
    fn kind(&self) -> String {
        match self {
            ApplyPlan::Update { .. } => "update".into(),
            ApplyPlan::Rebuild => "rebuild".into(),
            ApplyPlan::Upgrade(p) => format!("upgrade to NixOS {}", p.target),
        }
    }
}

/// How an apply ended, for `GetDetails`.
struct ApplyOutcome {
    state: ApplyState,
    failure: Option<Failure>,
}

/// Run the daemon until "Quit". `config_path` is retained so we can hot-reload settings
/// changes between checks and pass it to the settings window.
pub async fn run(config_path: PathBuf) -> Result<()> {
    let mut cfg = Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    let (tx, mut rx) = mpsc::unbounded_channel::<Command>();
    let shared: SharedState = Arc::new(Mutex::new(Shared {
        persisted: crate::persist::Persisted::load(),
        ..Shared::default()
    }));

    // Export the D-Bus service so an independently-launched GTK client can drive us.
    // Owning the well-known name is also what keeps this a single instance, so it is
    // claimed before the tray exists and without replacement on either side: zbus's
    // default flags let a second daemon take the name from the first, leaving two tray
    // icons and the windows talking to whichever started last.
    let conn = zbus::connection::Builder::session()
        .context("connecting to session bus")?
        .allow_name_replacements(false)
        .replace_existing_names(false)
        .name(nun_core::ipc::BUS_NAME)
        .context("requesting D-Bus name")?
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
        .await;
    let _conn = match conn {
        Ok(c) => c,
        // Already running — typically the service, with the app also launched from the
        // menu. Behave like "open the notifier" rather than an error.
        Err(zbus::Error::NameTaken) => {
            // Exit cleanly either way: a failure would have systemd restart the service
            // over and over while the other instance holds the name. Only a launch by the
            // user (not the service manager) asks for the window.
            if std::env::var_os("NIXOS_UPDATE_NOTIFIER_SERVICE").is_some() {
                tracing::info!("another instance is running; exiting");
            } else {
                tracing::info!("another instance is running; asking it to show its window");
                if let Err(e) = show_running_instance().await {
                    tracing::warn!("{e:#}");
                }
            }
            return Ok(());
        }
        Err(e) => return Err(e).context("building D-Bus connection"),
    };

    // Bring up the tray.
    let tray_handle = NixTray::new(cfg.icons.clone(), tx.clone())
        .spawn()
        .await
        .context("registering StatusNotifierItem (is a StatusNotifierHost running?)")?;

    // Scheduling is a wall-clock THRESHOLD, polled once a minute — not a monotonic
    // interval. Tokio's timer is driven by a monotonic clock, which on Linux does not
    // advance while the machine is suspended, so on a laptop a 24h interval silently
    // becomes "24h of uptime": sleep 8h a night and checks drift a day later each cycle,
    // eventually never landing while you are awake. Comparing wall-clock time against the
    // last check means a suspended machine simply finds the threshold already passed and
    // checks shortly after it wakes.
    const POLL: Duration = Duration::from_secs(60);
    let mut ticker = tokio::time::interval(POLL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_check: Option<SystemTime> = None;
    let mut sigusr1 = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
        .context("installing SIGUSR1 handler")?;

    // Checks run in their own task and report back here. Awaiting `check::run` inline
    // would block this loop for the entire check — 40s to several minutes on a cold
    // evaluation — during which every tray menu action (Settings, View updates, Quit)
    // would sit unprocessed in the channel and the daemon would look dead. Menu clicks
    // then all fired at once when the check finally finished.
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<CheckResult>();
    let mut checking = false;
    // A check and an apply must not overlap in effect. A check compares against the repo's
    // flake.lock, which an apply replaces, so a check that was running when an apply began
    // would report on a lock that is no longer the baseline — "no updates", while the
    // running system is still the old one — and its status would overwrite "applying".
    // Such a result is discarded and a fresh check runs once the apply is over.
    let mut check_stale = false;
    // Applies run in their own task for the same reason, so Cancel stays reachable.
    let (apply_done_tx, mut apply_done_rx) = mpsc::unbounded_channel::<ApplyOutcome>();
    let mut applying = false;
    let (preview_tx, mut preview_rx) =
        mpsc::unbounded_channel::<(u64, Version, Result<UpgradePreview>)>();
    let mut previewing = false;
    // Bumped whenever the repo's flake files change under us (an apply, a revert). A
    // preview carries the generation it started in and is dropped if that has moved on: it
    // describes files that are no longer there.
    let mut repo_gen: u64 = 0;

    // Kick off an initial check shortly after startup.
    let _ = tx.send(Command::CheckNow);

    loop {
        let cmd = tokio::select! {
            _ = ticker.tick() => {
                // Due only when enough WALL-CLOCK time has passed since the last check.
                let due = last_check.map_or(true, |t| {
                    // A clock that went backwards yields Err; treat that as "due" rather
                    // than never checking again.
                    t.elapsed().map(|e| e >= cfg.interval()).unwrap_or(true)
                });
                if !due {
                    continue;
                }
                Command::CheckNow
            }
            _ = sigusr1.recv() => Command::CheckNow,
            // A finished check is handled here rather than as a Command, so it can never
            // block command processing.
            Some((result, release)) = done_rx.recv() => {
                checking = false;
                if applying || check_stale {
                    tracing::info!("discarding a check that overlapped an apply");
                    check_stale = false;
                    if !applying {
                        let _ = tx.send(Command::CheckNow);
                    }
                    continue;
                }
                handle_check_result(result, &cfg, &shared, &tray_handle).await;
                handle_release(release, &cfg, &shared).await;
                continue;
            }
            Some(outcome) = apply_done_rx.recv() => {
                applying = false;
                {
                    let mut s = shared.lock().await;
                    s.apply.state = outcome.state;
                    s.apply.failure = outcome.failure;
                    // Anything but a rolled-back failure changed the repo's lock, which makes
                    // a preview stale (and an upgrade has been done).
                    // The update list and release offer describe the old repo too; the
                    // follow-up check fills them in again.
                    if outcome.state != ApplyState::Failed {
                        // A preview still running keeps showing as such until its (now
                        // stale) result arrives and is dropped; only then can another start.
                        if !previewing {
                            s.upgrade = UpgradeInfo::default();
                        }
                        s.upgrade_plan = None;
                        s.changes.clear();
                        s.candidate_lock = None;
                        s.candidate_base_lock = None;
                        s.candidate_drv = None;
                        s.release = None;
                        s.running_differs = false;
                        s.can_revert_lock = false;
                        repo_gen += 1;
                    }
                }
                // A failure has already set Error. After a success the follow-up check
                // owns the status, but it is dropped while a stale check is still running,
                // which would leave the tray saying "applying" until that one ends.
                if checking && outcome.state == ApplyState::Succeeded {
                    set_status(&shared, &tray_handle, Status::Checking).await;
                }
                // Re-check so the tray reflects the new baseline.
                let _ = tx.send(Command::CheckNow);
                continue;
            }
            Some((gen, target, result)) = preview_rx.recv() => {
                previewing = false;
                let mut s = shared.lock().await;
                if gen != repo_gen {
                    tracing::info!("discarding an upgrade preview made from replaced flake files");
                    if s.upgrade.state == UpgradeState::Previewing {
                        s.upgrade = UpgradeInfo::default();
                    }
                    continue;
                }
                match result {
                    Ok(plan) => {
                        s.upgrade = UpgradeInfo {
                            state: UpgradeState::Ready,
                            target: target.to_string(),
                            retargeted: plan.retargeted.clone(),
                            changes: plan.changes.clone(),
                            failure: None,
                        };
                        s.upgrade_plan = Some(plan);
                    }
                    Err(e) => {
                        tracing::error!("upgrade preview failed: {e:#}");
                        s.upgrade = UpgradeInfo {
                            state: UpgradeState::Failed,
                            target: target.to_string(),
                            failure: Some(Failure::from_error(&e)),
                            ..UpgradeInfo::default()
                        };
                        s.upgrade_plan = None;
                    }
                }
                continue;
            }
            maybe = rx.recv() => match maybe {
                Some(c) => c,
                None => break,
            },
        };

        match cmd {
            Command::CheckNow => {
                // Collapse overlapping requests: a timer tick or a second click while a
                // check is already in flight is a no-op rather than a queued duplicate.
                if checking {
                    tracing::debug!("check already in progress; ignoring request");
                    continue;
                }
                // The tray keeps saying "applying"; the apply's end triggers a check.
                if applying {
                    tracing::info!("an apply is in progress; checking once it finishes");
                    continue;
                }

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

                checking = true;
                last_check = Some(SystemTime::now());
                let cfg_for_check = cfg.clone();
                let done_tx = done_tx.clone();
                tokio::spawn(async move {
                    let outcome = check::run(&cfg_for_check).await;
                    let release = release::detect(&cfg_for_check).await;
                    let _ = done_tx.send((outcome, release));
                });
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

            Command::ShowProgress => {
                if let Err(e) = spawn_gtk(&["progress"]) {
                    tracing::warn!("could not open the progress window: {e:#}");
                }
            }

            Command::Apply | Command::Upgrade => {
                if applying {
                    tracing::debug!("an apply is already running; ignoring request");
                    continue;
                }
                let plan = {
                    let s = shared.lock().await;
                    if matches!(cmd, Command::Upgrade) {
                        s.upgrade_plan.clone().map(ApplyPlan::Upgrade)
                    } else if let (Some(lock), Some(base)) =
                        (s.candidate_lock.clone(), s.candidate_base_lock.clone())
                    {
                        Some(ApplyPlan::Update { lock, base })
                    } else if s.candidate_drv.is_none() && (s.apply_incomplete || s.running_differs)
                    {
                        // Only with no update set pending: a listed update whose lock could
                        // not be kept must not quietly turn into a rebuild of the old lock.
                        // Rebuild the repo's existing lock: after an unfinished switch it
                        // already describes the running system, and when it was advanced
                        // without a switch it is exactly what should be built.
                        Some(ApplyPlan::Rebuild)
                    } else {
                        None
                    }
                };
                let Some(plan) = plan else {
                    tracing::debug!("nothing to apply");
                    continue;
                };
                // Spawned, not awaited: a rebuild takes many minutes, and awaiting it here
                // would block this loop exactly as awaiting a check once did — which would
                // make Cancel unreachable for the entire time it is the one thing the user
                // might want.
                applying = true;
                check_stale = checking;
                // The apply stages files in the repo even if it later rolls them back, so a
                // preview evaluating meanwhile may have seen them.
                repo_gen += 1;
                // Empty the log before the new id is visible, so a progress window that
                // switches to the new apply never reads the previous one's output as its own.
                if let Err(e) = std::fs::File::create(nun_core::apply::apply_log_path()) {
                    tracing::debug!("could not reset the apply log: {e}");
                }
                // A stale flag from an earlier run must not abort this one.
                let _ = std::fs::remove_file(nun_core::apply::cancel_flag_path());
                {
                    let mut s = shared.lock().await;
                    // Vouched for against the lock this apply is about to replace.
                    s.can_revert_lock = false;
                    s.apply = ApplyInfo {
                        id: s.apply.id + 1,
                        state: ApplyState::Running,
                        kind: plan.kind(),
                        failure: None,
                    };
                }
                let cfg2 = cfg.clone();
                let shared2 = shared.clone();
                let handle2 = tray_handle.clone();
                let done = apply_done_tx.clone();
                tokio::spawn(async move {
                    let outcome = apply_flow(&cfg2, plan, &shared2, &handle2).await;
                    let _ = done.send(outcome);
                });
            }

            Command::CancelApply => {
                if !applying {
                    tracing::debug!("no apply is running; ignoring cancel");
                    continue;
                }
                // The daemon cannot signal a root process, so drop the flag the
                // privileged side polls. It stops itself, or declines if activation has
                // already begun.
                let flag = nun_core::apply::cancel_flag_path();
                match std::fs::write(&flag, b"") {
                    Ok(()) => tracing::info!("cancellation requested"),
                    Err(e) => tracing::warn!("could not request cancellation: {e}"),
                }
            }

            Command::RevertLock => {
                if applying {
                    continue;
                }
                // Only what the last check vouched for, once: after a revert the newest
                // backup is the lock just replaced, and acting again would undo it.
                if !std::mem::take(&mut shared.lock().await.can_revert_lock) {
                    tracing::info!("no revert is on offer; ignoring");
                    continue;
                }
                let Some(backup) = nun_core::apply::revertable_lock_backup(&cfg.flake_path).await
                else {
                    tracing::info!("no flake.lock backup differs from the current lock");
                    continue;
                };
                // Installed like any candidate, so the lock being replaced is itself backed
                // up first and the revert can be undone.
                match nun_core::apply::install_candidate_lock(&cfg.flake_path, &backup).await {
                    Ok(_) => tracing::info!("reverted flake.lock to {}", backup.display()),
                    Err(e) => tracing::error!("could not revert flake.lock: {e:#}"),
                }
                // The lock moved: a check in flight describes the old one, and a preview
                // was made from it.
                check_stale = checking;
                repo_gen += 1;
                {
                    let mut s = shared.lock().await;
                    if !previewing {
                        s.upgrade = UpgradeInfo::default();
                    }
                    s.upgrade_plan = None;
                }
                let _ = tx.send(Command::CheckNow);
            }

            Command::PreviewUpgrade(target) => {
                if previewing || applying {
                    continue;
                }
                let target: Version = match target.parse() {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("ignoring an upgrade preview for {target:?}: {e}");
                        continue;
                    }
                };
                {
                    let mut s = shared.lock().await;
                    s.upgrade = UpgradeInfo {
                        state: UpgradeState::Previewing,
                        target: target.to_string(),
                        ..UpgradeInfo::default()
                    };
                    s.upgrade_plan = None;
                }
                previewing = true;
                let cfg2 = cfg.clone();
                let preview_tx = preview_tx.clone();
                let gen = repo_gen;
                tokio::spawn(async move {
                    let result = release::preview(&cfg2, target).await;
                    let _ = preview_tx.send((gen, target, result));
                });
            }

            Command::DismissRelease => {
                let mut s = shared.lock().await;
                if let Some(newest) = s.release.as_ref().map(|r| r.newest().to_string()) {
                    s.persisted.dismissed_release = Some(newest);
                    s.persisted.save();
                }
            }

            Command::Dismiss => {
                let mut s = shared.lock().await;
                s.dismissed_drv = s.candidate_drv.clone();
            }

            Command::Quit => break,
        }
    }

    // Leave nothing behind on a clean exit. Stale artifacts are also collected by the next
    // run, but a service that is stopped and not restarted shouldn't leave cache sitting
    // around until something happens to notice.
    check::cleanup_own_artifacts().await;

    Ok(())
}

/// Ask the daemon that owns the bus name to open its updates window.
async fn show_running_instance() -> Result<()> {
    let conn = zbus::Connection::session()
        .await
        .context("connecting to session bus")?;
    conn.call_method(
        Some(nun_core::ipc::BUS_NAME),
        nun_core::ipc::OBJECT_PATH,
        Some(nun_core::ipc::INTERFACE),
        "ShowUpdates",
        &(),
    )
    .await
    .context("asking the running daemon to show its window")?;
    Ok(())
}

/// Fold a finished check into the shared state, the tray, and (if warranted) a
/// notification. Split out of the loop so the check itself can run in its own task.
async fn handle_check_result(
    result: Result<check::CheckOutcome>,
    cfg: &Config,
    shared: &SharedState,
    tray_handle: &ksni::Handle<NixTray>,
) {
    let can_revert = result.as_ref().is_ok_and(|o| o.revert_matches_running);
    // Stamped for every outcome, including a failed check: "we looked and it went wrong"
    // is still information the window should be able to show.
    {
        let mut s = shared.lock().await;
        s.last_check_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs());
        s.can_revert_lock = can_revert;
    }

    match result {
        Ok(outcome) if outcome.updates_available => {
            let count = outcome.count();
            // Distinguish "packages moved" from "the system derivation changed but no
            // package version did" — the latter is a config regeneration and gets a
            // quieter state (no attention highlight, no notification).
            let status = if count == 0 {
                Status::SystemChangesOnly
            } else {
                Status::UpdatesAvailable(count)
            };
            set_status(shared, tray_handle, status).await;

            let (is_new, dismissed) = {
                let s = shared.lock().await;
                (
                    s.last_notified_drv.as_deref() != Some(&outcome.candidate_drv),
                    s.dismissed_drv.as_deref() == Some(&outcome.candidate_drv),
                )
            };

            // A config-only change is deliberately not notified: nothing the user
            // recognises has changed, so a popup would be noise. The tray still shows the
            // quieter state and the update is still applicable from the menu.
            let notified = if cfg.notify && is_new && !dismissed && count > 0 {
                let (title, body) = (
                    format!("{count} NixOS update(s) available"),
                    summarize(&outcome.changes),
                );
                if let Err(e) = notify::notify(&title, &body, &cfg.icons.updates_available).await {
                    tracing::warn!("notification failed: {e:#}");
                }
                true
            } else {
                false
            };

            let mut s = shared.lock().await;
            s.changes = outcome.changes.clone();
            s.candidate_lock = match check::accept_candidate_lock(&outcome.candidate_lock) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!("{e:#}");
                    None
                }
            };
            s.candidate_base_lock = Some(outcome.base_lock.clone());
            s.candidate_drv = Some(outcome.candidate_drv.clone());
            s.failed_inputs = outcome.failed_inputs.clone();
            s.running_differs = outcome.running_differs;
            s.check_failure = None;
            s.notified_failure = None;
            if notified {
                s.last_notified_drv = Some(outcome.candidate_drv.clone());
            }
        }
        Ok(up_to_date) => {
            // Nothing newer upstream — but if the repo itself describes a system other than
            // the running one, "up to date" would be a false all-clear.
            let status = if up_to_date.running_differs {
                Status::NotApplied
            } else {
                Status::Idle
            };
            set_status(shared, tray_handle, status).await;
            let mut s = shared.lock().await;
            s.changes.clear();
            s.candidate_lock = None;
            s.candidate_base_lock = None;
            s.candidate_drv = None;
            s.dismissed_drv = None;
            // Keep these even when up to date: "no updates" means much less if an input
            // was silently skipped, so the client must still be able to say so.
            s.failed_inputs = up_to_date.failed_inputs.clone();
            s.running_differs = up_to_date.running_differs;
            s.check_failure = None;
            s.notified_failure = None;
        }
        Err(e) => {
            tracing::error!("update check failed: {e:#}");
            let failure = Failure::from_error(&e);
            set_status(shared, tray_handle, Status::Error).await;

            // Notify only failures we can name a fix for, and each one once. Those are
            // configuration problems that persist until the user acts (an insecure or
            // unfree package after a bump); a network blip is retried by the next check
            // and is not worth a popup.
            let notify_now = {
                let s = shared.lock().await;
                failure.hint.is_some() && s.notified_failure.as_deref() != Some(failure.headline())
            };
            if cfg.notify && notify_now {
                let body = format!(
                    "{}\n{}",
                    failure.headline(),
                    failure.hint.as_deref().unwrap_or_default()
                );
                let _ = notify::notify("NixOS update check failed", &body, &cfg.icons.error).await;
            }

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
            s.candidate_base_lock = None;
            s.candidate_drv = None;
            s.running_differs = false;
            if notify_now {
                s.notified_failure = Some(failure.headline().to_string());
            }
            s.check_failure = Some(failure);
        }
    }
}

/// Record the release lookup, announcing a newly available release once.
async fn handle_release(release: Result<Option<ReleaseOffer>>, cfg: &Config, shared: &SharedState) {
    let offer = match release {
        Ok(o) => o,
        Err(e) => {
            // A network failure keeps the last known answer rather than hiding the banner.
            tracing::warn!("could not look for new NixOS releases: {e:#}");
            return;
        }
    };
    let mut s = shared.lock().await;
    s.release = offer.clone();
    // A finished preview for a release no longer on offer (upgraded by hand, say) would
    // only lead to an upgrade that is refused.
    let offered = |t: &str| {
        offer
            .as_ref()
            .is_some_and(|o| o.choices.iter().any(|c| c.version == t))
    };
    if s.upgrade.state != UpgradeState::Previewing && !offered(&s.upgrade.target) {
        s.upgrade = UpgradeInfo::default();
        s.upgrade_plan = None;
    }
    let Some(offer) = offer else { return };
    let newest = offer.newest().to_string();
    if s.persisted.notified_release.as_deref() == Some(newest.as_str())
        || s.persisted.dismissed_release.as_deref() == Some(newest.as_str())
    {
        return;
    }
    s.persisted.notified_release = Some(newest.clone());
    s.persisted.save();
    drop(s);

    if cfg.notify {
        let support = if offer.current_eol {
            ", which is no longer supported"
        } else {
            ""
        };
        let body = format!(
            "You are on NixOS {}{support}. Open \"View updates…\" to preview the upgrade; \
             nothing changes until you choose to upgrade.",
            offer.current
        );
        let title = format!("NixOS {newest} is available");
        if let Err(e) = notify::notify(&title, &body, &cfg.icons.updates_available).await {
            tracing::warn!("notification failed: {e:#}");
        }
    }
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

/// Record whether a switch is left unfinished, and mirror it into the tray menu.
async fn set_apply_incomplete(
    shared: &SharedState,
    handle: &ksni::Handle<NixTray>,
    incomplete: bool,
) {
    shared.lock().await.apply_incomplete = incomplete;
    handle
        .update(move |t| t.apply_incomplete = incomplete)
        .await;
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
/// daemon itself.
///
/// Each is listed in both spellings, because under `makeBinaryWrapper` the file in `bin/`
/// is a small compiled launcher that execs a hidden `.<name>-wrapped` sibling — so
/// `/proc/<pid>/exe` reports the *wrapped* path while the path we spawn (or resolve) is the
/// wrapper. They are distinct real files, not symlinks, so `canonicalize()` does not bridge
/// them; listing only one silently rejects our own client.
fn allowed_caller_exes() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for p in [gtk_client_exe(), self_exe().unwrap_or_default()] {
        if let (Some(dir), Some(name)) = (p.parent(), p.file_name().and_then(|n| n.to_str())) {
            // The `.…-wrapped` form of an already-wrapped path simply won't exist and is
            // dropped by the canonicalize filter below.
            candidates.push(dir.join(format!(".{name}-wrapped")));
        }
        candidates.push(p);
    }
    candidates
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
///
/// The child is waited on in its own task: an exited child stays a zombie until its parent
/// reaps it, and the daemon outlives every window it opens.
fn spawn_gtk(args: &[&str]) -> Result<()> {
    let mut child = tokio::process::Command::new(gtk_client_exe())
        .args(args)
        .spawn()
        .context("spawning GTK client")?;
    tokio::spawn(async move {
        match child.wait().await {
            Ok(s) if !s.success() => tracing::warn!("GTK client exited with {s}"),
            Err(e) => tracing::warn!("waiting for GTK client: {e}"),
            _ => {}
        }
    });
    Ok(())
}

/// Report a failure that stopped an apply before the rebuild: notify, show the error state,
/// and return the outcome to record.
async fn abort_apply(
    cfg: &Config,
    shared: &SharedState,
    handle: &ksni::Handle<NixTray>,
    title: &str,
    body: &str,
    failure: Failure,
) -> ApplyOutcome {
    tracing::error!("apply aborted: {}", failure.details);
    if cfg.notify {
        let _ = notify::notify(title, body, &cfg.icons.error).await;
    }
    set_status(shared, handle, Status::Error).await;
    ApplyOutcome {
        state: ApplyState::Failed,
        failure: Some(failure),
    }
}

/// Install what `plan` needs, run the privileged rebuild, and report the outcome.
async fn apply_flow(
    cfg: &Config,
    plan: ApplyPlan,
    shared: &SharedState,
    handle: &ksni::Handle<NixTray>,
) -> ApplyOutcome {
    set_status(shared, handle, Status::Applying).await;

    let exe = match self_exe() {
        Ok(p) => p,
        Err(e) => {
            let f = Failure::from_error(&e);
            return abort_apply(
                cfg,
                shared,
                handle,
                "NixOS update failed",
                &f.step.clone(),
                f,
            )
            .await;
        }
    };

    let (lock, nix_backup) = match &plan {
        ApplyPlan::Rebuild => {
            tracing::info!("rebuilding from the repo's current lock");
            (None, None)
        }
        ApplyPlan::Update { lock, base } => {
            if let Err(e) = nun_core::apply::ensure_lock_unchanged(&cfg.flake_path, base).await {
                let f = Failure::from_error(&e);
                return abort_apply(
                    cfg,
                    shared,
                    handle,
                    "NixOS update blocked",
                    &e.to_string(),
                    f,
                )
                .await;
            }
            (Some(lock.clone()), None)
        }
        ApplyPlan::Upgrade(p) => {
            if let Err(e) = release::ensure_repo_unchanged(&cfg.flake_path, p).await {
                let f = Failure::from_error(&e);
                return abort_apply(
                    cfg,
                    shared,
                    handle,
                    "NixOS upgrade blocked",
                    &e.to_string(),
                    f,
                )
                .await;
            }
            match nun_core::apply::install_candidate_flake_nix(
                &cfg.flake_path,
                &p.candidate_flake_nix,
            )
            .await
            {
                Ok(b) => (
                    Some(p.candidate_lock.clone()),
                    Some((b, p.candidate_flake_nix.clone())),
                ),
                Err(e) => {
                    let f = Failure::from_error(&e);
                    return abort_apply(
                        cfg,
                        shared,
                        handle,
                        "NixOS upgrade failed",
                        "Could not write the upgraded flake.nix; nothing was changed.",
                        f,
                    )
                    .await;
                }
            }
        }
    };

    let lock_backup = if let Some(candidate_lock) = &lock {
        // Last-chance safety net before anything is written: verify this candidate does not
        // advance a pinned input. check.rs already restricts `nix flake update` to the
        // configured set, so this only fires if that logic is wrong — which is exactly when
        // you want it, since silently advancing e.g. a rev-pinned kernel input can leave
        // the machine unbootable.
        let staged = match verify_pins(cfg, candidate_lock).await {
            Err(e) => Err(("NixOS update blocked", e.to_string(), e)),
            // Install the candidate lock ourselves, UNPRIVILEGED — it's the user's own
            // file, and keeping root away from user-writable paths is a deliberate security
            // property (see nun_core::apply docs). Root only ever activates the result.
            Ok(()) => nun_core::apply::install_candidate_lock(&cfg.flake_path, candidate_lock)
                .await
                .map_err(|e| {
                    (
                        "NixOS update failed",
                        "Could not stage the new flake.lock; nothing was changed.".to_string(),
                        e,
                    )
                }),
        };
        match staged {
            Ok(b) => Some((b, candidate_lock.clone())),
            Err((title, body, e)) => {
                if let Some((b, installed)) = &nix_backup {
                    nun_core::apply::restore_flake_nix(&cfg.flake_path, b, installed).await;
                }
                let f = Failure::from_error(&e);
                return abort_apply(cfg, shared, handle, title, &body, f).await;
            }
        }
    } else {
        None
    };

    apply_rebuild(cfg, &exe, lock_backup, nix_backup, shared, handle).await
}

/// What the apply log says went wrong: its text (with nix's JSON events folded back into
/// messages), as a [`Failure`].
fn apply_log_failure() -> Failure {
    const TAIL_LINES: usize = 200;
    // nix logs every builder output line as an event, so a source build leaves megabytes;
    // the cause is at the end.
    const TAIL_BYTES: u64 = 1 << 20;
    let text = (|| -> std::io::Result<String> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(nun_core::apply::apply_log_path())?;
        let len = f.metadata()?.len();
        f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    })()
    .unwrap_or_default();
    let mut progress = nun_core::progress::Progress::default();
    let lines: Vec<String> = text.lines().filter_map(|l| progress.feed_line(l)).collect();
    let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    Failure::from_output("applying updates (nixos-rebuild switch)", &tail)
}

/// Run the privileged rebuild and report the outcome. The backups are `(backup, installed
/// candidate)`, `None` when nothing was staged (a rebuild of the current lock), so there is
/// nothing to roll back.
async fn apply_rebuild(
    cfg: &Config,
    exe: &Path,
    lock_backup: Option<(PathBuf, PathBuf)>,
    nix_backup: Option<(PathBuf, PathBuf)>,
    shared: &SharedState,
    handle: &ksni::Handle<NixTray>,
) -> ApplyOutcome {
    // Capture the rebuild's output to a file the GTK client tails, so the user can watch
    // what is happening. Previously it went to the daemon's journal, which meant a long
    // privileged operation ran with no visible indication of progress at all.
    let log_path = nun_core::apply::apply_log_path();
    let log = match std::fs::File::create(&log_path) {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!("could not open apply log {}: {e}", log_path.display());
            None
        }
    };

    // Cleared when the apply was started, so a cancel clicked while files were being staged
    // still stops the rebuild.
    let cancel_flag = nun_core::apply::cancel_flag_path();

    // Show the progress window straight away, before the polkit prompt, so the user can
    // see the operation and reach Cancel.
    if let Err(e) = spawn_gtk(&["progress"]) {
        tracing::warn!("could not open the progress window: {e:#}");
    }

    // pkexec <self> apply-privileged --repo <path> --host <host> --cancel-flag <path>
    // No lock path and no pass-through rebuild args cross the privilege boundary; the
    // cancel flag is only ever READ by the privileged side.
    let mut cmd = tokio::process::Command::new("pkexec");
    cmd.arg(exe)
        .arg("apply-privileged")
        .arg("--repo")
        .arg(&cfg.flake_path)
        .arg("--host")
        .arg(&cfg.host_attr)
        .arg("--cancel-flag")
        .arg(&cancel_flag);
    if let Some(f) = log {
        let err = match f.try_clone() {
            Ok(c) => c,
            Err(_) => std::fs::File::create("/dev/null").expect("open /dev/null"),
        };
        cmd.stdout(std::process::Stdio::from(f))
            .stderr(std::process::Stdio::from(err));
    }

    if cfg.notify {
        let _ = notify::notify(
            "Applying NixOS updates…",
            "Running nixos-rebuild switch (you may be prompted to authenticate).",
            &cfg.icons.checking,
        )
        .await;
    }

    // Sampled before the switch so a failure afterwards can tell "nothing happened" from
    // "the new system is already live but a later step failed".
    let system_before = nun_core::apply::current_system();

    let restore_all = || async {
        if let Some((b, installed)) = &lock_backup {
            nun_core::apply::restore_lock(&cfg.flake_path, b, installed).await;
        }
        if let Some((b, installed)) = &nix_backup {
            nun_core::apply::restore_flake_nix(&cfg.flake_path, b, installed).await;
        }
    };

    match cmd.status().await {
        Ok(status) if status.success() => {
            tracing::info!("apply succeeded");
            // A complete switch clears any unfinished-apply state, including one left by an
            // earlier attempt this retry has just put right.
            set_apply_incomplete(shared, handle, false).await;
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
            ApplyOutcome {
                state: ApplyState::Succeeded,
                failure: None,
            }
        }
        Ok(status) => {
            tracing::error!("apply failed: pkexec exited with {status}");
            let failure = apply_log_failure();

            // A non-zero exit does NOT mean nothing happened. `switch-to-configuration`
            // updates /run/current-system partway through activation, so the new system can
            // already be live when a later step fails — an activation snippet that needs
            // the network, say, run just after the switch restarted NetworkManager.
            //
            // Restoring the lock then would be actively harmful: the repo would describe
            // the OLD system while the NEW one is running, and the user's next rebuild
            // would silently revert them. Only roll back if nothing was activated.
            let activated =
                system_before.is_some() && nun_core::apply::current_system() != system_before;

            let state = if activated {
                tracing::warn!(
                    "rebuild reported failure but the new system is already active; \
                     keeping the new flake.lock so the repo matches the running system"
                );
                // The switch still needs finishing, but there are no pending updates left
                // to re-apply, so nothing would re-enable Apply. Remember it, and the tray
                // offers "Retry unfinished apply" until a switch completes.
                set_apply_incomplete(shared, handle, true).await;
                if cfg.notify {
                    let _ = notify::notify(
                        "NixOS update partly applied",
                        "The new system is active, but part of the switch failed. Fix the \
                         cause (see the apply log), then choose 'Retry unfinished apply' \
                         from the tray.",
                        &cfg.icons.error,
                    )
                    .await;
                }
                ApplyState::PartlyApplied
            } else {
                // Covers a failed build and a cancelled/denied polkit prompt alike: put the
                // previous files back so the repo is left exactly as it was. A rebuild
                // staged nothing, so there is nothing to restore.
                restore_all().await;
                if cfg.notify {
                    let _ = notify::notify(
                        "NixOS update failed",
                        &format!(
                            "{}\nThe previous flake files were restored.",
                            failure.headline()
                        ),
                        &cfg.icons.error,
                    )
                    .await;
                }
                ApplyState::Failed
            };
            set_status(shared, handle, Status::Error).await;
            ApplyOutcome {
                state,
                failure: Some(failure),
            }
        }
        Err(e) => {
            tracing::error!("could not launch pkexec: {e:#}");
            restore_all().await;
            if cfg.notify {
                let _ = notify::notify(
                    "NixOS update failed",
                    "Could not launch the privileged helper (pkexec).",
                    &cfg.icons.error,
                )
                .await;
            }
            set_status(shared, handle, Status::Error).await;
            let e = anyhow::Error::from(e).context("launching the privileged helper (pkexec)");
            ApplyOutcome {
                state: ApplyState::Failed,
                failure: Some(Failure::from_error(&e)),
            }
        }
    }
}
