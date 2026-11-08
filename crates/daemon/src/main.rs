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
mod persist;
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
        /// Path polled to allow the user to cancel; only ever read, never written.
        #[arg(long)]
        cancel_flag: Option<PathBuf>,
    },
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("nixos_update_notifier=info,nun_core=info,warn"));
    // stderr, not fmt's default stdout: `check --json` output must be JSON alone.
    let stderr = std::io::stderr();
    let ansi = std::io::IsTerminal::is_terminal(&stderr);
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_ansi(ansi)
        .init();
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
        Cmd::ApplyPrivileged {
            repo,
            host,
            cancel_flag,
        } => {
            let result = rt.block_on(apply::rebuild_privileged(
                &repo,
                &host,
                cancel_flag.as_deref(),
            ));
            match result {
                Ok(reboot) => {
                    if reboot.0 {
                        println!("REBOOT_RECOMMENDED");
                    }
                    Ok(())
                }
                Err(e) => match e.downcast_ref::<apply::RebuildFailed>() {
                    Some(failed) => {
                        // Not `Error: …`: the failure summary reads that as the cause.
                        eprintln!("{failed}");
                        std::process::exit(1);
                    }
                    None => Err(e),
                },
            }
        }
    }
}

async fn run_check_cli(config_path: &std::path::Path, json: bool, exact: bool) -> Result<()> {
    let cfg = Config::load(config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    let mut outcome = check::run(&cfg).await?;

    if exact && outcome.updates_available {
        let mut exact_changes = diff::diff_exact(&outcome.candidate_drv).await?;
        // Same candidate lock the check just produced, so the exact diff resolves
        // changelogs against the nixpkgs this update would install.
        changelog::enrich(&cfg, &outcome.candidate_lock, &mut exact_changes).await;
        outcome.changes = exact_changes;
    }

    if json {
        // What applying would change: the pending updates, or with none, what the repo
        // would change on the running system.
        let changes = match &outcome.unapplied {
            Some(u) if !outcome.updates_available => &u.changes,
            _ => &outcome.changes,
        };
        println!("{}", serde_json::to_string_pretty(changes)?);
        return Ok(());
    }

    println!("Advanced inputs: {}", outcome.advanced_inputs.join(", "));
    if !outcome.failed_inputs.is_empty() {
        println!("\nWARNING — these inputs could not be advanced and were skipped:");
        for (name, reason) in &outcome.failed_inputs {
            println!("  {name}: {reason}");
        }
        println!("The results below reflect only the inputs that did advance.\n");
    }
    if outcome.running_differs {
        println!(
            "NOTE — the running system is not built from the repo as it is now (flake.lock \
             or configuration changed without a switch)."
        );
    }
    if !outcome.updates_available {
        if outcome.running_differs {
            println!("No newer updates, but the repo has not been applied to this system.");
            for line in not_applied_lines(outcome.unapplied.as_ref()) {
                println!("{line}");
            }
        } else {
            println!("System is up to date — no changes from advancing those inputs.");
        }
        return Ok(());
    }

    print_changes(&outcome.changes, exact);
    Ok(())
}

/// What applying the repo would change on the running system, for `check`.
fn not_applied_lines(unapplied: Option<&nun_core::ipc::UnappliedDiff>) -> Vec<String> {
    let Some(u) = unapplied else {
        return Vec::new();
    };
    if let Some(e) = &u.error {
        return vec![format!("Could not list what differs: {e}")];
    }
    if u.changes.is_empty() {
        return vec!["No package changes; only configuration differs.".into()];
    }
    let mut out = vec![format!(
        "Applying would change {} package(s) (this check downloaded nothing):\n",
        u.changes.len()
    )];
    out.extend(u.changes.iter().map(change_line));
    out
}

fn change_line(c: &diff::PackageChange) -> String {
    match &c.changelog {
        Some(url) => format!("  {}   [changelog: {}]", c.render_line(), url),
        None => format!("  {}", c.render_line()),
    }
}

fn print_changes(changes: &[diff::PackageChange], exact: bool) {
    let note = if exact {
        "exact runtime diff (candidate was built)"
    } else {
        "this check downloaded nothing"
    };
    println!("{} package change(s) ({note}):\n", changes.len());
    for c in changes {
        println!("{}", change_line(c));
    }
}

#[cfg(test)]
mod tests {
    use super::not_applied_lines;
    use nun_core::diff::{ChangeKind, PackageChange};
    use nun_core::ipc::UnappliedDiff;

    fn added(name: &str, v: &str) -> PackageChange {
        PackageChange {
            name: name.into(),
            old: vec![],
            new: vec![v.into()],
            kind: ChangeKind::Added,
            size_delta: None,
            changelog: None,
            runtime: true,
        }
    }

    #[test]
    fn not_applied_output_lists_what_would_change() {
        let u = UnappliedDiff {
            changes: vec![added("wakeful", "0.1.0")],
            error: None,
        };
        let lines = not_applied_lines(Some(&u));
        assert!(lines[0].starts_with("Applying would change 1 package(s)"));
        assert_eq!(lines[1], "  wakeful: (new) -> 0.1.0");
    }

    #[test]
    fn not_applied_output_says_when_only_configuration_differs() {
        let u = UnappliedDiff::default();
        assert_eq!(
            not_applied_lines(Some(&u)),
            vec!["No package changes; only configuration differs."]
        );
        let failed = UnappliedDiff {
            changes: vec![],
            error: Some("no deriver".into()),
        };
        assert_eq!(
            not_applied_lines(Some(&failed)),
            vec!["Could not list what differs: no deriver"]
        );
        assert!(not_applied_lines(None).is_empty());
    }
}
