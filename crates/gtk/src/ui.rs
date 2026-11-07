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
use nun_core::failure::Failure;
use nun_core::ipc::{ApplyState, Details, UpgradeState};
use nun_core::progress::{act, human_bytes, Progress, RateMeter};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

const APP_ID_UPDATES: &str = "org.nixos.UpdateNotifier.Updates";
const APP_ID_SETTINGS: &str = "org.nixos.UpdateNotifier.Settings";

/// Show the update list, live from the daemon over D-Bus. Blocks until the window closes.
///
/// Single-instance: the tray spawns a fresh process on every click, so without this a
/// second click stacks another identical window on top of the first. Registering under a
/// unique application id makes GTK hand the activation to the already-running instance,
/// which presents its existing window and the new process exits.
pub fn run_updates_window() -> Result<()> {
    let app = Application::builder()
        .application_id(APP_ID_UPDATES)
        .build();

    app.connect_activate(|app| {
        if let Some(existing) = app.active_window() {
            existing.present();
            return;
        }
        build_updates_window(app);
    });
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

    // What the list currently displays. The periodic poll compares against this and
    // returns early when nothing has changed — rebuilding the rows resets the scroll
    // position, which made a long update list impossible to read: scroll down, and a
    // few seconds later you are back at the top.
    type Rendered = (
        String,
        Vec<PackageChange>,
        Vec<(String, String)>,
        Option<u64>,
        Details,
    );
    let rendered: Rc<RefCell<Option<Rendered>>> = Rc::new(RefCell::new(None));
    // The release picked in the upgrade section, kept across repaints of the list.
    let selected_release: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    // Rebuild the list from the daemon's current state. Cloneable so we can hand it to
    // several button handlers and a periodic poll.
    let refresh = {
        let list = list.clone();
        let header = header.clone();
        let client = client.clone();
        let rendered = rendered.clone();
        move || {
            // Cheap poll: fetch first, and only touch the widgets if something moved.
            if let Some(c) = &client {
                let now: Rendered = (
                    c.status().map(|(s, _)| s).unwrap_or_default(),
                    c.updates().unwrap_or_default(),
                    c.warnings().unwrap_or_default(),
                    c.last_check(),
                    c.details(),
                );
                if rendered.borrow().as_ref() == Some(&now) {
                    return;
                }
                *rendered.borrow_mut() = Some(now);
            }

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
                        let details = c.details();

                        // A new release is a separate decision from the package list, so it
                        // gets its own section above it.
                        if let Some(b) = release_box(c, &details, &selected_release) {
                            list.append(&b);
                        }

                        // Why the last apply failed. The tray moves on to a fresh check
                        // straight after, so without this the reason is only in the log.
                        if status != "applying" {
                            if let Some(f) = &details.apply.failure {
                                let title = match details.apply.state {
                                    ApplyState::PartlyApplied => "Last apply only partly completed",
                                    _ => "Last apply failed",
                                };
                                list.append(&failure_box(title, f));
                            }
                        }

                        // Surface skipped inputs prominently: "no updates" (or any count)
                        // means something different if an input could not be advanced at
                        // all, and the user is the only one who can fix a moved local fork.
                        for (name, reason) in c.warnings().unwrap_or_default() {
                            let w = gtk::Label::new(None);
                            w.set_markup(&format!(
                                "<b>⚠ input '{}' was skipped</b>\n<small>{}</small>",
                                glib::markup_escape_text(&name),
                                glib::markup_escape_text(&reason)
                            ));
                            w.set_wrap(true);
                            w.set_xalign(0.0);
                            w.set_margin_start(8);
                            w.set_margin_end(8);
                            w.set_margin_top(8);
                            w.set_margin_bottom(8);
                            w.add_css_class("warning");
                            list.append(&w);
                        }

                        // An empty list is ambiguous on its own, so let the daemon's status
                        // disambiguate. Advancing an input can change the system derivation
                        // without changing any package version (a flake rev bump with no
                        // rebuilt packages) — there IS something to apply, and saying "no
                        // pending updates" here would flatly contradict the tray icon.
                        if details.running_differs && !changes.is_empty() {
                            list.append(&note_label(
                                "<b>Your running system is not built from the current \
                                 flake.lock either.</b>\n<small>The list below shows only what \
                                 the newer inputs change; applying also builds what is already \
                                 in the repo.</small>",
                                "warning",
                            ));
                        }

                        let (heading, empty_note) = match (status.as_str(), changes.is_empty()) {
                            ("applying", _) => {
                                let open = gtk::Button::with_label("Show progress");
                                open.set_halign(Align::Center);
                                open.connect_clicked(|_| open_progress_window());
                                list.append(&open);
                                (
                                    "An apply is in progress".to_string(),
                                    Some("Checks are paused until it finishes."),
                                )
                            }
                            ("not-applied", _) => {
                                if details.can_revert_lock {
                                    let revert = gtk::Button::with_label("Revert flake.lock");
                                    revert.set_tooltip_text(Some(
                                        "Put the previous flake.lock back. The current one is \
                                         backed up first.",
                                    ));
                                    revert.set_halign(Align::Center);
                                    let c = c.clone();
                                    revert.connect_clicked(move |_| {
                                        let _ = c.revert_lock();
                                    });
                                    list.append(&revert);
                                }
                                (
                                    "Your flake is not what is running".to_string(),
                                    Some(
                                        "No newer updates, but your flake describes a different \
                                         system from the one running.\nUsually flake.lock was \
                                         updated without a switch (an apply that did not finish, \
                                         or a hand-run `nix flake update`),\nor configuration \
                                         changes have not been rebuilt yet — including a new \
                                         commit, if your config embeds its git revision.\n\
                                         Apply rebuilds from the repo as it is now.",
                                    ),
                                )
                            }
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
                            ("error", true) => match &details.check_failure {
                                Some(f) => {
                                    list.append(&failure_box("Why it failed", f));
                                    ("Last check failed".to_string(), None)
                                }
                                // An apply failure is shown above; anything else is in the
                                // daemon's log.
                                None if details.apply.failure.is_some() => {
                                    ("Last apply failed".to_string(), None)
                                }
                                None => (
                                    "Last check failed".to_string(),
                                    Some("See the daemon log; press Check now to retry."),
                                ),
                            },
                            // "Up to date" is a claim about a check that happened. Before the
                            // first one completes there is nothing to base it on, and the
                            // window is reachable from the tray at any time — including
                            // seconds after login.
                            (_, true) if c.last_check().is_none() => (
                                "No check has run yet".to_string(),
                                Some(
                                    "Nothing has been checked since the daemon started.\n\
                                     Press Check now to look for updates.",
                                ),
                            ),
                            (_, true) => (
                                "System is up to date".to_string(),
                                Some("No pending updates."),
                            ),
                            (_, false) => {
                                let build_only = changes.iter().filter(|c| !c.runtime).count();
                                let installed = changes.len() - build_only;
                                let h = if build_only > 0 {
                                    format!(
                                        "{installed} update(s) to installed software \
                                         · {build_only} build-time only"
                                    )
                                } else {
                                    format!("{} package change(s)", changes.len())
                                };
                                (h, None)
                            }
                        };
                        header.set_text(&heading);

                        if let Some(note) = empty_note {
                            let row = gtk::Label::new(Some(note));
                            row.set_margin_top(16);
                            row.set_margin_bottom(16);
                            row.set_justify(gtk::Justification::Center);
                            list.append(&row);
                        }
                        // Updates to installed software first — that is what the user came
                        // to read; build-time churn is context, not the headline.
                        let mut ordered: Vec<&PackageChange> = changes.iter().collect();
                        ordered.sort_by_key(|c| (!c.runtime, c.name.clone()));
                        for change in ordered {
                            list.append(&update_row(change));
                        }

                        // Dates what is on screen. Without it, "System is up to date" gives
                        // no way to tell a check from a minute ago from one that last ran
                        // before the machine was suspended for a week.
                        if let Some(secs) = c.last_check() {
                            let foot = gtk::Label::new(Some(&format!(
                                "Last checked {}",
                                relative_time(secs)
                            )));
                            foot.set_xalign(0.0);
                            foot.set_margin_start(8);
                            foot.set_margin_top(12);
                            foot.set_margin_bottom(6);
                            foot.add_css_class("dim-label");
                            list.append(&foot);
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

/// Render an epoch timestamp as a coarse "how long ago", e.g. "3 hours ago".
///
/// Coarse on purpose: the point is to tell a check from minutes ago apart from one that
/// last ran before a week of suspend, not to report seconds. A clock that went backwards
/// (NTP correction, timezone-less epoch skew) says "just now" rather than a negative age.
fn relative_time(epoch_secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let age = now.saturating_sub(epoch_secs);
    match age {
        0..=59 => "just now".to_string(),
        60..=3599 => {
            let m = age / 60;
            format!("{m} minute{} ago", if m == 1 { "" } else { "s" })
        }
        3600..=86_399 => {
            let h = age / 3600;
            format!("{h} hour{} ago", if h == 1 { "" } else { "s" })
        }
        _ => {
            let d = age / 86_400;
            format!("{d} day{} ago", if d == 1 { "" } else { "s" })
        }
    }
}

/// A wrapped, left-aligned label in the list's margins.
fn note_label(markup: &str, css: &str) -> gtk::Label {
    let l = gtk::Label::new(None);
    l.set_markup(markup);
    l.set_wrap(true);
    l.set_xalign(0.0);
    l.set_margin_start(8);
    l.set_margin_end(8);
    l.set_margin_top(8);
    l.set_margin_bottom(8);
    if !css.is_empty() {
        l.add_css_class(css);
    }
    l
}

/// What went wrong, in the order a reader needs it: which step, nix's own error line, a
/// suggested fix when we recognise the failure, and the full output behind "Show details"
/// with a Copy button for pasting into an issue.
fn failure_box(title: &str, f: &Failure) -> gtk::Box {
    let b = gtk::Box::new(Orientation::Vertical, 4);
    b.set_margin_start(8);
    b.set_margin_end(8);
    b.set_margin_top(8);
    b.set_margin_bottom(8);

    let mut markup = format!("<b>{}</b>", glib::markup_escape_text(title));
    if !f.step.is_empty() {
        markup.push_str(&format!(
            "\n<small>While {}:</small>",
            glib::markup_escape_text(&f.step)
        ));
    }
    let head = gtk::Label::new(None);
    head.set_markup(&markup);
    head.set_xalign(0.0);
    head.set_wrap(true);
    head.add_css_class("error");
    b.append(&head);

    if !f.summary.is_empty() {
        let s = gtk::Label::new(Some(&f.summary));
        s.set_xalign(0.0);
        s.set_wrap(true);
        s.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        s.set_selectable(true);
        b.append(&s);
    }
    if let Some(h) = &f.hint {
        let l = gtk::Label::new(None);
        l.set_markup(&format!("<b>Fix:</b> {}", glib::markup_escape_text(h)));
        l.set_xalign(0.0);
        l.set_wrap(true);
        l.set_selectable(true);
        b.append(&l);
    }

    let buffer = gtk::TextBuffer::new(None);
    buffer.set_text(&f.details);
    let view = gtk::TextView::with_buffer(&buffer);
    view.set_editable(false);
    view.set_monospace(true);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    let scroller = gtk::ScrolledWindow::builder()
        .min_content_height(200)
        .child(&view)
        .build();
    let copy = gtk::Button::with_label("Copy");
    copy.set_halign(Align::Start);
    {
        let details = f.details.clone();
        copy.connect_clicked(move |btn| btn.clipboard().set_text(&details));
    }
    let inner = gtk::Box::new(Orientation::Vertical, 4);
    inner.append(&scroller);
    inner.append(&copy);
    let expander = gtk::Expander::new(Some("Show details"));
    expander.set_child(Some(&inner));
    b.append(&expander);
    b
}

/// Open the apply progress window: another instance of this client, which GTK hands to an
/// already-open window if there is one.
fn open_progress_window() {
    match std::env::current_exe() {
        Ok(exe) => {
            if let Ok(mut child) = std::process::Command::new(exe).arg("progress").spawn() {
                // Reaped off the main loop so the window never leaves a zombie behind.
                std::thread::spawn(move || child.wait());
            }
        }
        Err(e) => eprintln!("could not locate own executable: {e}"),
    }
}

/// The release upgrade section: a newer NixOS release, a choice of target, and the
/// preview → upgrade flow. `None` when there is nothing to show.
fn release_box(
    client: &Rc<Client>,
    details: &Details,
    selected: &Rc<RefCell<Option<String>>>,
) -> Option<gtk::Box> {
    let offer = details.release.as_ref();
    let upgrade = &details.upgrade;
    if offer.is_none() && upgrade.state == UpgradeState::None {
        return None;
    }
    let b = gtk::Box::new(Orientation::Vertical, 6);
    b.set_margin_start(8);
    b.set_margin_end(8);
    b.set_margin_top(8);
    b.set_margin_bottom(12);

    if let Some(offer) = offer {
        // Dismissed: a single quiet line, so it can still be reached deliberately.
        let quiet = details.release_dismissed && upgrade.state == UpgradeState::None;
        let support = if offer.current_eol {
            " — <b>your release is no longer supported</b>"
        } else {
            ""
        };
        let head = gtk::Label::new(None);
        head.set_markup(&format!(
            "{}NixOS {} is available{}</span> (you are on {}{support})",
            if quiet {
                "<span>"
            } else {
                "<span size='large' weight='bold'>"
            },
            glib::markup_escape_text(offer.newest()),
            if quiet { " (dismissed)" } else { "" },
            glib::markup_escape_text(&offer.current),
        ));
        head.set_xalign(0.0);
        head.set_wrap(true);
        if quiet {
            head.add_css_class("dim-label");
        }
        b.append(&head);

        if !quiet {
            let note = gtk::Label::new(Some(
                "Upgrading moves your release inputs (nixpkgs, home-manager) to the new \
                 release branch in flake.nix, then shows what would change before anything \
                 is applied. system.stateVersion is not changed, and should not be. \
                 Releases often need configuration changes, so read the release notes first.",
            ));
            note.set_xalign(0.0);
            note.set_wrap(true);
            b.append(&note);
        }

        let row = gtk::Box::new(Orientation::Horizontal, 8);
        let versions: Vec<String> = offer.choices.iter().map(|c| c.version.clone()).collect();
        let labels: Vec<String> = offer
            .choices
            .iter()
            .map(|c| {
                let mut l = format!("NixOS {}", c.version);
                if c.next && offer.choices.len() > 1 {
                    l.push_str(" — next release (one at a time is safest)");
                }
                if c.eol {
                    l.push_str(" — end of life");
                }
                l
            })
            .collect();
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let dropdown = gtk::DropDown::from_strings(&label_refs);
        // Default to the newest, unless the user picked something earlier.
        let pick = selected
            .borrow()
            .as_ref()
            .and_then(|s| versions.iter().position(|v| v == s))
            .unwrap_or(versions.len().saturating_sub(1));
        dropdown.set_selected(pick as u32);
        {
            let selected = selected.clone();
            let versions = versions.clone();
            dropdown.connect_selected_notify(move |d| {
                *selected.borrow_mut() = versions.get(d.selected() as usize).cloned();
            });
        }
        if versions.len() > 1 {
            row.append(&dropdown);
        }

        let preview = gtk::Button::with_label(if versions.len() > 1 {
            "Preview upgrade"
        } else {
            "Preview upgrade…"
        });
        preview.set_sensitive(upgrade.state != UpgradeState::Previewing);
        {
            let client = client.clone();
            let dropdown = dropdown.clone();
            preview.connect_clicked(move |_| {
                if let Some(v) = versions.get(dropdown.selected() as usize) {
                    let _ = client.preview_upgrade(v);
                }
            });
        }
        row.append(&preview);

        let notes = gtk::LinkButton::builder()
            .uri(&offer.notes_url)
            .label("Release notes")
            .build();
        row.append(&notes);

        if !details.release_dismissed {
            let dismiss = gtk::Button::with_label("Dismiss");
            dismiss.set_tooltip_text(Some("Don't mention this release again"));
            let client = client.clone();
            dismiss.connect_clicked(move |_| {
                let _ = client.dismiss_release();
            });
            row.append(&dismiss);
        }
        b.append(&row);
    }

    match upgrade.state {
        UpgradeState::None => {}
        UpgradeState::Previewing => {
            let row = gtk::Box::new(Orientation::Horizontal, 8);
            let spinner = gtk::Spinner::new();
            spinner.start();
            row.append(&spinner);
            row.append(&gtk::Label::new(Some(&format!(
                "Evaluating your system on NixOS {}… (this downloads the new nixpkgs source)",
                upgrade.target
            ))));
            b.append(&row);
        }
        UpgradeState::Failed => {
            if let Some(f) = &upgrade.failure {
                b.append(&failure_box(
                    &format!("Could not prepare the upgrade to NixOS {}", upgrade.target),
                    f,
                ));
            }
        }
        UpgradeState::Ready => {
            let moved: Vec<String> = upgrade
                .retargeted
                .iter()
                .map(|(input, old, new)| format!("{input}: {old} → {new}"))
                .collect();
            let installed = upgrade.changes.iter().filter(|c| c.runtime).count();
            let summary = gtk::Label::new(None);
            summary.set_markup(&format!(
                "<b>Upgrade to NixOS {}</b>: {} package change(s), {installed} to installed \
                 software.\n<small>flake.nix: {}</small>",
                glib::markup_escape_text(&upgrade.target),
                upgrade.changes.len(),
                glib::markup_escape_text(&moved.join(", ")),
            ));
            summary.set_xalign(0.0);
            summary.set_wrap(true);
            b.append(&summary);

            let changes = gtk::ListBox::new();
            changes.set_selection_mode(gtk::SelectionMode::None);
            let mut ordered: Vec<&PackageChange> = upgrade.changes.iter().collect();
            ordered.sort_by_key(|c| (!c.runtime, c.name.clone()));
            for c in ordered {
                changes.append(&update_row(c));
            }
            let scroller = gtk::ScrolledWindow::builder()
                .min_content_height(240)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(&changes)
                .build();
            let expander = gtk::Expander::new(Some("Show changes"));
            expander.set_child(Some(&scroller));
            b.append(&expander);

            let go = gtk::Button::with_label(&format!("Upgrade to NixOS {}…", upgrade.target));
            go.add_css_class("suggested-action");
            go.set_halign(Align::Start);
            go.set_tooltip_text(Some(
                "Backs up flake.nix and flake.lock, installs the new ones, and runs \
                 nixos-rebuild switch. Both files are restored if the rebuild fails.",
            ));
            let client = client.clone();
            go.connect_clicked(move |_| {
                let _ = client.upgrade();
            });
            b.append(&go);
        }
    }
    b.append(&gtk::Separator::new(Orientation::Horizontal));
    Some(b)
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

    // Build-time-only entries are dimmed and labelled: they are dependencies used to build
    // something on the system, not software that gets installed, and showing them exactly
    // like real upgrades makes it look like e.g. Go is about to appear on your machine.
    let name = gtk::Label::new(None);
    if change.runtime {
        name.set_markup(&format!(
            "<b>{}</b>",
            glib::markup_escape_text(&change.name)
        ));
    } else {
        name.set_markup(&format!(
            "{} <small>· build-time only</small>",
            glib::markup_escape_text(&change.name)
        ));
        name.add_css_class("dim-label");
        row.set_tooltip_text(Some(
            "Used to build part of your system; not installed on it.",
        ));
    }
    name.set_halign(Align::Start);
    name.set_hexpand(true);
    name.set_xalign(0.0);

    // A package with many distinct versions must not be able to squeeze the name column to
    // nothing (zoom managed a 148-character line), so long values are truncated — but a
    // truncated value you cannot read is its own problem. Clicking the row expands it, the
    // way the Ubuntu updater does.
    let full_versions = change.render_line_versions();
    let versions = gtk::Label::new(Some(&full_versions));
    versions.set_halign(Align::End);
    versions.add_css_class("dim-label");
    versions.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    versions.set_max_width_chars(48);

    // Only advertise the interaction when there is actually something hidden.
    let is_truncated = full_versions.chars().count() > 48;
    if is_truncated {
        row.set_tooltip_text(Some("Click to show all versions"));

        let click = gtk::GestureClick::new();
        {
            let versions = versions.clone();
            let full = full_versions.clone();
            click.connect_released(move |_, _, _, _| {
                let expanded = versions.ellipsize() == gtk::pango::EllipsizeMode::None;
                if expanded {
                    versions.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
                    versions.set_wrap(false);
                    versions.set_tooltip_text(Some("Click to show all versions"));
                } else {
                    versions.set_ellipsize(gtk::pango::EllipsizeMode::None);
                    versions.set_wrap(true);
                    versions.set_tooltip_text(Some(&full));
                }
            });
        }
        row.add_controller(click);
    }

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
///
/// Single-instance for the same reason as the updates window — and more importantly here,
/// since two editors open on the same file would let the second overwrite the first's save.
pub fn run_settings_window(config_path: PathBuf) -> Result<()> {
    let app = Application::builder()
        .application_id(APP_ID_SETTINGS)
        .build();

    app.connect_activate(move |app| {
        if let Some(existing) = app.active_window() {
            existing.present();
            return;
        }
        build_settings_window(app, &config_path);
    });
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
    // Match the form's own 16px margins; without these the button sits flush against the
    // window edge while everything above it is inset.
    buttons.set_margin_end(16);
    buttons.set_margin_bottom(16);
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
    check_releases: gtk::Switch,
    // Fields the form doesn't expose but must not clobber on Save.
    preserved_icons: nun_core::config::Icons,
    preserved_release_inputs: Vec<String>,
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
            check_releases: {
                let s = gtk::Switch::new();
                s.set_active(cfg.map(|c| c.check_releases).unwrap_or(true));
                s.set_halign(Align::Start);
                s
            },
            preserved_icons: cfg.map(|c| c.icons.clone()).unwrap_or_default(),
            preserved_release_inputs: cfg
                .map(|c| c.release_inputs.clone())
                .unwrap_or_else(nun_core::config::default_release_inputs),
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
            (
                "Check for new NixOS releases",
                self.check_releases.clone().upcast(),
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
            check_releases: self.check_releases.is_active(),
            release_inputs: self.preserved_release_inputs.clone(),
            icons: self.preserved_icons.clone(),
        })
    }
}

const APP_ID_PROGRESS: &str = "org.nixos.UpdateNotifier.Progress";

/// Live view of a running apply: a one-line status, an expandable details pane tailing the
/// rebuild's output, and a Cancel button.
///
/// Before this, a privileged rebuild ran for many minutes with no indication of what was
/// happening and no way to stop it — the output went to the daemon's journal, so when an
/// apply failed the reason was a single line the user never saw. (The first real failure
/// was `curl: (22) … error 525` from a vendor download server, buried at line 6881.)
pub fn run_progress_window() -> Result<()> {
    let app = Application::builder()
        .application_id(APP_ID_PROGRESS)
        .build();

    app.connect_activate(|app| {
        if let Some(existing) = app.active_window() {
            existing.present();
            return;
        }
        build_progress_window(app);
    });
    app.run_with_args::<&str>(&[]);
    Ok(())
}

fn build_progress_window(app: &Application) {
    let client: Option<Rc<Client>> = Client::connect().ok().map(Rc::new);

    let heading = gtk::Label::new(Some("Applying updates…"));
    heading.set_halign(Align::Start);
    heading.set_margin_start(12);
    heading.set_margin_top(12);
    heading.add_css_class("title-4");

    let subtitle = gtk::Label::new(Some("Running nixos-rebuild switch."));
    subtitle.set_halign(Align::Start);
    subtitle.set_margin_start(12);
    subtitle.set_margin_bottom(4);
    subtitle.add_css_class("dim-label");

    // Downloads and builds, each with a count line and a bar. Filled from nix's progress
    // events; before any arrive (evaluation, the polkit prompt) the first bar just pulses.
    let progress_label = |text: &str| {
        let l = gtk::Label::new(Some(text));
        l.set_halign(Align::Start);
        l.set_margin_start(12);
        l.set_margin_end(12);
        l.set_wrap(true);
        l.set_xalign(0.0);
        l
    };
    let new_bar = || {
        let b = gtk::ProgressBar::new();
        b.set_margin_start(12);
        b.set_margin_end(12);
        b
    };
    let downloads = progress_label("");
    let bar = new_bar();
    let builds = progress_label("");
    let builds_bar = new_bar();
    let current = progress_label("");
    current.add_css_class("dim-label");
    current.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    current.set_wrap(false);
    current.set_lines(4);
    let stall = progress_label("");
    stall.add_css_class("warning");
    for w in [&downloads, &builds, &current, &stall] {
        w.set_visible(false);
    }
    builds_bar.set_visible(false);

    // The rebuild's output, collapsed by default — the same shape as the Ubuntu updater's
    // details pane.
    let buffer = gtk::TextBuffer::new(None);
    let view = gtk::TextView::with_buffer(&buffer);
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_monospace(true);
    view.set_wrap_mode(gtk::WrapMode::WordChar);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .min_content_height(260)
        .child(&view)
        .build();

    let expander = gtk::Expander::new(Some("Details"));
    expander.set_margin_start(12);
    expander.set_margin_end(12);
    expander.set_child(Some(&scroller));
    expander.set_vexpand(true);

    let cancel = gtk::Button::with_label("Cancel");
    let close = gtk::Button::with_label("Close");
    let buttons = gtk::Box::new(Orientation::Horizontal, 8);
    buttons.set_halign(Align::End);
    buttons.set_margin_end(12);
    buttons.set_margin_bottom(12);
    buttons.set_margin_top(4);
    buttons.append(&cancel);
    buttons.append(&close);

    // Set once Cancel is clicked, so the poll does not offer it again for the same apply.
    let cancel_requested = Rc::new(std::cell::Cell::new(false));
    {
        let client = client.clone();
        let cancel_btn = cancel.clone();
        let subtitle = subtitle.clone();
        let cancel_requested = cancel_requested.clone();
        cancel.connect_clicked(move |_| {
            if let Some(c) = &client {
                let _ = c.cancel_apply();
            }
            cancel_requested.set(true);
            // Cancellation is cooperative and may be declined during activation, so say
            // "requested" rather than claiming it stopped.
            cancel_btn.set_sensitive(false);
            subtitle.set_text(
                "Cancellation requested. Building stops promptly; if activation has already \
                 started it will finish, since stopping midway can leave services mixed.",
            );
        });
    }

    let vbox = gtk::Box::new(Orientation::Vertical, 6);
    vbox.append(&heading);
    vbox.append(&subtitle);
    vbox.append(&downloads);
    vbox.append(&bar);
    vbox.append(&builds);
    vbox.append(&builds_bar);
    vbox.append(&current);
    vbox.append(&stall);
    vbox.append(&expander);
    vbox.append(&buttons);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("NixOS — Applying updates")
        .default_width(720)
        .default_height(200)
        .child(&vbox)
        .build();

    {
        let window = window.clone();
        close.connect_clicked(move |_| window.close());
    }

    // Tail the log and follow the daemon's apply. Reading from a byte offset means the
    // window can be opened, closed and reopened during a long apply without losing output;
    // a reopened window replays the log from the start, which rebuilds the totals.
    let log_path = client.as_ref().and_then(|c| c.apply_log().ok());
    let tail = Rc::new(RefCell::new(LogTail::default()));
    // The apply this window is showing. Status alone cannot say which apply has finished:
    // a window still open from a cancelled apply would read a later apply's progress as
    // its own and report "finished" while the new rebuild is still running.
    let followed: Rc<RefCell<Option<u64>>> = Rc::new(RefCell::new(None));
    let started = std::time::Instant::now();

    {
        let client = client.clone();
        let buffer = buffer.clone();
        let scroller = scroller.clone();
        let heading = heading.clone();
        let subtitle = subtitle.clone();
        let cancel = cancel.clone();

        glib::timeout_add_local(Duration::from_millis(700), move || {
            // A failed call (the daemon restarting, a timeout) is not "no apply": keep what
            // is on screen until the next poll answers.
            let apply = match &client {
                Some(c) => match c.try_details() {
                    Some(d) => d.apply,
                    None => return glib::ControlFlow::Continue,
                },
                None => Default::default(),
            };

            // A newer apply than the one on screen: start over with it.
            if followed.borrow().is_some_and(|id| id != apply.id) || followed.borrow().is_none() {
                if followed.borrow().is_some() {
                    buffer.set_text("");
                    *tail.borrow_mut() = LogTail::default();
                    heading.set_text("Applying updates…");
                    subtitle.set_text("Running nixos-rebuild switch.");
                    cancel_requested.set(false);
                    for w in [&downloads, &builds, &current, &stall] {
                        w.set_visible(false);
                    }
                    builds_bar.set_visible(false);
                    bar.set_visible(true);
                    bar.set_fraction(0.0);
                }
                *followed.borrow_mut() = Some(apply.id);
            }

            if let Some(path) = &log_path {
                let mut t = tail.borrow_mut();
                let (restarted, shown) = t.read(path);
                if restarted {
                    buffer.set_text("");
                }
                if !shown.is_empty() {
                    buffer.insert(&mut buffer.end_iter(), &shown);
                    // Follow the tail, as a terminal would.
                    let adj = scroller.vadjustment();
                    adj.set_value(adj.upper() - adj.page_size());
                }
            }

            let t = tail.borrow();
            let running = apply.state == ApplyState::Running;
            show_progress(
                &t.progress,
                running,
                started.elapsed().as_secs_f64(),
                &mut t.rate.borrow_mut(),
                [&downloads, &builds, &current, &stall],
                [&bar, &builds_bar],
            );

            cancel.set_sensitive(running && !cancel_requested.get());
            if running {
                heading.set_text(&format!("Applying ({})…", apply.kind));
            } else {
                let (h, s) = match apply.state {
                    ApplyState::Succeeded => (
                        "Apply finished".to_string(),
                        "The new system is active.".to_string(),
                    ),
                    ApplyState::Failed => (
                        "Apply failed".to_string(),
                        apply
                            .failure
                            .as_ref()
                            .map(|f| f.headline().to_string())
                            .unwrap_or_default(),
                    ),
                    ApplyState::PartlyApplied => (
                        "Apply partly completed".to_string(),
                        "The new system is active, but part of the switch failed. Retry it \
                         from the tray once the cause is fixed."
                            .to_string(),
                    ),
                    ApplyState::Running | ApplyState::None => {
                        ("No apply is running".to_string(), String::new())
                    }
                };
                heading.set_text(&h);
                subtitle.set_text(&s);
            }
            glib::ControlFlow::Continue
        });
    }

    window.present();
}

/// The apply log as read so far: the byte offset, a partial last line, and the progress
/// folded from nix's events.
#[derive(Default)]
struct LogTail {
    offset: u64,
    partial: String,
    progress: Progress,
    rate: RefCell<RateMeter>,
}

impl LogTail {
    /// Read what has been appended since the last call; returns whether the log started
    /// over (a new apply truncated it), and the text to show.
    fn read(&mut self, path: &std::path::Path) -> (bool, String) {
        let before = self.offset;
        let Some(new) = read_new(path, &mut self.offset) else {
            return (false, String::new());
        };
        // `read_new` rewinds to the start of a truncated file, so everything it returned
        // is the new log.
        let restarted = before > 0 && self.offset == new.len() as u64;
        if restarted {
            *self = LogTail {
                offset: self.offset,
                ..LogTail::default()
            };
        }
        self.partial.push_str(&new);
        let mut shown = String::new();
        // Split off every complete line at once: a reopened window replays a log that can
        // run to many megabytes, and removing lines from the front one by one is quadratic.
        let Some(end) = self.partial.rfind('\n') else {
            return (restarted, shown);
        };
        let complete: String = self.partial.drain(..=end).collect();
        for line in complete.lines() {
            if let Some(text) = self.progress.feed_line(line) {
                shown.push_str(&text);
                shown.push('\n');
            }
        }
        (restarted, shown)
    }
}

/// Render download and build progress. Without nix's events (an older log, or before the
/// build starts) only a pulsing bar shows that something is running.
fn show_progress(
    p: &Progress,
    running: bool,
    now: f64,
    rate: &mut RateMeter,
    labels: [&gtk::Label; 4],
    bars: [&gtk::ProgressBar; 2],
) {
    let [downloads, builds, current, stall] = labels;
    let [dl_bar, build_bar] = bars;

    let paths = p.counts(act::COPY_PATHS);
    let bytes = {
        let dl = p.counts(act::FILE_TRANSFER);
        if dl.expected > 0 {
            dl
        } else {
            p.counts(act::COPY_PATH)
        }
    };
    let built = p.counts(act::BUILDS);

    if !p.structured || (paths.expected == 0 && built.expected == 0) {
        if running {
            dl_bar.pulse();
        } else {
            dl_bar.set_fraction(1.0);
        }
        return;
    }

    rate.record(now, bytes.done);
    if paths.expected > 0 {
        let mut text = format!("Downloads: {}/{} paths", paths.done, paths.expected);
        if bytes.expected > 0 {
            text.push_str(&format!(
                " · {} / {}",
                human_bytes(bytes.done),
                human_bytes(bytes.expected)
            ));
        }
        if let Some(r) = rate.rate().filter(|_| running && paths.running > 0) {
            text.push_str(&format!(" · {}/s", human_bytes(r as u64)));
            let left = bytes.expected.saturating_sub(bytes.done) as f64;
            if r > 0.0 && left > 0.0 {
                text.push_str(&format!(" · about {} left", eta(left / r)));
            }
        }
        downloads.set_text(&text);
        downloads.set_visible(true);
        dl_bar.set_visible(true);
        dl_bar.set_fraction(bytes.fraction().or(paths.fraction()).unwrap_or(0.0));
    } else {
        downloads.set_visible(false);
        dl_bar.set_visible(false);
    }

    if built.expected > 0 {
        let mut text = format!("Builds: {}/{}", built.done, built.expected);
        if built.running > 0 {
            text.push_str(&format!(" ({} running)", built.running));
        }
        if built.failed > 0 {
            text.push_str(&format!(" · {} failed", built.failed));
        }
        builds.set_text(&text);
        builds.set_visible(true);
        build_bar.set_visible(true);
        build_bar.set_fraction(built.fraction().unwrap_or(0.0));
    }

    let now_doing: Vec<String> = p
        .current()
        .iter()
        .take(4)
        .map(|c| {
            let mut s = c.text.clone();
            if let Some(phase) = &c.phase {
                s.push_str(&format!(" ({phase})"));
            }
            if c.expected > 0 {
                s.push_str(&format!(
                    " — {} / {}",
                    human_bytes(c.done),
                    human_bytes(c.expected)
                ));
            }
            s
        })
        .collect();
    current.set_text(&now_doing.join("\n"));
    current.set_visible(running && !now_doing.is_empty());

    // "Slow" and "stuck" look the same in a log. A download that has not moved for a while
    // is worth saying out loud, so the user can tell a network problem from a hung build.
    const STALL_SECS: f64 = 120.0;
    let idle = rate.idle_for(now);
    if running && paths.running > 0 && idle >= STALL_SECS {
        stall.set_text(&format!(
            "No download progress for {}. Check your network connection; cancelling and \
             retrying is safe at this stage.",
            eta(idle)
        ));
        stall.set_visible(true);
    } else {
        stall.set_visible(false);
    }
}

/// A coarse duration: "40 s", "3 min", "1 h 5 min".
fn eta(secs: f64) -> String {
    let s = secs.round() as u64;
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min", s / 60),
        _ => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

/// Read whatever has been appended to `path` since `offset`, advancing it. The bytes are
/// decoded lossily, so a read that ends mid-character garbles that one character.
///
/// A truncated file (a new apply reusing the same path) resets the offset rather than
/// producing garbage.
fn read_new(path: &std::path::Path, offset: &mut u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len < *offset {
        *offset = 0;
    }
    if len == *offset {
        return Some(String::new());
    }
    f.seek(SeekFrom::Start(*offset)).ok()?;
    let mut buf = Vec::with_capacity((len - *offset) as usize);
    f.read_to_end(&mut buf).ok()?;
    *offset = len;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{relative_time, LogTail};

    #[test]
    fn the_log_tail_keeps_partial_lines_and_replays_large_logs_quickly() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("nun-tail-{}.log", std::process::id()));
        let event = r#"@nix {"action":"result","fields":["compiling foo.c"],"id":7,"type":101}"#;
        {
            let mut f = std::fs::File::create(&path).unwrap();
            for _ in 0..50_000 {
                writeln!(f, "{event}").unwrap();
            }
            write!(f, "building the sys").unwrap();
        }
        let mut t = LogTail::default();
        let started = std::time::Instant::now();
        let (_, shown) = t.read(&path);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "replaying ~4 MB took {:?}",
            started.elapsed()
        );
        assert_eq!(shown, "", "build-log events are not shown as text");

        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"tem configuration...\n")
            .unwrap();
        let (_, shown) = t.read(&path);
        assert_eq!(shown, "building the system configuration...\n");
        let _ = std::fs::remove_file(path);
    }

    fn ago(secs: u64) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        relative_time(now - secs)
    }

    #[test]
    fn ages_are_described_in_the_largest_sensible_unit() {
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(60), "1 minute ago");
        assert_eq!(ago(3 * 60), "3 minutes ago");
        assert_eq!(ago(3600), "1 hour ago");
        assert_eq!(ago(5 * 3600), "5 hours ago");
        assert_eq!(ago(86_400), "1 day ago");
        assert_eq!(ago(9 * 86_400), "9 days ago");
    }

    /// A timestamp in the future (clock stepped backwards by NTP) must not underflow into
    /// a nonsensical age — saturating_sub keeps it at "just now".
    #[test]
    fn a_future_timestamp_reads_as_just_now() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(relative_time(now + 10_000), "just now");
    }
}
