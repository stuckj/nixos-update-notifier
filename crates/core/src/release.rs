//! New NixOS stable releases: detecting them, and previewing an upgrade to one.
//!
//! A check only advances inputs within the branches `flake.nix` already names, so moving
//! from `nixos-26.05` to `nixos-26.11` is a different operation: it rewrites the branch in
//! `flake.nix` itself. It is kept apart from "Apply updates" and is never automatic.
//!
//! Detection lists the `nixos-YY.MM` branches of nixpkgs with `git ls-remote` (a few KB,
//! no checkout) and compares them with the branch the configured release inputs are
//! locked from (`original.ref` in `flake.lock`). A flake that tracks `nixos-unstable` has no
//! release to compare and is simply skipped.

use crate::check;
use crate::config::Config;
use crate::diff::PackageChange;
use crate::{changelog, lock, nix, pkgs};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const NIXPKGS_GIT: &str = "https://github.com/NixOS/nixpkgs";
pub const RELEASE_NOTES_URL: &str = "https://nixos.org/manual/nixos/stable/release-notes";

/// A NixOS release, `YY.MM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub year: u32,
    pub month: u32,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02}.{:02}", self.year, self.month)
    }
}

impl std::str::FromStr for Version {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let (y, m) = s.split_once('.').context("expected YY.MM")?;
        anyhow::ensure!(y.len() == 2 && m.len() == 2, "expected YY.MM, got {s:?}");
        let v = Version {
            year: y.parse().context("year")?,
            month: m.parse().context("month")?,
        };
        anyhow::ensure!((1..=12).contains(&v.month), "month out of range in {s:?}");
        Ok(v)
    }
}

/// Split a release branch name into its prefix and version: `nixos-26.05` →
/// `("nixos", 26.05)`, `release-26.05` (home-manager) → `("release", 26.05)`.
/// `-small` channels, `unstable` and anything else yield `None`.
pub fn branch_version(r: &str) -> Option<(&str, Version)> {
    let (prefix, ver) = r.split_once('-')?;
    if !matches!(prefix, "nixos" | "release") {
        return None;
    }
    Some((prefix, ver.parse().ok()?))
}

/// Release versions from `git ls-remote` output (`<sha>\trefs/heads/nixos-26.05`), sorted
/// and deduplicated.
pub fn parse_ls_remote(out: &str) -> Vec<Version> {
    let mut v: Vec<Version> = out
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .filter_map(|r| r.strip_prefix("refs/heads/"))
        .filter_map(branch_version)
        .filter(|(p, _)| *p == "nixos")
        .map(|(_, v)| v)
        .collect();
    v.sort();
    v.dedup();
    v
}

/// A calendar date, `(year, month, day)`.
pub type Date = (i32, u32, u32);

