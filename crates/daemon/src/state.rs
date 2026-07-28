//! Tray status, the command channel, and the shared state read by the D-Bus interface.

use nun_core::config::Icons;
use nun_core::diff::PackageChange;
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
    /// At least one package version changed; `usize` is how many.
    UpdatesAvailable(usize),
    Error,
}

impl Status {
    pub fn icon_name<'a>(&self, icons: &'a Icons) -> &'a str {
        match self {
            Status::Idle => &icons.idle,
            Status::Checking => &icons.checking,
            Status::SystemChangesOnly => &icons.system_changes,
            Status::UpdatesAvailable(_) => &icons.updates_available,
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
            Status::UpdatesAvailable(n) => format!("NixOS: {n} package update(s) available"),
            Status::Error => "NixOS update check failed".to_string(),
        }
    }

    /// Short machine string for the D-Bus `GetStatus` method.
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Checking => "checking",
            Status::SystemChangesOnly => "system-changes",
            Status::UpdatesAvailable(_) => "updates",
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
            Status::UpdatesAvailable(_) | Status::SystemChangesOnly
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
    Dismiss,
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
    pub candidate_drv: Option<String>,
    /// Dismiss / re-notify bookkeeping.
    pub dismissed_drv: Option<String>,
    pub last_notified_drv: Option<String>,
}

pub type SharedState = Arc<Mutex<Shared>>;
