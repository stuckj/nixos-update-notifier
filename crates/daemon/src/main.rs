//! nixos-update-notifier — the tray daemon + headless CLI for flake-based NixOS updates.
//!
//! Subcommands:
//!   (default) / run   Run the tray daemon (also exports the D-Bus service).
//!   check             One-shot check; prints the diff. Downloads nothing (unless --exact).
//!   apply-privileged  (internal) Root-side apply, invoked via pkexec by the daemon.
//!
//! The GTK windows live in a separate binary (`nixos-update-notifier-gtk`) that talks to
//! the running daemon over D-Bus.

mod daemon;
mod dbus;
mod notify;
mod state;
mod tray;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nun_core::config::Config;
use nun_core::{apply, changelog, check, diff};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "nixos-update-notifier", version, about)]
struct Cli {
    /// Path to config.toml (defaults to $XDG_CONFIG_HOME/nixos-update-notifier/config.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the tray daemon (default).
    Run,
    /// Run a single check and print the result (downloads nothing).
    Check {
        /// Emit the changes as JSON instead of text.
        #[arg(long)]
        json: bool,
        /// Compute the pristine runtime diff by BUILDING the candidate (this DOWNLOADS
        /// substitutes). Off by default; the default check downloads nothing.
        #[arg(long)]
        exact: bool,
    },
    /// (internal) Privileged rebuild, invoked via pkexec — do not call directly.
    ///
    /// Takes only the flake ref to activate: the candidate lock is installed beforehand by
    /// the unprivileged daemon, and no pass-through arguments reach root's `nixos-rebuild`.
    #[command(hide = true)]
    ApplyPrivileged {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        host: String,
    },
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("nixos_update_notifier=info,nun_core=info,warn"));
    fmt().with_env_filter(filter).with_target(false).init();
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();

    let config_path = Config::resolve_path(cli.config.clone())?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    match cli.command.unwrap_or(Cmd::Run) {
        Cmd::Run => rt.block_on(daemon::run(config_path)),
        Cmd::Check { json, exact } => rt.block_on(run_check_cli(&config_path, json, exact)),
        Cmd::ApplyPrivileged { repo, host } => rt.block_on(async move {
            let reboot = apply::rebuild_privileged(&repo, &host).await?;
            if reboot.0 {
                println!("REBOOT_RECOMMENDED");
            }
            Ok(())
        }),
    }
}

async fn run_check_cli(config_path: &std::path::Path, json: bool, exact: bool) -> Result<()> {
    let cfg = Config::load(config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    let mut outcome = check::run(&cfg).await?;

    if exact && outcome.updates_available {
        let mut exact_changes = diff::diff_exact(&outcome.candidate_drv).await?;
        changelog::enrich(&cfg, &mut exact_changes).await;
        outcome.changes = exact_changes;
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&outcome.changes)?);
        return Ok(());
    }

    println!("Advanced inputs: {}", outcome.advanced_inputs.join(", "));
    if !outcome.updates_available {
        println!("System is up to date — no changes from advancing those inputs.");
        return Ok(());
    }

    let note = if exact {
        "exact runtime diff (candidate was built)"
    } else {
        "this check downloaded nothing"
    };
    println!("{} package change(s) ({note}):\n", outcome.changes.len());
    for c in &outcome.changes {
        match &c.changelog {
            Some(url) => println!("  {}   [changelog: {}]", c.render_line(), url),
            None => println!("  {}", c.render_line()),
        }
    }
    Ok(())
}