/// Today's UTC date, without a calendar dependency.
pub fn today() -> Date {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    civil_from_days((secs / 86_400) as i64)
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> Date {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = (yoe + era * 400) as i32 + i32::from(m <= 2);
    (y, m, d)
}

/// The first day of the month `n` months after `v`'s release month.
fn months_after(v: Version, n: u32) -> Date {
    let idx = v.month - 1 + n;
    (2000 + (v.year + idx / 12) as i32, idx % 12 + 1, 1)
}

/// Whether a release is out. The `nixos-YY.MM` branch is created a few weeks before the
/// release, during beta, so the branch existing is not enough. A release is treated as out
/// from the first day of the month after its release month — never early, at most a few
/// days late.
pub fn is_released(v: Version, today: Date) -> bool {
    today >= months_after(v, 1)
}

/// Whether a release is past its support window: a release is maintained until about a
/// month after the next one, i.e. until the end of the month after the next release month.
pub fn is_eol(v: Version, released: &[Version], today: Date) -> bool {
    released
        .iter()
        .filter(|r| **r > v)
        .min()
        .is_some_and(|next| today >= months_after(*next, 2))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseChoice {
    pub version: String,
    /// Past its support window already (an older target when several are available).
    pub eol: bool,
    /// The release right after the current one. The NixOS manual recommends upgrading one
    /// release at a time, since removed options are usually warned about for one release
    /// before they become errors.
    pub next: bool,
}

/// Newer releases the user could upgrade to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseOffer {
    pub current: String,
    pub current_eol: bool,
    /// Oldest first; never empty.
    pub choices: Vec<ReleaseChoice>,
    pub notes_url: String,
}

impl ReleaseOffer {
    pub fn newest(&self) -> &str {
        self.choices
            .last()
            .map(|c| c.version.as_str())
            .unwrap_or("")
    }
}

/// Build the offer for `current` from the branches that exist, or `None` if nothing newer
/// has been released.
pub fn offer(current: Version, branches: &[Version], today: Date) -> Option<ReleaseOffer> {
    let released: Vec<Version> = branches
        .iter()
        .copied()
        .filter(|v| is_released(*v, today))
        .collect();
    let newer: Vec<Version> = released.iter().copied().filter(|v| *v > current).collect();
    let first = *newer.first()?;
    Some(ReleaseOffer {
        current: current.to_string(),
        current_eol: is_eol(current, &released, today),
        choices: newer
            .iter()
            .map(|v| ReleaseChoice {
                version: v.to_string(),
                eol: is_eol(*v, &released, today),
                next: *v == first,
            })
            .collect(),
        notes_url: RELEASE_NOTES_URL.to_string(),
    })
}

/// The release the system is on: the `nixos-YY.MM` branch of the first of `release_inputs`
/// the flake has (normally nixpkgs). `None` — no upgrade to offer — when that input is
/// excluded or not on such a branch (`nixos-unstable`, `nixos-26.05-small`, a pinned
/// revision): an upgrade must move the input that decides the release, or it would move
/// only the others (home-manager) and leave the system where it was.
pub fn current_release(
    lock_json: &str,
    release_inputs: &[String],
    exclude_inputs: &[String],
) -> Option<Version> {
    let input = release_inputs
        .iter()
        .find(|i| lock::has_input(lock_json, i))?;
    if exclude_inputs.contains(input) {
        return None;
    }
    match branch_version(&lock::original_ref(lock_json, input)?)? {
        ("nixos", v) => Some(v),
        _ => None,
    }
}

/// List the released nixpkgs release branches.
pub async fn ls_remote_releases() -> Result<Vec<Version>> {
    let mut c = tokio::process::Command::new("git");
    c.args(["ls-remote", "--heads", NIXPKGS_GIT, "refs/heads/nixos-*"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    let out = tokio::time::timeout(std::time::Duration::from_secs(60), c.output())
        .await
        .context("git ls-remote timed out")?
        .context("running git ls-remote (is git installed?)")?;
    if !out.status.success() {
        bail!(
            "git ls-remote {NIXPKGS_GIT} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(parse_ls_remote(&String::from_utf8_lossy(&out.stdout)))
}

/// Look for a newer release than the one the flake tracks. `Ok(None)` when disabled, when
/// the flake tracks no release branch, or when nothing newer is out.
pub async fn detect(cfg: &Config) -> Result<Option<ReleaseOffer>> {
    if !cfg.check_releases {
        return Ok(None);
    }
    let lock_json = tokio::fs::read_to_string(cfg.flake_path.join("flake.lock"))
        .await
        .context("reading flake.lock")?;
    let Some(current) = current_release(&lock_json, &cfg.release_inputs, &cfg.exclude_inputs)
    else {
        return Ok(None);
    };
    let branches = ls_remote_releases().await?;
    Ok(offer(current, &branches, today()))
}

/// Which `(input, old ref)` pairs an upgrade from `current` retargets: the configured
/// release inputs, minus `exclude_inputs`, that are locked from a release branch of
/// `current`.
pub fn inputs_to_retarget(
    cfg: &Config,
    lock_json: &str,
    current: Version,
) -> Vec<(String, String)> {
    lock::original_refs(lock_json)
        .into_iter()
        .filter(|(name, _)| cfg.release_inputs.contains(name) && !cfg.exclude_inputs.contains(name))
        .filter(|(_, r)| branch_version(r).is_some_and(|(_, v)| v == current))
        .collect()
}

/// Rewrite the release branch of `targets` in `flake.nix` to `to`.
///
/// Only the literal branch names are replaced, e.g. `nixos-26.05` → `nixos-26.11`, which
/// covers `github:NixOS/nixpkgs/nixos-26.05`, `?ref=nixos-26.05` and similar URL forms.
/// It refuses rather than guesses when:
///   * a branch does not appear in `flake.nix` literally (built from a variable, say), or
///   * another input, one that is not being retargeted, tracks the same branch — the
///     occurrences cannot be told apart textually.
///
/// `all_refs` is every top-level input's `original.ref`, for the second check.
pub fn retarget(
    flake_nix: &str,
    all_refs: &[(String, String)],
    targets: &[(String, String)],
    to: Version,
) -> Result<String> {
    let mut text = flake_nix.to_string();
    let mut done: Vec<&str> = Vec::new();
    for (_, old) in targets {
        if done.contains(&old.as_str()) {
            continue;
        }
        done.push(old);
        let others: Vec<&str> = all_refs
            .iter()
            .filter(|(n, r)| r == old && !targets.iter().any(|(t, _)| t == n))
            .map(|(n, _)| n.as_str())
            .collect();
        if !others.is_empty() {
            bail!(
                "input(s) {} also track {old} but are not being upgraded, so the change \
                 cannot be made automatically; edit flake.nix by hand",
                others.join(", ")
            );
        }
        let (prefix, _) = branch_version(old).context("not a release branch")?;
        let new = format!("{prefix}-{to}");
        let (replaced, n) = replace_token(&text, old, &new);
        if n == 0 {
            bail!(
                "{old} does not appear literally in flake.nix, so it cannot be changed \
                 automatically; change it to {new} by hand"
            );
        }
        text = replaced;
    }
    Ok(text)
}

/// Replace whole-token occurrences of `old` (not part of a longer name such as
/// `nixos-26.05-small` or `xnixos-26.05`). Returns the new text and the count replaced.
fn replace_token(text: &str, old: &str, new: &str) -> (String, usize) {
    let is_word = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_');
    let mut out = String::with_capacity(text.len());
    let mut n = 0;
    let mut rest = text;
    while let Some(i) = rest.find(old) {
        let before = rest[..i].chars().next_back();
        let after = rest[i + old.len()..].chars().next();
        out.push_str(&rest[..i]);
        if before.is_some_and(is_word) || after.is_some_and(is_word) {
            out.push_str(old);
        } else {
            out.push_str(new);
            n += 1;
        }
        rest = &rest[i + old.len()..];
    }
    out.push_str(rest);
    (out, n)
}

/// A previewed upgrade, ready to apply.
#[derive(Debug, Clone)]
pub struct UpgradePreview {
    pub target: Version,
    pub changes: Vec<PackageChange>,
    /// `(input, old ref, new ref)` for each retargeted input.
    pub retargeted: Vec<(String, String, String)>,
    /// The rewritten `flake.nix` and its lock, kept outside the repo until applied.
    pub candidate_flake_nix: PathBuf,
    pub candidate_lock: PathBuf,
    /// The `flake.nix` and `flake.lock` the preview was made from. The upgrade refuses to
    /// install over either if it has changed since: the candidate lock carries every other
    /// input at its preview-time revision, so installing it over a newer lock would move
    /// those inputs back, and a newer `flake.nix` would lose the user's edits.
    pub base_flake_nix: String,
    pub base_flake_lock: String,
}

/// Retarget the release inputs to `target` in a scratch flake, relock them, evaluate the
/// result against the user's repo and diff it with the current system — the same
/// download-free comparison a check makes. Nothing in the repo is touched.
pub async fn preview(cfg: &Config, target: Version) -> Result<UpgradePreview> {
    let repo = &cfg.flake_path;
    let lock_json = tokio::fs::read_to_string(repo.join("flake.lock"))
        .await
        .context("reading flake.lock")?;
    let flake_nix = tokio::fs::read_to_string(repo.join("flake.nix"))
        .await
        .context("reading flake.nix")?;
    let current = current_release(&lock_json, &cfg.release_inputs, &cfg.exclude_inputs)
        .context("the configured release inputs do not track a NixOS release branch")?;
    anyhow::ensure!(target > current, "{target} is not newer than {current}");

    let targets = inputs_to_retarget(cfg, &lock_json, current);
    anyhow::ensure!(!targets.is_empty(), "no release inputs to retarget");
    let new_nix = retarget(
        &flake_nix,
        &lock::original_refs(&lock_json),
        &targets,
        target,
    )?;

    let scratch = check::scratch_flake(repo, "upgrade").await?;
    tokio::fs::write(scratch.join("flake.nix"), &new_nix)
        .await
        .context("writing the retargeted flake.nix")?;
    let names: Vec<String> = targets.iter().map(|(n, _)| n.clone()).collect();
    let failed = nix::flake_update_inputs(&scratch, &names)
        .await
        .context("relocking the retargeted inputs")?;
    if let Some((name, reason)) = failed.first() {
        bail!("could not lock {name} at {target}: {reason}");
    }
    let new_lock = tokio::fs::read_to_string(scratch.join("flake.lock"))
        .await
        .context("reading the relocked flake.lock")?;

    // The repo's own flake.nix still names the old branches, and nix would relock an input
    // whose lock entry no longer matches it. Overriding the retargeted inputs with the
    // exact revisions just locked sidesteps that while still evaluating the user's tree.
    let mut overrides = Vec::new();
    for name in &names {
        let r = lock::input_flake_ref(&new_lock, name)
            .with_context(|| format!("rendering the locked ref of {name}"))?;
        overrides.push((name.clone(), r));
    }
    let repo_ref = repo.display().to_string();
    let current_drv = nix::toplevel_drv_path(&repo_ref, &cfg.host_attr, None, &[])
        .await
        .context("evaluating current toplevel drvPath")?;
    let scratch_lock = scratch.join("flake.lock");
    let candidate_drv =
        nix::toplevel_drv_path(&repo_ref, &cfg.host_attr, Some(&scratch_lock), &overrides)
            .await
            .with_context(|| format!("evaluating the system on NixOS {target}"))?;

    let (candidate_lock, candidate_flake_nix) = check::save_candidate(&scratch, "upgrade").await?;
    if let Err(e) = tokio::fs::remove_dir_all(&scratch).await {
        tracing::warn!("could not remove scratch dir {}: {e}", scratch.display());
    }

    let mut changes = pkgs::diff(&current_drv, &candidate_drv)
        .await
        .context("diffing derivation closures")?;
    check::annotate_runtime(&mut changes).await;
    changelog::enrich(cfg, &candidate_lock, &mut changes).await;

    Ok(UpgradePreview {
        target,
        changes,
        retargeted: targets
            .into_iter()
            .map(|(n, old)| {
                let prefix = branch_version(&old).map(|(p, _)| p).unwrap_or("nixos");
                let new = format!("{prefix}-{target}");
                (n, old, new)
            })
            .collect(),
        candidate_flake_nix,
        candidate_lock,
        base_flake_nix: flake_nix,
        base_flake_lock: lock_json,
    })
}

/// Fail if `flake.nix` or `flake.lock` in the repo is no longer what `preview` was made
/// from.
pub async fn ensure_repo_unchanged(repo: &Path, preview: &UpgradePreview) -> Result<()> {
    for (file, base) in [
        ("flake.nix", &preview.base_flake_nix),
        ("flake.lock", &preview.base_flake_lock),
    ] {
        let now = tokio::fs::read_to_string(repo.join(file))
            .await
            .with_context(|| format!("reading {file}"))?;
        anyhow::ensure!(
            &now == base,
            "{file} has changed since the upgrade was previewed; preview it again"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    #[test]
    fn parses_release_branches_and_ignores_the_rest() {
        let out = "\
a\trefs/heads/nixos-25.05
b\trefs/heads/nixos-25.05-small
c\trefs/heads/nixos-unstable
d\trefs/heads/nixos-unstable-small
e\trefs/heads/nixos-26.05
f\trefs/heads/nixos-25.11
g\trefs/heads/nixos-24.11
";
        assert_eq!(
            parse_ls_remote(out),
            vec![v("24.11"), v("25.05"), v("25.11"), v("26.05")]
        );
    }

    #[test]
    fn branch_names_split_into_prefix_and_version() {
        assert_eq!(branch_version("nixos-26.05"), Some(("nixos", v("26.05"))));
        assert_eq!(
            branch_version("release-26.05"),
            Some(("release", v("26.05")))
        );
        assert_eq!(branch_version("nixos-unstable"), None);
        assert_eq!(branch_version("nixos-26.05-small"), None);
        assert_eq!(branch_version("master"), None);
        assert_eq!(branch_version("nixos-26.13"), None);
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_399), (2025, 11, 7));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn a_branch_in_beta_is_not_a_release() {
        // nixos-26.11 is branched off in early November, weeks before the release.
        assert!(!is_released(v("26.11"), (2026, 11, 7)));
        assert!(is_released(v("26.11"), (2026, 12, 1)));
        assert!(is_released(v("26.05"), (2026, 11, 7)));
    }

    #[test]
    fn support_ends_a_month_after_the_next_release() {
        let rel = [v("25.05"), v("25.11"), v("26.05")];
        // 25.11 is superseded by 26.05 (May); supported through June.
        assert!(!is_eol(v("25.11"), &rel, (2026, 6, 30)));
        assert!(is_eol(v("25.11"), &rel, (2026, 7, 1)));
        // The newest release is never EOL.
        assert!(!is_eol(v("26.05"), &rel, (2030, 1, 1)));
        // A November release rolls over the year.
        assert_eq!(months_after(v("25.11"), 2), (2026, 1, 1));
    }

    #[test]
    fn offers_only_released_newer_versions() {
        let branches = [v("25.05"), v("25.11"), v("26.05"), v("26.11")];
        // Today 26.11 is still in beta: on 26.05 there is nothing to offer.
        assert_eq!(offer(v("26.05"), &branches, (2026, 11, 7)), None);

        let o = offer(v("25.11"), &branches, (2026, 12, 15)).unwrap();
        assert_eq!(o.current, "25.11");
        assert!(o.current_eol);
        let got: Vec<(&str, bool, bool)> = o
            .choices
            .iter()
            .map(|c| (c.version.as_str(), c.eol, c.next))
            .collect();
        assert_eq!(got, vec![("26.05", false, true), ("26.11", false, false)]);
        assert_eq!(o.newest(), "26.11");
    }

    fn lock_with(inputs: &[(&str, &str)]) -> String {
        let mut nodes = serde_json::Map::new();
        let mut root_inputs = serde_json::Map::new();
        for (name, r) in inputs {
            root_inputs.insert(name.to_string(), serde_json::json!(name));
            nodes.insert(
                name.to_string(),
                serde_json::json!({
                    "locked": {"type": "github", "owner": "o", "repo": name, "rev": "abc"},
                    "original": {"type": "github", "owner": "o", "repo": name, "ref": r}
                }),
            );
        }
        nodes.insert("root".into(), serde_json::json!({ "inputs": root_inputs }));
        serde_json::json!({"nodes": nodes, "root": "root", "version": 7}).to_string()
    }

    fn cfg() -> Config {
        toml::from_str(
            r#"
            flake_path = "/r"
            host_attr = "h"
            exclude_inputs = ["nixpkgs-kernel"]
            "#,
        )
        .unwrap()
    }

    #[test]
    fn finds_the_current_release_and_the_inputs_to_move() {
        let lock = lock_with(&[
            ("nixpkgs", "nixos-26.05"),
            ("home-manager", "release-26.05"),
            ("nixpkgs-kernel", "nixos-26.05"),
            ("hardware", "master"),
        ]);
        let c = cfg();
        assert_eq!(
            current_release(&lock, &c.release_inputs, &c.exclude_inputs),
            Some(v("26.05"))
        );
        assert_eq!(
            inputs_to_retarget(&c, &lock, v("26.05")),
            vec![
                ("home-manager".to_string(), "release-26.05".to_string()),
                ("nixpkgs".to_string(), "nixos-26.05".to_string()),
            ]
        );
    }

    #[test]
    fn an_unstable_flake_tracks_no_release() {
        let c = cfg();
        // Home-manager on a release branch must not make nixpkgs' release look like that one.
        for nixpkgs in ["nixos-unstable", "nixos-26.05-small"] {
            let lock = lock_with(&[("nixpkgs", nixpkgs), ("home-manager", "release-26.05")]);
            assert_eq!(
                current_release(&lock, &c.release_inputs, &c.exclude_inputs),
                None,
                "{nixpkgs}"
            );
        }
    }

    #[test]
    fn home_manager_never_decides_the_release() {
        let c = cfg();
        // nixpkgs pinned to a revision (no ref): its release is unknown.
        let pinned = lock_with(&[("home-manager", "release-26.05")]).replace(
            r#""inputs":{"home-manager":"home-manager"}"#,
            r#""inputs":{"home-manager":"home-manager","nixpkgs":"nixpkgs"}"#,
        );
        let pinned = pinned.replacen(
            r#""root":{"#,
            r#""nixpkgs":{"locked":{"type":"github","owner":"o","repo":"nixpkgs","rev":"abc"},"original":{"type":"github","owner":"o","repo":"nixpkgs","rev":"abc"}},"root":{"#,
            1,
        );
        assert!(lock::has_input(&pinned, "nixpkgs"), "{pinned}");
        assert_eq!(
            current_release(&pinned, &c.release_inputs, &c.exclude_inputs),
            None
        );
        // No input called nixpkgs at all: home-manager's branch is not a NixOS release.
        let renamed = lock_with(&[("nixos", "nixos-26.05"), ("home-manager", "release-26.05")]);
        assert_eq!(
            current_release(&renamed, &c.release_inputs, &c.exclude_inputs),
            None
        );
    }

    #[test]
    fn an_excluded_nixpkgs_means_no_upgrade_is_offered() {
        let lock = lock_with(&[
            ("nixpkgs", "nixos-26.05"),
            ("home-manager", "release-26.05"),
        ]);
        let c = cfg();
        let excluded = vec!["nixpkgs".to_string()];
        assert_eq!(current_release(&lock, &c.release_inputs, &excluded), None);
    }

    const FLAKE: &str = r#"{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    small.url = "github:NixOS/nixpkgs/nixos-26.05-small";
    home-manager.url = "github:nix-community/home-manager?ref=release-26.05";
    home-manager.inputs.nixpkgs.follows = "nixpkgs";
  };
  outputs = _: { }; # system.stateVersion = "26.05";
}"#;

    #[test]
    fn retargets_the_branch_names_only() {
        let all = vec![
            ("home-manager".to_string(), "release-26.05".to_string()),
            ("nixpkgs".to_string(), "nixos-26.05".to_string()),
            ("small".to_string(), "nixos-26.05-small".to_string()),
        ];
        let out = retarget(FLAKE, &all, &all[..2], v("26.11")).unwrap();
        assert!(
            out.contains(r#""github:NixOS/nixpkgs/nixos-26.11""#),
            "{out}"
        );
        assert!(out.contains("home-manager?ref=release-26.11"), "{out}");
        // Untouched: a longer branch name, and the stateVersion.
        assert!(out.contains("nixos-26.05-small"), "{out}");
        assert!(out.contains(r#"stateVersion = "26.05""#), "{out}");
    }

    #[test]
    fn refuses_when_another_input_shares_the_branch() {
        let all = vec![
            ("nixpkgs".to_string(), "nixos-26.05".to_string()),
            ("nixpkgs-kernel".to_string(), "nixos-26.05".to_string()),
        ];
        let e = retarget(FLAKE, &all, &all[..1], v("26.11")).unwrap_err();
        assert!(e.to_string().contains("nixpkgs-kernel"), "{e}");
    }

    #[tokio::test]
    async fn an_upgrade_refuses_a_repo_changed_since_its_preview() {
        let dir = crate::testutil::scratch_dir("upgrade-base");
        std::fs::write(dir.join("flake.nix"), "nix-0").unwrap();
        std::fs::write(dir.join("flake.lock"), "lock-0").unwrap();
        let p = UpgradePreview {
            target: v("26.11"),
            changes: vec![],
            retargeted: vec![],
            candidate_flake_nix: PathBuf::new(),
            candidate_lock: PathBuf::new(),
            base_flake_nix: "nix-0".into(),
            base_flake_lock: "lock-0".into(),
        };
        ensure_repo_unchanged(&dir, &p).await.unwrap();

        // An update applied after the preview: installing the preview's lock now would
        // move every other input back to its preview-time revision.
        std::fs::write(dir.join("flake.lock"), "lock-1").unwrap();
        let e = ensure_repo_unchanged(&dir, &p).await.unwrap_err();
        assert!(e.to_string().contains("flake.lock"), "{e}");

        std::fs::write(dir.join("flake.lock"), "lock-0").unwrap();
        std::fs::write(dir.join("flake.nix"), "nix-1").unwrap();
        let e = ensure_repo_unchanged(&dir, &p).await.unwrap_err();
        assert!(e.to_string().contains("flake.nix"), "{e}");
    }

    #[test]
    fn refuses_when_the_branch_is_not_written_literally() {
        let all = vec![("nixpkgs".to_string(), "nixos-26.05".to_string())];
        let nix = r#"{ inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-${rel}"; }"#;
        let e = retarget(nix, &all, &all, v("26.11")).unwrap_err();
        assert!(e.to_string().contains("by hand"), "{e}");
    }
}
