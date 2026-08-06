//! Configuration: TOML file, loaded from `$XDG_CONFIG_HOME/nixos-update-notifier/config.toml`
//! by default (overridable with `--config`).
//!
//! Everything the tool does is driven from here — nothing is hardcoded to a particular
//! machine. See `config.example.toml` in the repo for a documented template.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default check cadence if the user does not set one.
const DEFAULT_INTERVAL_SECS: u64 = 6 * 60 * 60; // 6 hours

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Absolute path to the flake repository (the directory containing `flake.nix`).
    pub flake_path: PathBuf,

    /// The `nixosConfigurations.<name>` attribute to evaluate/rebuild, e.g. `nixos-x1`.
    pub host_attr: String,

    /// Inputs to advance during a check/apply. If empty, *all* inputs are candidates
    /// EXCEPT those in `exclude_inputs`. If non-empty, ONLY these inputs are advanced
    /// (still minus anything also present in `exclude_inputs`).
    #[serde(default)]
    pub update_inputs: Vec<String>,

    /// Inputs that must NEVER be advanced (e.g. a rev-pinned `nixpkgs-kernel` that
    /// provides the kernel + ZFS). Always skipped, even if listed in `update_inputs`.
    #[serde(default)]
    pub exclude_inputs: Vec<String>,

    /// Background check cadence, in seconds.
    #[serde(default = "default_interval_secs", rename = "interval")]
    pub interval_secs: u64,

    /// Whether to fire desktop notifications when updates appear.
    #[serde(default = "default_true")]
    pub notify: bool,

    /// Which nixpkgs to resolve `meta.changelog` attributes against when rendering the
    /// update list. If unset, the `nixpkgs` from the candidate lock is used — the revision
    /// the pending update would install, already fetched, so nothing is downloaded to read
    /// it. Set this only if the flake names its nixpkgs something other than `nixpkgs`.
    ///
    /// Pin it to a revision: an unpinned ref (the registry alias `nixpkgs`, or a branch)
    /// downloads on every check and reports whatever it points at today rather than the
    /// version being offered. It must also be nixpkgs-shaped — attributes are read from
    /// `legacyPackages.<system>`, for the system this machine evaluates as, which is not
    /// necessarily the system a cross-built or remote host attribute targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nixpkgs_ref_for_changelogs: Option<String>,

    // NOTE: there is deliberately no `rebuild_extra_args` option. Arbitrary arguments
    // forwarded into root's `nixos-rebuild` (`--override-input`, `-I`, `--substituters`, …)
    // would let anything that can write this file change what root evaluates and builds —
    // materially more than "rebuild my machine", and invisible on the polkit prompt. If a
    // specific option is ever needed, add it as a typed, vetted field rather than a
    // free-form pass-through. `deny_unknown_fields` means an old config carrying the key
    // fails loudly instead of silently ignoring it.
    /// Icon theme names for each tray state. Freedesktop icon names are resolved by the
    /// active KDE/Plasma icon theme; override here if your theme lacks them.
    #[serde(default)]
    pub icons: Icons,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Icons {
    pub idle: String,
    pub checking: String,
    /// A privileged rebuild is running — distinct from `checking`, which is the
    /// read-only look for updates.
    pub applying: String,
    /// System derivation changed but no package versions did — a deliberately low-key
    /// state, so it uses a quieter icon than `updates_available`.
    pub system_changes: String,
    pub updates_available: String,
    pub error: String,
}

impl Default for Icons {
    fn default() -> Self {
        // Names verified to exist in Breeze (KDE's default theme) — this is the
        // `update-*` status family KDE's own updater uses, plus `view-refresh` and
        // `dialog-error` from the standard action/status sets.
        //
        // Do not be tempted by plausible-sounding freedesktop names: `nix-snowflake`
        // (needs the nixos-icons package), `emblem-synchronizing` and
        // `software-update-available` are NOT in Breeze, and an unresolvable name makes
        // the tray item render as a blank gap with no error anywhere.
        //
        // On a non-KDE desktop these may not resolve either; override them in `[icons]`.
        Self {
            idle: "update-none".into(),
            checking: "view-refresh".into(),
            applying: "system-software-update".into(),
            system_changes: "update-low".into(),
            updates_available: "update-medium".into(),
            error: "dialog-error".into(),
        }
    }
}

