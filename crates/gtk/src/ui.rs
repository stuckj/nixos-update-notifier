//! GTK4 windows for "View updates…" and "Settings".
//!
//! This is a separate binary (`nixos-update-notifier-gtk`). The updates window is a live
//! client of the daemon over D-Bus; the settings window edits the TOML config directly.
//! GTK owns its own main thread here, entirely decoupled from the daemon's tokio loop.

use crate::client::Client;
use anyhow::{Context, Result};
use gtk::prelude::*;
use gtk::{glib, Align, Application, ApplicationWindow, Orientation};
use nun_core::config::Config;
use nun_core::diff::{ChangeKind, PackageChange};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

const APP_ID_UPDATES: &str = "org.nixos.UpdateNotifier.Updates";
const APP_ID_SETTINGS: &str = "org.nixos.UpdateNotifier.Settings";

/// Show the update list, live from the daemon over D-Bus. Blocks until the window closes.
pub fn run_updates_window() -> Result<()> {
    let app = Application::builder()
        .application_id(APP_ID_UPDATES)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_activate(build_updates_window);
    // Don't let GTK parse our process args (clap already did).
    app.run_with_args::<&str>(&[]);
    Ok(())
}

fn build_updates_window(app: &Application) {
    // Connect to the daemon; `None` means it isn't running (handled gracefully below).
    let client: Option<Rc<Client>> = Client::connect().ok().map(Rc::new);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");

    let header = gtk::Label::new(None);
    header.set_halign(Align::Start);
    header.set_margin_start(8);
    header.set_margin_top(8);
    header.set_margin_bottom(4);

    // Rebuild the list from the daemon's current state. Cloneable so we can hand it to
    // several button handlers and a periodic poll.
    let refresh = {
        let list = list.clone();
        let header = header.clone();
        let client = client.clone();
        move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            match &client {
                None => {
                    header.set_text("Daemon not running");
                    let row = gtk::Label::new(Some(
                        "Start the nixos-update-notifier service, then Refresh.",
                    ));
                    row.set_margin_top(16);
                    row.set_margin_bottom(16);
                    list.append(&row);
                }
                Some(c) => match c.updates() {
                    Ok(changes) => {
                        let status = c.status().map(|(s, _)| s).unwrap_or_default();

                        // An empty list is ambiguous on its own, so let the daemon's status
                        // disambiguate. Advancing an input can change the system derivation
                        // without changing any package version (a flake rev bump with no
                        // rebuilt packages) — there IS something to apply, and saying "no
                        // pending updates" here would flatly contradict the tray icon.
                        let (heading, empty_note) = match (status.as_str(), changes.is_empty()) {
                            ("system-changes", _) => (
                                "Configuration changes — no package updates".to_string(),
                                Some(
                                    "Your flake inputs moved, but no package changed version.\n\
                                     This is usually a module regenerating its configuration.\n\
                                     Applying is safe but not urgent.",
                                ),
                            ),
                            ("updates", true) => (
                                "System update available — no package version changes".to_string(),
                                Some(
                                    "Input revisions advanced, but no package versions changed.\n\
                                     Applying will rebuild the system with the new inputs.",
                                ),
                            ),
                            ("checking", true) => (
                                "Checking for updates…".to_string(),
                                Some("The daemon is evaluating your flake."),
                            ),
                            ("error", true) => (
                                "Last check failed".to_string(),
                                Some("See the daemon log; press Check now to retry."),
                            ),
                            (_, true) => (
                                "System is up to date".to_string(),
                                Some("No pending updates."),
                            ),
                            (_, false) => (format!("{} package change(s)", changes.len()), None),
                        };
                        header.set_text(&heading);

                        if let Some(note) = empty_note {
                            let row = gtk::Label::new(Some(note));
                            row.set_margin_top(16);
                            row.set_margin_bottom(16);
                            row.set_justify(gtk::Justification::Center);
                            list.append(&row);
                        }
                        for change in &changes {
                            list.append(&update_row(change));
                        }
                    }
                    Err(e) => {
                        header.set_text("Could not read updates from the daemon");
                        let row = gtk::Label::new(Some(&e.to_string()));
                        list.append(&row);
                    }
                },
            }
        }
    };
    refresh();

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();

    // Buttons: Check now (trigger a daemon check), Apply, Refresh.
    let check_btn = gtk::Button::with_label("Check now");
    {
        let client = client.clone();
        let refresh = refresh.clone();
        check_btn.connect_clicked(move |_| {
            if let Some(c) = &client {
                let _ = c.check_now();
            }
            // Give the daemon a moment to finish, then repaint.
            glib::timeout_add_local_once(Duration::from_millis(1500), refresh.clone());
        });
    }

    let apply_btn = gtk::Button::with_label("Apply updates");
    apply_btn.add_css_class("suggested-action");
    {
        let client = client.clone();
        apply_btn.connect_clicked(move |_| {
            if let Some(c) = &client {
                let _ = c.apply();
            }
        });
    }

    let refresh_btn = gtk::Button::with_label("Refresh");
    {
        let refresh = refresh.clone();
        refresh_btn.connect_clicked(move |_| refresh());
    }

    // Keep the window in sync with daemon state via a light poll.
    {
        let refresh = refresh.clone();
        glib::timeout_add_local(Duration::from_secs(3), move || {
            refresh();
            glib::ControlFlow::Continue
        });
    }

    let buttons = gtk::Box::new(Orientation::Horizontal, 8);
    buttons.set_margin_start(8);
    buttons.set_margin_end(8);
    buttons.set_margin_top(4);
    buttons.set_margin_bottom(8);
    buttons.append(&check_btn);
    buttons.append(&apply_btn);
    let spacer = gtk::Box::new(Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    buttons.append(&spacer);
    buttons.append(&refresh_btn);

    let vbox = gtk::Box::new(Orientation::Vertical, 0);
    vbox.append(&header);
    vbox.append(&scroller);
    vbox.append(&buttons);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("NixOS — Pending updates")
        .default_width(600)
        .default_height(540)
        .child(&vbox)
        .build();
    window.present();
}

