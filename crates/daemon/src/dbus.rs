//! The daemon's D-Bus interface (`org.nixos.UpdateNotifier1`).
//!
//! This is what makes the daemon a standalone service: the GTK client — launched
//! independently — connects to the session bus and drives the daemon through these
//! methods, rather than the daemon shovelling a one-shot data dump into a spawned
//! process. State changes are picked up by the client via polling (`GetStatus`/
//! `GetUpdates`); a push signal is a possible future optimisation.

use crate::state::{Command, CommandTx, SharedState};
use zbus::interface;

pub struct Updater {
    pub shared: SharedState,
    pub tx: CommandTx,
}

#[interface(name = "org.nixos.UpdateNotifier1")]
impl Updater {
    /// Trigger a check now (same as the tray "Check now").
    async fn check_now(&self) {
        let _ = self.tx.send(Command::CheckNow);
    }

    /// Apply the pending updates (spawns the authenticated rebuild).
    async fn apply(&self) {
        let _ = self.tx.send(Command::Apply);
    }

    /// Suppress re-notification for the current pending set until it changes.
    async fn dismiss(&self) {
        let _ = self.tx.send(Command::Dismiss);
    }

    /// The pending package changes as a JSON array of `PackageChange`.
    async fn get_updates(&self) -> String {
        let s = self.shared.lock().await;
        serde_json::to_string(&s.changes).unwrap_or_else(|_| "[]".to_string())
    }

    /// Current status string (`idle`/`checking`/`updates`/`error`) and update count.
    async fn get_status(&self) -> (String, u32) {
        let s = self.shared.lock().await;
        (s.status.as_str().to_string(), s.status.count())
    }
}
