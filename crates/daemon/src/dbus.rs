//! The daemon's D-Bus interface (`org.nixos.UpdateNotifier1`).
//!
//! This is what makes the daemon a standalone service: the GTK client — launched
//! independently — connects to the session bus and drives the daemon through these
//! methods, rather than the daemon shovelling a one-shot data dump into a spawned
//! process. State changes are picked up by the client via polling (`GetStatus`/
//! `GetUpdates`); a push signal is a possible future optimisation.
//!
//! ## Caller checks, and their honest limits
//!
//! The session bus's default policy lets ANY process running as the same user call these
//! methods. Two of them deserve more than that: `Apply` starts the privileged rebuild
//! flow, and `Dismiss` can silently suppress update notifications indefinitely — a
//! misbehaving app could keep you unaware of pending security updates.
//!
//! So the state-changing methods verify that the caller's executable is one of ours (the
//! GTK client, or the daemon itself), via the peer's PID from the bus and `/proc/<pid>/exe`.
//!
//! Be clear about what that is worth: it is defence in depth, NOT a privilege boundary.
//! An attacker who can already execute code as your user can simply run our GTK binary —
//! or ptrace the daemon — and defeat it. What it does stop is a *confined* caller (e.g. a
//! Flatpak app with session-bus access but no path to our binaries) and ordinary misuse or
//! buggy clients. Same-uid isolation is not something D-Bus can provide.

use crate::state::{Command, CommandTx, SharedState};
use std::path::PathBuf;
use zbus::interface;
use zbus::message::Header;

pub struct Updater {
    pub shared: SharedState,
    pub tx: CommandTx,
    /// Executables permitted to invoke state-changing methods (our GTK client and the
    /// daemon itself), canonicalised at startup.
    pub allowed_exes: Vec<PathBuf>,
}

impl Updater {
    /// Resolve the executable behind a bus name and check it against `allowed_exes`.
    ///
    /// Fails closed on an unrecognised executable, but allows (with a warning) when the
    /// peer simply cannot be identified — e.g. an unreadable `/proc/<pid>/exe` under
    /// hardened `hidepid`. This check is defence in depth and must not break normal use.
    async fn caller_allowed(&self, conn: &zbus::Connection, hdr: &Header<'_>) -> bool {
        let Some(sender) = hdr.sender() else {
            return false;
        };
        let proxy = match zbus::fdo::DBusProxy::new(conn).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("could not reach the bus daemon to check caller: {e}");
                return true;
            }
        };
        let pid = match proxy
            .get_connection_credentials(sender.clone().into())
            .await
        {
            Ok(c) => c.process_id(),
            Err(e) => {
                tracing::warn!("could not read caller credentials: {e}");
                return true;
            }
        };
        let Some(pid) = pid else {
            tracing::warn!("bus did not report a PID for {sender}; allowing");
            return true;
        };

        match std::fs::read_link(format!("/proc/{pid}/exe")).and_then(|p| p.canonicalize()) {
            Ok(exe) => {
                let ok = self.allowed_exes.contains(&exe);
                if !ok {
                    tracing::warn!("rejected D-Bus call from unrecognised executable {exe:?}");
                }
                ok
            }
            Err(e) => {
                tracing::warn!("could not resolve /proc/{pid}/exe ({e}); allowing");
                true
            }
        }
    }
}

#[interface(name = "org.nixos.UpdateNotifier1")]
impl Updater {
    /// Trigger a check now (same as the tray "Check now"). Read-only in effect, so it is
    /// open to any same-user caller.
    async fn check_now(&self) {
        let _ = self.tx.send(Command::CheckNow);
    }

    /// Apply the pending updates (starts the authenticated rebuild). Restricted.
    async fn apply(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        if !self.caller_allowed(conn, &hdr).await {
            return Err(zbus::fdo::Error::AccessDenied(
                "Apply may only be called by the nixos-update-notifier client".into(),
            ));
        }
        let _ = self.tx.send(Command::Apply);
        Ok(())
    }

    /// Suppress re-notification for the current pending set until it changes. Restricted,
    /// so another app cannot quietly keep you unaware of pending updates.
    async fn dismiss(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] hdr: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        if !self.caller_allowed(conn, &hdr).await {
            return Err(zbus::fdo::Error::AccessDenied(
                "Dismiss may only be called by the nixos-update-notifier client".into(),
            ));
        }
        let _ = self.tx.send(Command::Dismiss);
        Ok(())
    }

    /// The pending package changes as a JSON array of `PackageChange`.
    async fn get_updates(&self) -> String {
        let s = self.shared.lock().await;
        serde_json::to_string(&s.changes).unwrap_or_else(|_| "[]".to_string())
    }

    /// Current status string (`idle`/`checking`/`system-changes`/`updates`/`error`) and
    /// the package-update count.
    async fn get_status(&self) -> (String, u32) {
        let s = self.shared.lock().await;
        (s.status.as_str().to_string(), s.status.count())
    }

    /// Inputs skipped on the last check, as a JSON array of `[name, reason]` pairs.
    /// Empty when every configured input advanced.
    async fn get_warnings(&self) -> String {
        let s = self.shared.lock().await;
        serde_json::to_string(&s.failed_inputs).unwrap_or_else(|_| "[]".to_string())
    }
}
