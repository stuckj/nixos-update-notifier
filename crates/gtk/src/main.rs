//! nixos-update-notifier-gtk — the GTK client.
//!
//! Launched by the daemon (or by the user) to show the update list / settings. It talks
//! to the running daemon over D-Bus; it holds no update logic of its own.

mod client;
#[cfg(feature = "replay")]
mod replay;
mod ui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use nun_core::config::Config;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "nixos-update-notifier-gtk", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the pending-updates window (default).
    Updates,
    /// Show live progress of a running apply.
    Progress,
    /// Show the settings editor.
    Settings {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Replay a recorded apply log in the progress window, without the daemon.
    #[cfg(feature = "replay")]
    #[command(hide = true)]
    Replay {
        /// An apply log, e.g. ~/.cache/nixos-update-notifier/apply.log.
        log: PathBuf,
        /// Log lines fed each poll (every 0.7 s).
        #[arg(long, default_value_t = 800)]
        lines_per_poll: usize,
        /// Stop after this many lines and hold the window there.
        #[arg(long)]
        until: Option<usize>,
    },
}

fn main() -> Result<()> {
    // GTK owns the main OS thread; no tokio here.
    match Cli::parse().command.unwrap_or(Cmd::Updates) {
        Cmd::Updates => ui::run_updates_window(),
        Cmd::Progress => ui::run_progress_window(),
        Cmd::Settings { config } => {
            let path = Config::resolve_path(config)?;
            ui::run_settings_window(path)
        }
        #[cfg(feature = "replay")]
        Cmd::Replay {
            log,
            lines_per_poll,
            until,
        } => replay::run(&log, lines_per_poll, until),
    }
}