fn update_row(change: &PackageChange) -> gtk::Box {
    let row = gtk::Box::new(Orientation::Horizontal, 8);
    row.set_margin_start(8);
    row.set_margin_end(8);
    row.set_margin_top(4);
    row.set_margin_bottom(4);

    let (glyph, css) = match change.kind {
        ChangeKind::Added => ("＋", "success"),
        ChangeKind::Removed => ("－", "error"),
        ChangeKind::Changed => ("↑", "accent"),
    };
    let kind_label = gtk::Label::new(Some(glyph));
    kind_label.add_css_class(css);
    kind_label.set_width_chars(2);

    let name = gtk::Label::new(None);
    name.set_markup(&format!(
        "<b>{}</b>",
        glib::markup_escape_text(&change.name)
    ));
    name.set_halign(Align::Start);
    name.set_hexpand(true);
    name.set_xalign(0.0);

    let versions = gtk::Label::new(Some(&change.render_line_versions()));
    versions.set_halign(Align::End);
    versions.add_css_class("dim-label");

    row.append(&kind_label);
    row.append(&name);
    row.append(&versions);

    if let Some(url) = &change.changelog {
        let link = gtk::LinkButton::builder()
            .uri(url)
            .label("changelog")
            .build();
        row.append(&link);
    }

    row
}

/// Show the settings editor. Blocks until closed.
pub fn run_settings_window(config_path: PathBuf) -> Result<()> {
    let app = Application::builder()
        .application_id(APP_ID_SETTINGS)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_activate(move |app| build_settings_window(app, &config_path));
    app.run_with_args::<&str>(&[]);
    Ok(())
}

