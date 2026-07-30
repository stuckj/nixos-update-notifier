//! Desktop notifications via the freedesktop `org.freedesktop.Notifications` D-Bus API.
//!
//! We call `Notify` directly over the session bus with zbus (no `notify-send` dependency,
//! and no extra process spawn).

use anyhow::{Context, Result};
use std::collections::HashMap;
use zbus::zvariant::Value;

const APP_NAME: &str = "NixOS Update Notifier";

/// Fire a notification. `icon` is a freedesktop icon name. Returns the notification id.
/// Errors are non-fatal to the caller (log and continue) — a missing notification daemon
/// should never take down the tray.
pub async fn notify(summary: &str, body: &str, icon: &str) -> Result<u32> {
    let conn = zbus::Connection::session()
        .await
        .context("connecting to session bus for notifications")?;

    let actions: Vec<&str> = Vec::new();
    let mut hints: HashMap<&str, Value> = HashMap::new();
    // Normal urgency (1). 0 = low, 2 = critical.
    hints.insert("urgency", Value::U8(1));

    let reply = conn
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                APP_NAME, 0u32, // replaces_id
                icon, // app_icon
                summary, body, actions, hints, -1i32, // expire_timeout: server default
            ),
        )
        .await
        .context("calling org.freedesktop.Notifications.Notify")?;

    let id: u32 = reply.body().deserialize().unwrap_or(0);
    Ok(id)
}
