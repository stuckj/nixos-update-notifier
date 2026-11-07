//! The StatusNotifierItem tray icon + menu, via `ksni` (native SNI — the only thing that
//! works on KDE Plasma 6 / Wayland; legacy XEmbed is not an option there).
//!
//! Note on the "count badge": SNI has no standard numeric badge. When updates are pending
//! we switch the item to the `NeedsAttention` status (which KDE highlights) and put the
//! count in the tooltip/title. That is the idiomatic SNI way to signal a count.

use crate::state::{Command, CommandTx, Status};
use ksni::menu::{MenuItem, StandardItem};
use nun_core::config::Icons;

pub struct NixTray {
    pub status: Status,
    pub icons: Icons,
    pub tx: CommandTx,
    /// A switch activated but did not finish. Keeps the apply action reachable even with
    /// nothing pending, which is exactly the state that leaves it needing a re-run.
    pub apply_incomplete: bool,
}

impl NixTray {
    pub fn new(icons: Icons, tx: CommandTx) -> Self {
        Self {
            status: Status::Idle,
            icons,
            tx,
            apply_incomplete: false,
        }
    }

    /// Whether there is something to apply — enables the action menu items. Note this is
    /// broader than `needs_attention()`: a config-only change is applicable but does not
    /// light up the tray.
    fn has_updates(&self) -> bool {
        self.status.is_applicable()
    }

    /// Label and enabled state for the apply item.
    ///
    /// An unfinished switch has no pending updates left — the system is already running the
    /// new configuration — so gating purely on "are there updates" would strand the user
    /// with no way to finish it from the tray.
    fn apply_item(&self) -> (String, bool) {
        if self.apply_incomplete {
            ("Retry unfinished apply".to_string(), true)
        } else if self.status == Status::NotApplied {
            ("Apply (rebuild from current flake)".to_string(), true)
        } else {
            ("Apply updates".to_string(), self.has_updates())
        }
    }

    fn send(&self, cmd: Command) {
        // Worker owns the receiver for the whole daemon lifetime; a send error only
        // happens during shutdown, where dropping is fine.
        let _ = self.tx.send(cmd);
    }
}

impl ksni::Tray for NixTray {
    fn id(&self) -> String {
        "org.nixos.UpdateNotifier".into()
    }

    fn title(&self) -> String {
        self.status.tooltip()
    }

    fn icon_name(&self) -> String {
        self.status.icon_name(&self.icons).to_string()
    }

    fn attention_icon_name(&self) -> String {
        self.icons.updates_available.clone()
    }

    fn status(&self) -> ksni::Status {
        // Only real package updates get the attention highlight. A config-only change is
        // applicable but not worth nagging about, so it stays Active.
        if self.status.needs_attention() {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.status.tooltip(),
            description: String::new(),
            icon_name: self.icon_name(),
            icon_pixmap: Vec::new(),
        }
    }

    /// Left-click opens the update list.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Command::ViewUpdates);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let updates = self.has_updates();
        // While an apply runs, its progress window (with Cancel) must stay reachable after
        // it has been closed.
        let progress: Option<MenuItem<Self>> = (self.status == Status::Applying).then(|| {
            StandardItem {
                label: "Show apply progress…".into(),
                icon_name: "system-run".into(),
                activate: Box::new(|t: &mut Self| t.send(Command::ShowProgress)),
                ..Default::default()
            }
            .into()
        });
        let mut items: Vec<MenuItem<Self>> = progress.into_iter().collect();
        items.extend(vec![
            StandardItem {
                label: "Check now".into(),
                icon_name: "view-refresh".into(),
                activate: Box::new(|t: &mut Self| t.send(Command::CheckNow)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                // Always available, even with nothing pending. It is the only window that
                // says what the daemon is doing — when it last checked, whether a check is
                // running, which inputs it had to skip — and that is exactly what you want
                // to look at BEFORE a check has found anything.
                label: "View updates…".into(),
                icon_name: "dialog-information".into(),
                enabled: true,
                activate: Box::new(|t: &mut Self| t.send(Command::ViewUpdates)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: self.apply_item().0,
                icon_name: "system-software-update".into(),
                enabled: self.apply_item().1,
                activate: Box::new(|t: &mut Self| t.send(Command::Apply)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Dismiss until next check".into(),
                icon_name: "window-close".into(),
                enabled: updates,
                activate: Box::new(|t: &mut Self| t.send(Command::Dismiss)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Settings".into(),
                icon_name: "configure".into(),
                activate: Box::new(|t: &mut Self| t.send(Command::Settings)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| t.send(Command::Quit)),
                ..Default::default()
            }
            .into(),
        ]);
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn tray(status: Status, apply_incomplete: bool) -> NixTray {
        let (tx, _rx) = mpsc::unbounded_channel();
        NixTray {
            status,
            icons: Icons::default(),
            tx,
            apply_incomplete,
        }
    }

    #[test]
    fn apply_is_offered_only_when_there_is_something_to_apply() {
        assert_eq!(
            tray(Status::UpdatesAvailable(3), false).apply_item(),
            ("Apply updates".to_string(), true)
        );
        assert_eq!(
            tray(Status::Idle, false).apply_item(),
            ("Apply updates".to_string(), false)
        );
    }

    /// The case that stranded a real user: a switch activated but failed partway, leaving
    /// the system on the new configuration with NO pending updates. Gating on updates alone
    /// would grey out the one action that finishes the job.
    #[test]
    fn an_unfinished_switch_keeps_the_apply_action_reachable() {
        let (label, enabled) = tray(Status::Idle, true).apply_item();
        assert!(enabled, "an unfinished switch must stay retryable");
        assert_eq!(label, "Retry unfinished apply");
    }

    /// A lock advanced without a switch has no pending updates either, yet must be
    /// applicable.
    #[test]
    fn a_lock_ahead_of_the_running_system_can_be_applied() {
        let (label, enabled) = tray(Status::NotApplied, false).apply_item();
        assert!(enabled);
        assert_eq!(label, "Apply (rebuild from current flake)");
    }

    fn labels(t: &NixTray) -> Vec<String> {
        use ksni::Tray;
        t.menu()
            .into_iter()
            .filter_map(|i| match i {
                MenuItem::Standard(s) => Some(s.label),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_progress_window_is_reachable_only_while_applying() {
        let show = "Show apply progress…".to_string();
        assert!(labels(&tray(Status::Applying, false)).contains(&show));
        assert!(!labels(&tray(Status::Idle, false)).contains(&show));
    }
}
