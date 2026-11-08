//! Tray status, the command channel, and the shared state read by the D-Bus interface.

use nun_core::config::Icons;
use nun_core::diff::PackageChange;
use nun_core::failure::Failure;
use nun_core::ipc::{ApplyInfo, Details, UnappliedDiff, UpgradeInfo};
use nun_core::release::{ReleaseOffer, UpgradePreview};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Tray/notifier status, which drives the icon.
///
/// `SystemChangesOnly` and `UpdatesAvailable` are deliberately separate states. Advancing
/// an input can change the system derivation while changing zero package versions — e.g. a
/// home-manager bump that only regenerates its activation script, units and /etc entries
/// (observed: 10 of 20,042 derivations differing, none of them a package). Badging that as
/// "updates available" over-signals: the user sees an update badge, opens the window, and
/// finds nothing to update. It is still worth applying, just not worth nagging about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Idle,
    Checking,
    /// The system derivation differs but no package version changed.
    SystemChangesOnly,
    /// A privileged rebuild is in progress.
    Applying,
    /// At least one package version changed; `usize` is how many.
    UpdatesAvailable(usize),
    /// Nothing newer upstream, but the repo describes a different system from the running
    /// one: `flake.lock` (or the configuration) moved without a switch.
    NotApplied,
    Error,
}

impl Status {
    pub fn icon_name<'a>(&self, icons: &'a Icons) -> &'a str {
        match self {
            Status::Idle => &icons.idle,
            Status::Checking => &icons.checking,
            Status::SystemChangesOnly => &icons.system_changes,
            Status::Applying => &icons.applying,
            Status::UpdatesAvailable(_) => &icons.updates_available,
            Status::NotApplied => &icons.system_changes,
            Status::Error => &icons.error,
        }
    }

    pub fn tooltip(&self) -> String {
        match self {
            Status::Idle => "NixOS: system up to date".to_string(),
            Status::Checking => "NixOS: checking for updates…".to_string(),
            Status::SystemChangesOnly => {
                "NixOS: system configuration changes available (no package updates)".to_string()
            }
            Status::Applying => "NixOS: applying updates…".to_string(),
            Status::UpdatesAvailable(n) => format!("NixOS: {n} package update(s) available"),
            Status::NotApplied => {
                "NixOS: flake changes not applied to the running system".to_string()
            }
            Status::Error => "NixOS update check failed".to_string(),
        }
    }

    /// Short machine string for the D-Bus `GetStatus` method.
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Checking => "checking",
            Status::SystemChangesOnly => "system-changes",
            Status::Applying => "applying",
            Status::UpdatesAvailable(_) => "updates",
            Status::NotApplied => "not-applied",
            Status::Error => "error",
        }
    }

    pub fn count(&self) -> u32 {
        match self {
            Status::UpdatesAvailable(n) => *n as u32,
            _ => 0,
        }
    }

    /// Whether this state warrants the tray's attention highlight. A config-only change
    /// deliberately does not.
    pub fn needs_attention(&self) -> bool {
        matches!(self, Status::UpdatesAvailable(_))
    }

    /// Whether this state has something the user could apply.
    pub fn is_applicable(&self) -> bool {
        matches!(
            self,
            Status::UpdatesAvailable(_) | Status::SystemChangesOnly | Status::NotApplied
        )
    }
}

/// Commands emitted by the tray menu or the D-Bus interface, consumed by the worker loop.
#[derive(Debug, Clone)]
pub enum Command {
    CheckNow,
    /// Open the "View updates…" window (tray only — spawns the GTK client).
    ViewUpdates,
    Apply,
    /// Ask an in-flight privileged rebuild to stop.
    CancelApply,
    /// Reopen the progress window of a running apply (tray only).
    ShowProgress,
    /// Put the newest `flake.lock` backup back.
    RevertLock,
    Dismiss,
    /// Evaluate and diff an upgrade to this release (`YY.MM`).
    PreviewUpgrade(String),
    /// Apply the previewed release upgrade.
    Upgrade,
    /// Stop offering the newest available release until a newer one appears.
    DismissRelease,
    /// Open the settings window (tray only — spawns the GTK client).
    Settings,
    Quit,
}

pub type CommandTx = mpsc::UnboundedSender<Command>;

/// State shared between the worker (writer) and the D-Bus interface (reader).
#[derive(Debug, Default)]
pub struct Shared {
    pub status: Status,
    /// Pending changes for the current candidate (what `GetUpdates` serialises).
    pub changes: Vec<PackageChange>,
    /// Candidate lock to apply, and the drv that identifies this update set.
    pub candidate_lock: Option<PathBuf>,
    /// The repo lock `candidate_lock` was made from.
    pub candidate_base_lock: Option<String>,
    pub candidate_drv: Option<String>,
    /// Dismiss / re-notify bookkeeping.
    pub dismissed_drv: Option<String>,
    pub last_notified_drv: Option<String>,
    /// Inputs that could not be advanced on the last check, with the reason. Surfaced to
    /// the client so a silently-skipped input (a moved local fork, an unreachable remote)
    /// is visible rather than quietly narrowing what "up to date" means.
    pub failed_inputs: Vec<(String, String)>,
    /// When the last check completed, as seconds since the Unix epoch. `None` until one
    /// finishes, which is what lets the window say "no check has run yet" instead of
    /// claiming the system is up to date on the strength of never having looked.
    pub last_check_epoch: Option<u64>,
    /// Set when an apply activated the new system but the switch did not finish — a late
    /// activation step failing, say. The system is running the new configuration, so there
    /// are no pending updates left to re-apply, yet the switch still needs re-running.
    /// Without this the tool would offer no way back to a consistent state; cleared by the
    /// next successful apply.
    pub apply_incomplete: bool,
    /// Why the last check failed, for the window; cleared by a successful check.
    pub check_failure: Option<Failure>,
    /// The headline of the last check failure that was notified, so it is notified once.
    pub notified_failure: Option<String>,
    /// The repo describes a different system from the running one (see `Status::NotApplied`;
    /// also shown alongside pending updates).
    pub running_differs: bool,
    /// Restoring the newest `flake.lock` backup would make the repo match the running system.
    pub can_revert_lock: bool,
    /// The current or most recent apply.
    pub apply: ApplyInfo,
    /// Newer NixOS releases, from the last check.
    pub release: Option<ReleaseOffer>,
    /// The release upgrade preview the window shows…
    pub upgrade: UpgradeInfo,
    /// …and what applying it installs, once it is ready.
    pub upgrade_plan: Option<UpgradePreview>,
    /// What differs from the running system, in the not-applied state.
    pub unapplied: Option<UnappliedDiff>,
    /// Remembered across restarts so a release is announced once.
    pub persisted: crate::persist::Persisted,
}

impl Shared {
    /// Everything `GetDetails` serves.
    pub fn details(&self) -> Details {
        let release_dismissed = self
            .release
            .as_ref()
            .is_some_and(|r| self.persisted.dismissed_release.as_deref() == Some(r.newest()));
        Details {
            apply: self.apply.clone(),
            check_failure: self.check_failure.clone(),
            running_differs: self.running_differs,
            can_revert_lock: self.can_revert_lock,
            release: self.release.clone(),
            release_dismissed,
            upgrade: self.upgrade.clone(),
            unapplied: self.unapplied.clone(),
        }
    }
}

pub type SharedState = Arc<Mutex<Shared>>;
