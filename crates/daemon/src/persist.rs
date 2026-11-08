//! The little daemon state that must survive a restart: which NixOS release has already
//! been announced, and which one the user dismissed. Without it every login would
//! announce the same release again.
//!
//! Kept in `$XDG_STATE_HOME/nixos-update-notifier/state.json`. Losing the file costs at
//! most one repeated notification, so read and write errors are logged and ignored.

use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Persisted {
    pub notified_release: Option<String>,
    pub dismissed_release: Option<String>,
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("nixos-update-notifier").join("state.json"))
}

impl Persisted {
    pub fn load() -> Self {
        let Some(p) = path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&p) else {
            return Self::default();
        };
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Self {
            notified_release: s("notified_release"),
            dismissed_release: s("dismissed_release"),
        }
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let v = json!({
            "notified_release": self.notified_release,
            "dismissed_release": self.dismissed_release,
        });
        if let Err(e) = std::fs::write(&p, v.to_string()) {
            tracing::warn!("could not save {}: {e}", p.display());
        }
    }
}
