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
}

impl NixTray {
    pub fn new(icons: Icons, tx: CommandTx) -> Self {
        Self {
            status: Status::Idle,
            icons,
            tx,
        }
    }

    /// Whether there is something to apply — enables the action menu items. Note this is
    /// broader than `needs_attention()`: a config-only change is applicable but does not
    /// light up the tray.
    fn has_updates(&self) -> bool {
        self.status.is_applicable()
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
        vec![
            StandardItem {
                label: "Check now".into(),
                icon_name: "view-refresh".into(),
                activate: Box::new(|t: &mut Self| t.send(Command::CheckNow)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "View updates…".into(),
                icon_name: "dialog-information".into(),
                enabled: updates,
                activate: Box::new(|t: &mut Self| t.send(Command::ViewUpdates)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Apply updates".into(),
                icon_name: "system-software-update".into(),
                enabled: updates,
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
        ]
    }
}