fn build_settings_window(app: &Application, config_path: &std::path::Path) {
    // Best-effort load; fall back to a blank template if the file doesn't parse yet.
    let cfg = Config::load(config_path).ok();

    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();

    let entries = SettingsEntries::new(cfg.as_ref());
    for (r, (label, widget)) in entries.rows().into_iter().enumerate() {
        let r = r as i32;
        let l = gtk::Label::new(Some(label));
        l.set_halign(Align::End);
        grid.attach(&l, 0, r, 1, 1);
        grid.attach(&widget, 1, r, 1, 1);
    }

    let status = gtk::Label::new(None);
    status.set_halign(Align::Start);

    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");

    {
        let entries = entries.clone();
        let config_path = config_path.to_path_buf();
        let status = status.clone();
        save.connect_clicked(move |_| {
            match entries.to_config().and_then(|c| {
                let text = toml::to_string_pretty(&c).context("serializing config")?;
                if let Some(parent) = config_path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(&config_path, text)
                    .with_context(|| format!("writing {}", config_path.display()))?;
                Ok::<_, anyhow::Error>(())
            }) {
                Ok(()) => status.set_markup(
                    "<span foreground='green'>Saved. Changes apply on the next check \
                 (restart the service to change the interval).</span>",
                ),
                Err(e) => status.set_markup(&format!(
                    "<span foreground='red'>{}</span>",
                    glib::markup_escape_text(&e.to_string())
                )),
            }
        });
    }

    let buttons = gtk::Box::new(Orientation::Horizontal, 8);
    buttons.set_halign(Align::End);
    buttons.append(&save);

    let vbox = gtk::Box::new(Orientation::Vertical, 12);
    vbox.append(&grid);
    vbox.append(&status);
    vbox.append(&buttons);
    vbox.set_margin_start(4);
    vbox.set_margin_end(4);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("NixOS Update Notifier — Settings")
        .default_width(520)
        .child(&vbox)
        .build();
    window.present();
}

/// Editable widgets backing the settings form.
#[derive(Clone)]
struct SettingsEntries {
    flake_path: gtk::Entry,
    host_attr: gtk::Entry,
    update_inputs: gtk::Entry,
    exclude_inputs: gtk::Entry,
    interval_secs: gtk::Entry,
    notify: gtk::Switch,
    nixpkgs_ref: gtk::Entry,
    // Fields the form doesn't expose but must not clobber on Save.
    preserved_icons: nun_core::config::Icons,
}

impl SettingsEntries {
    fn new(cfg: Option<&Config>) -> Self {
        let entry = |val: String| {
            let e = gtk::Entry::new();
            e.set_text(&val);
            e.set_hexpand(true);
            e
        };
        Self {
            flake_path: entry(
                cfg.map(|c| c.flake_path.display().to_string())
                    .unwrap_or_default(),
            ),
            host_attr: entry(cfg.map(|c| c.host_attr.clone()).unwrap_or_default()),
            update_inputs: entry(cfg.map(|c| c.update_inputs.join(", ")).unwrap_or_default()),
            exclude_inputs: entry(cfg.map(|c| c.exclude_inputs.join(", ")).unwrap_or_default()),
            interval_secs: entry(
                cfg.map(|c| c.interval_secs.to_string())
                    .unwrap_or_else(|| "21600".into()),
            ),
            notify: {
                let s = gtk::Switch::new();
                s.set_active(cfg.map(|c| c.notify).unwrap_or(true));
                s.set_halign(Align::Start);
                s
            },
            nixpkgs_ref: entry(
                cfg.and_then(|c| c.nixpkgs_ref_for_changelogs.clone())
                    .unwrap_or_default(),
            ),
            preserved_icons: cfg.map(|c| c.icons.clone()).unwrap_or_default(),
        }
    }

    fn rows(&self) -> Vec<(&'static str, gtk::Widget)> {
        vec![
            ("Flake path", self.flake_path.clone().upcast()),
            ("Host attribute", self.host_attr.clone().upcast()),
            (
                "Update inputs (comma-sep)",
                self.update_inputs.clone().upcast(),
            ),
            (
                "Exclude inputs (comma-sep)",
                self.exclude_inputs.clone().upcast(),
            ),
            ("Interval (seconds)", self.interval_secs.clone().upcast()),
            ("Notifications", self.notify.clone().upcast()),
            (
                "nixpkgs ref for changelogs",
                self.nixpkgs_ref.clone().upcast(),
            ),
        ]
    }

    fn to_config(&self) -> Result<Config> {
        let split = |e: &gtk::Entry| -> Vec<String> {
            e.text()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let nixpkgs = self.nixpkgs_ref.text().trim().to_string();
        Ok(Config {
            flake_path: PathBuf::from(self.flake_path.text().trim()),
            host_attr: self.host_attr.text().trim().to_string(),
            update_inputs: split(&self.update_inputs),
            exclude_inputs: split(&self.exclude_inputs),
            interval_secs: self
                .interval_secs
                .text()
                .trim()
                .parse()
                .context("interval must be a whole number of seconds")?,
            notify: self.notify.is_active(),
            nixpkgs_ref_for_changelogs: if nixpkgs.is_empty() {
                None
            } else {
                Some(nixpkgs)
            },
            icons: self.preserved_icons.clone(),
        })
    }
}