fn default_interval_secs() -> u64 {
    DEFAULT_INTERVAL_SECS
}
fn default_true() -> bool {
    true
}

impl Config {
    /// Resolve the config path: explicit `--config`, else the XDG default.
    pub fn resolve_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
        if let Some(p) = explicit {
            return Ok(p);
        }
        let dirs = ProjectDirs::from("org", "nixos", "nixos-update-notifier")
            .context("could not determine XDG config directory")?;
        Ok(dirs.config_dir().join("config.toml"))
    }

    /// Load and validate a config from a TOML file.
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.flake_path.is_absolute(),
            "flake_path must be an absolute path (got {})",
            self.flake_path.display()
        );
        anyhow::ensure!(!self.host_attr.is_empty(), "host_attr must not be empty");
        anyhow::ensure!(
            self.interval_secs >= 60,
            "interval must be at least 60 seconds"
        );
        Ok(())
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs)
    }

    /// Compute the effective set of input names to advance, given the inputs that
    /// actually exist in the flake (`available`). Honors `update_inputs` (allow-list)
    /// and always subtracts `exclude_inputs`.
    pub fn effective_update_inputs(&self, available: &[String]) -> Vec<String> {
        let excluded: std::collections::HashSet<&str> =
            self.exclude_inputs.iter().map(String::as_str).collect();

        let candidates: Vec<&str> = if self.update_inputs.is_empty() {
            available.iter().map(String::as_str).collect()
        } else {
            self.update_inputs.iter().map(String::as_str).collect()
        };

        candidates
            .into_iter()
            .filter(|name| !excluded.contains(name))
            // Only keep names that really exist in the flake, so a typo can't silently
            // become a no-op that looks like "no updates".
            .filter(|name| available.iter().any(|a| a == name))
            .map(String::from)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Config {
        Config {
            flake_path: PathBuf::from("/home/u/cfg"),
            host_attr: "nixos-x1".into(),
            update_inputs: vec![],
            exclude_inputs: vec![],
            interval_secs: DEFAULT_INTERVAL_SECS,
            notify: true,
            nixpkgs_ref_for_changelogs: None,
            icons: Icons::default(),
        }
    }

    #[test]
    fn exclude_wins_over_allow_list() {
        let mut c = base();
        c.update_inputs = vec!["nixpkgs".into(), "nixpkgs-kernel".into()];
        c.exclude_inputs = vec!["nixpkgs-kernel".into()];
        let avail = vec![
            "nixpkgs".to_string(),
            "nixpkgs-kernel".to_string(),
            "home-manager".to_string(),
        ];
        assert_eq!(c.effective_update_inputs(&avail), vec!["nixpkgs"]);
    }

    #[test]
    fn empty_allow_list_means_all_minus_excluded() {
        let mut c = base();
        c.exclude_inputs = vec!["nixpkgs-kernel".into()];
        let avail = vec![
            "nixpkgs".to_string(),
            "nixpkgs-kernel".to_string(),
            "home-manager".to_string(),
        ];
        assert_eq!(
            c.effective_update_inputs(&avail),
            vec!["nixpkgs", "home-manager"]
        );
    }

    #[test]
    fn nonexistent_input_is_dropped() {
        let mut c = base();
        c.update_inputs = vec!["nixpkgs".into(), "typo-input".into()];
        let avail = vec!["nixpkgs".to_string()];
        assert_eq!(c.effective_update_inputs(&avail), vec!["nixpkgs"]);
    }

    #[test]
    fn parses_minimal_toml() {
        let toml = r#"
            flake_path = "/home/u/cfg"
            host_attr = "nixos-x1"
            exclude_inputs = ["nixpkgs-kernel"]
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        c.validate().unwrap();
        assert_eq!(c.interval_secs, DEFAULT_INTERVAL_SECS);
        assert!(c.notify);
        assert_eq!(c.exclude_inputs, vec!["nixpkgs-kernel"]);
    }
}
