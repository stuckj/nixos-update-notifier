//! Blocking D-Bus client to the running daemon.
//!
//! GTK runs on the glib main loop (single-threaded, synchronous), so we use zbus's
//! *blocking* proxy — method calls are brief request/response round-trips to the daemon.

use anyhow::{Context, Result};
use nun_core::diff::PackageChange;

#[zbus::proxy(
    interface = "org.nixos.UpdateNotifier1",
    default_service = "org.nixos.UpdateNotifier",
    default_path = "/org/nixos/UpdateNotifier"
)]
trait Updater {
    fn check_now(&self) -> zbus::Result<()>;
    fn apply(&self) -> zbus::Result<()>;
    fn dismiss(&self) -> zbus::Result<()>;
    fn get_updates(&self) -> zbus::Result<String>;
    fn get_status(&self) -> zbus::Result<(String, u32)>;
}

pub struct Client {
    proxy: UpdaterProxyBlocking<'static>,
}

impl Client {
    /// Connect to the session bus and bind the daemon proxy. Fails if the daemon isn't
    /// running (no owner of the well-known name).
    pub fn connect() -> Result<Self> {
        let conn = zbus::blocking::Connection::session().context("connecting to session bus")?;
        let proxy = UpdaterProxyBlocking::new(&conn)
            .context("binding to the nixos-update-notifier daemon (is it running?)")?;
        Ok(Self { proxy })
    }

    pub fn updates(&self) -> Result<Vec<PackageChange>> {
        let json = self.proxy.get_updates().context("GetUpdates")?;
        // Surface a parse failure (malformed data / a future protocol change) rather than
        // silently showing an empty list — the updates window renders this error.
        serde_json::from_str(&json).context("parsing updates JSON from the daemon")
    }

    pub fn status(&self) -> Result<(String, u32)> {
        self.proxy.get_status().context("GetStatus")
    }

    pub fn check_now(&self) -> Result<()> {
        self.proxy.check_now().context("CheckNow")
    }

    pub fn apply(&self) -> Result<()> {
        self.proxy.apply().context("Apply")
    }
}
