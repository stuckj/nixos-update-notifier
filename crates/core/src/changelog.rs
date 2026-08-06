//! Best-effort resolution of `meta.changelog` URLs for changed packages.
//!
//! Coverage in nixpkgs is partial (firefox/git/ripgrep/brave have it; vlc/obs-studio do
//! not). We map each changed derivation name to a top-level nixpkgs attribute as best we
//! can and treat every miss as non-fatal — the UI simply shows no link.

use crate::config::Config;
use crate::diff::PackageChange;
use crate::lock;
use crate::nix;
use std::path::Path;

/// The input name we look for in the lock. A flake that calls its nixpkgs something else
/// has to name a ref outright via `nixpkgs_ref_for_changelogs` — that option is a flake
/// ref, not an input name, so it cannot express "the input called `nixos`".
const NIXPKGS_INPUT: &str = "nixpkgs";

/// Whether a flake ref resolves without going through the registry or the network.
///
/// Only used to warn. The value that matters is the old default, the bare alias `nixpkgs`,
/// which resolves through the registry to a channel tarball: a config still carrying it
/// keeps downloading on every check and keeps showing links from an unrelated revision —
/// exactly what this module stopped doing by default. A local path counts, even though it
/// pins no revision, because it costs no download and is the user's own tree. Deliberately
/// a heuristic; guessing wrong only costs a log line.
fn looks_pinned(r: &str) -> bool {
    r.contains("narHash=")
        || r.contains("rev=")
        || r.starts_with("path:/")
        || r.starts_with('/')
        // A bare revision, as in `github:NixOS/nixpkgs/<40 hex>`.
        || r.split(['/', '?', '&', '='])
            .any(|seg| seg.len() == 40 && seg.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Resolve the nixpkgs to look changelogs up against: the explicit config value if set,
/// otherwise the `nixpkgs` recorded in the CANDIDATE lock.
///
/// The candidate is the right source and neither obvious alternative is. The registry alias
/// `nixpkgs` — what this used to use — resolves to a channel tarball URL, so it downloads
/// (a check must not) and reports a revision unrelated to either side of the diff. The
/// flake's *current* input is no better: that is the revision being upgraded away from, so
/// for `nano 9.1 -> 9.2` it describes 9.1. The candidate lock names the revision the update
/// would actually install, and `nix flake update` has already fetched it, so reading it
/// costs nothing and reaches the network only if that fetch is somehow gone.
///
/// `None` means "no changelogs this time". That is deliberate: no links beats a download
/// plus links from the wrong revision.
fn nixpkgs_ref(cfg: &Config, candidate_lock: &Path) -> Option<String> {
    if let Some(explicit) = &cfg.nixpkgs_ref_for_changelogs {
        if !looks_pinned(explicit) {
            tracing::warn!(
                "nixpkgs_ref_for_changelogs = {explicit:?} is not pinned to a revision: \
                 resolving it may download, and changelogs will describe whatever that ref \
                 points at rather than the version being offered. Unset it to use the \
                 candidate lock's nixpkgs."
            );
        }
        return Some(explicit.clone());
    }
    let raw = std::fs::read_to_string(candidate_lock)
        .map_err(|e| {
            tracing::info!(
                "no changelogs: cannot read candidate lock {}: {e}",
                candidate_lock.display()
            )
        })
        .ok()?;
    let r = lock::input_flake_ref(&raw, NIXPKGS_INPUT);
    if r.is_none() {
        // At info, not debug: this is why every link is missing, and the default filter
        // (`nun_core=info`) would otherwise hide the explanation entirely.
        tracing::info!(
            "no changelogs: candidate lock has no `{NIXPKGS_INPUT}` input pinned to a \
             revision (set nixpkgs_ref_for_changelogs to a pinned ref to override)"
        );
    }
    r
}

/// Candidate top-level nixpkgs attribute names to try for a given derivation name.
/// nixpkgs top-level attrs are the common case (firefox, git, ripgrep). We also try a
/// version-suffix-stripped form defensively. Language-scoped names
/// (e.g. `python3.11-requests`) usually won't resolve at top level; those simply miss.
fn attr_candidates(name: &str) -> Vec<String> {
    let mut out = vec![name.to_string()];
    // Strip a trailing `-<version>` if diff-closures ever left one on the name.
    if let Some((base, ver)) = name.rsplit_once('-') {
        if ver
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
        {
            out.push(base.to_string());
        }
    }
    out.dedup();
    out
}

/// Enrich each change in place with a changelog URL where one exists.
///
/// Best-effort throughout: this runs after the diff is complete and only ever writes
/// `PackageChange::changelog`, so every failure path here costs links, never correctness.
pub async fn enrich(cfg: &Config, candidate_lock: &Path, changes: &mut [PackageChange]) {
    // Only bother looking up things that were added or upgraded (a removed package has no
    // "new" changelog worth showing).
    let mut names: Vec<String> = changes
        .iter()
        .filter(|c| !matches!(c.kind, crate::diff::ChangeKind::Removed) && c.changelog.is_none())
        .flat_map(|c| attr_candidates(&c.name))
        .collect();
    names.sort();
    names.dedup();

    if names.is_empty() {
        return;
    }

    let Some(nixpkgs) = nixpkgs_ref(cfg, candidate_lock) else {
        return;
    };
    let system = match nix::current_system_double().await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("no changelogs: could not determine the current system: {e:#}");
            return;
        }
    };

    let map = nix::meta_changelogs(&nixpkgs, &system, &names).await;

    for change in changes.iter_mut() {
        if change.changelog.is_some() {
            continue;
        }
        for cand in attr_candidates(&change.name) {
            if let Some(url) = map.get(&cand) {
                change.changelog = Some(url.clone());
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attr_candidates_strips_version_suffix() {
        assert_eq!(attr_candidates("firefox"), vec!["firefox"]);
        let c = attr_candidates("git-2.44.0");
        assert!(c.contains(&"git-2.44.0".to_string()));
        assert!(c.contains(&"git".to_string()));
    }

    #[test]
    fn attr_candidates_ignores_non_version_suffix() {
        // "home-manager" should not be split into "home".
        assert_eq!(attr_candidates("home-manager"), vec!["home-manager"]);
    }

    fn cfg_with(explicit: Option<&str>) -> Config {
        Config {
            flake_path: std::path::PathBuf::from("/home/u/cfg"),
            host_attr: "host".into(),
            update_inputs: vec![],
            exclude_inputs: vec![],
            interval_secs: 86400,
            notify: true,
            nixpkgs_ref_for_changelogs: explicit.map(str::to_string),
            icons: crate::config::Icons::default(),
        }
    }

    #[test]
    fn an_explicit_ref_wins_over_the_lock() {
        let cfg = cfg_with(Some("github:NixOS/nixpkgs/deadbeef"));
        // The lock is not even read, so a path that cannot exist is fine.
        let got = nixpkgs_ref(&cfg, Path::new("/nonexistent/flake.lock"));
        assert_eq!(got.as_deref(), Some("github:NixOS/nixpkgs/deadbeef"));
    }

    #[test]
    fn an_unreadable_lock_means_no_changelogs_not_a_fallback() {
        // Falling back to the registry alias here is what downloaded a channel tarball.
        let cfg = cfg_with(None);
        assert_eq!(
            nixpkgs_ref(&cfg, Path::new("/nonexistent/flake.lock")),
            None
        );
    }

    #[test]
    fn derives_the_ref_from_the_candidate_lock() {
        let dir = std::env::temp_dir().join(format!("nun-cl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("flake.lock");
        std::fs::write(
            &p,
            r#"{"version":7,"root":"root","nodes":{
                 "root":{"inputs":{"nixpkgs":"nixpkgs"}},
                 "nixpkgs":{"locked":{"type":"github","owner":"NixOS","repo":"nixpkgs","rev":"cafe"}}}}"#,
        )
        .unwrap();
        assert_eq!(
            nixpkgs_ref(&cfg_with(None), &p).as_deref(),
            Some("github:NixOS/nixpkgs/cafe")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recognises_which_refs_carry_a_pin() {
        // The value that matters: the old default, which resolves to a channel tarball.
        assert!(!looks_pinned("nixpkgs"));
        assert!(!looks_pinned("github:NixOS/nixpkgs/nixos-unstable"));
        assert!(!looks_pinned(
            "https://channels.nixos.org/nixpkgs-unstable/nixexprs.tar.xz"
        ));

        assert!(looks_pinned(
            "github:NixOS/nixpkgs/04607e1165ac22c5fde6dcc54c9e0b3c0487c555"
        ));
        assert!(looks_pinned("github:NixOS/nixpkgs/abc?narHash=sha256-x"));
        assert!(looks_pinned("git+https://ex.com/n.git?rev=abc"));
        assert!(looks_pinned("path:/nix/store/abc-source"));
    }
}
