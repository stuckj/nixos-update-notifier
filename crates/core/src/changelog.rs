//! Best-effort resolution of `meta.changelog` URLs for changed packages.
//!
//! Coverage in nixpkgs is partial (firefox/git/ripgrep/brave have it; vlc/obs-studio do
//! not). We map each changed derivation name to a top-level nixpkgs attribute as best we
//! can and treat every miss as non-fatal — the UI simply shows no link.

use crate::config::Config;
use crate::diff::PackageChange;
use crate::nix;

/// Resolve the nixpkgs flake ref to look changelogs up against:
/// explicit config value, else the flake's own `nixpkgs` input (`<flake>#` resolves the
/// input named `nixpkgs`). We express "the flake's nixpkgs" as `<flake_path>` with the
/// `nixpkgs` input, which `nix eval` reaches via `<flake>#legacyPackages`… — but the
/// simplest portable handle is the registry alias `nixpkgs`. Prefer explicit config for
/// reproducibility.
pub fn nixpkgs_ref(cfg: &Config) -> String {
    cfg.nixpkgs_ref_for_changelogs
        .clone()
        .unwrap_or_else(|| "nixpkgs".to_string())
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
pub async fn enrich(cfg: &Config, changes: &mut [PackageChange]) {
    let nixpkgs = nixpkgs_ref(cfg);

    // Only bother looking up things that were added or upgraded (a removed package has no
    // "new" changelog worth showing).
    let names: Vec<String> = changes
        .iter()
        .filter(|c| !matches!(c.kind, crate::diff::ChangeKind::Removed))
        .flat_map(|c| attr_candidates(&c.name))
        .collect();

    if names.is_empty() {
        return;
    }

    let map = nix::meta_changelogs(&nixpkgs, &names).await;

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
}
