//! nixos-update-notifier-gtk — the GTK client.
//!
//! Launched by the daemon (or by the user) to show the update list / settings. It talks
//! to the running daemon over D-Bus; it holds no update logic of its own.

mod client;
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
    }
}
