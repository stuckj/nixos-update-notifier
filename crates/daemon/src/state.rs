//! Tray status, the command channel, and the shared state read by the D-Bus interface.

use nun_core::config::Icons;
use nun_core::diff::PackageChange;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Tray/notifier status, which drives the icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Idle,
    Checking,
    UpdatesAvailable(usize),
    Error,
}

impl Status {
    pub fn icon_name<'a>(&self, icons: &'a Icons) -> &'a str {
        match self {
            Status::Idle => &icons.idle,
            Status::Checking => &icons.checking,
            Status::UpdatesAvailable(_) => &icons.updates_available,
            Status::Error => &icons.error,
        }
    }

    pub fn tooltip(&self) -> String {
        match self {
            Status::Idle => "NixOS: system up to date".to_string(),
            Status::Checking => "NixOS: checking for updates…".to_string(),
            Status::UpdatesAvailable(0) => {
                "NixOS: system changes available (no package version changes)".to_string()
            }
            Status::UpdatesAvailable(n) => format!("NixOS: {n} update(s) available"),
            Status::Error => "NixOS update check failed".to_string(),
        }
    }

    /// Short machine string for the D-Bus `GetStatus` method.
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Checking => "checking",
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
