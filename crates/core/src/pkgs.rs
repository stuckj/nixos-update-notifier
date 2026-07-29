//! Package inventory of a derivation closure, from nix's own structured metadata.
//!
//! This replaces parsing `nix store diff-closures`' rendered output, which turned out to
//! be unreliable for this use. That command is built for *realised output* closures, where
//! store paths are tidy `name-version`. A no-download check has to diff *derivation*
//! closures instead, and there its name/version splitter sees things like
//! `curl-8.21.0.tar.xz.drv` beside `curl-8.21.0.drv` and produces incoherent groupings —
//! most visibly reporting `curl: ∅ → 8.21.0` on a machine that has curl 8.20.0 installed.
//!
//! Every heuristic that accumulated around that output — stripping ANSI colours, the two
//! empty markers (`∅` absent / `ε` versionless), peeling `.drv`, filtering `.tar.*`,
//! `.patch`, `.dmg`, mangled `CVE` names and toolchain bootstrap stages — existed to
//! reconstruct information nix already has.
//!
//! `nix derivation show` reports it directly: a derivation that builds a package carries
//! `pname` and `version` in its environment. Crucially, derivations that are NOT packages
//! (sources, patches, internal helpers) simply have no `pname`, so the noise filters
//! disappear by construction rather than by pattern-matching — in a real system closure,
//! only 6,175 of 18,695 derivations are packages.

use crate::diff::{ChangeKind, PackageChange};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};
use std::process::Stdio;

/// Only the fields we need. Serde skips everything else without allocating it, which
/// matters: the full JSON for a system closure is ~70 MB.
#[derive(Deserialize)]
struct Drv {
    #[serde(default)]
    env: Env,
    #[serde(default)]
    outputs: HashMap<String, Output>,
}

#[derive(Deserialize, Default)]
struct Env {
    pname: Option<String>,
    version: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize, Default)]
struct Output {
    /// Present only on FIXED-OUTPUT derivations — fetches. This is how a downloaded
    /// source is told apart from a built package, structurally, instead of by looking for
    /// `.tar.gz` in a filename.
    hash: Option<String>,
}

impl Drv {
    fn is_fetch(&self) -> bool {
        self.outputs.values().any(|o| o.hash.is_some())
    }

    /// `(pname, version)` for a derivation that builds a package, else `None`.
    ///
    /// Prefers the explicit `pname`/`version` attributes. Packages declared the older way,
    /// with just `name = "curl-8.20.0"`, carry neither — and silently dropping them would
    /// hide real upgrades (this is exactly what happened to the installed curl). For those
    /// we split `name` on nixpkgs' own convention: the first `-` followed by a digit
    /// separates package from version.
    fn package(&self) -> Option<(String, String)> {
        if self.is_fetch() {
            return None;
        }
        if let (Some(p), Some(v)) = (&self.env.pname, &self.env.version) {
            if !p.is_empty() && !v.is_empty() {
                return Some((p.clone(), v.clone()));
            }
        }
        split_name(self.env.name.as_deref()?)
    }
}

/// Split `"<pname>-<version>"` at the first `-` that begins a version.
fn split_name(name: &str) -> Option<(String, String)> {
    let bytes = name.as_bytes();
    for (i, ch) in name.char_indices() {
        if ch == '-'
            && bytes
                .get(i + 1)
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
        {
            return Some((name[..i].to_string(), name[i + 1..].to_string()));
        }
    }
    None
}

/// `nix derivation show` gained a `{"derivations": …}` wrapper in newer releases; older
/// ones emit the map directly. Accept both so a nix upgrade doesn't silently break us.
#[derive(Deserialize)]
#[serde(untagged)]
enum ShowOutput {
    Wrapped { derivations: HashMap<String, Drv> },
    Flat(HashMap<String, Drv>),
}

impl ShowOutput {
    fn into_map(self) -> HashMap<String, Drv> {
        match self {
            ShowOutput::Wrapped { derivations } => derivations,
            ShowOutput::Flat(m) => m,
        }
    }
}

/// Package name -> the set of versions present in a closure.
///
/// A closure legitimately holds several versions of the same package (a system can carry
/// both `zfs-user` 2.4.2 and 2.4.3), so this is a set, not a single value. Comparing sets
/// is what makes "one of two versions went away" render correctly instead of looking like
/// a removal.
pub type Inventory = HashMap<String, BTreeSet<String>>;

/// Read the package inventory of a derivation closure.
///
/// Evaluation-only: it reads already-instantiated `.drv` files and downloads nothing.
pub async fn inventory(drv_path: &str) -> Result<Inventory> {
    // The whole thing runs on a blocking thread: parsing tens of MB of JSON is CPU-bound
    // and must not stall the runtime shared with the tray and the D-Bus service. Doing the
    // spawn there too lets us stream straight from the pipe with std, so the JSON is never
    // held in memory in full.
    let drv = drv_path.to_string();
    tokio::task::spawn_blocking(move || inventory_blocking(&drv))
        .await
        .context("joining derivation-show reader")?
}

fn inventory_blocking(drv_path: &str) -> Result<Inventory> {
    let mut child = std::process::Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "--no-warn-dirty",
            "derivation",
            "show",
            "-r",
            drv_path,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning `nix derivation show`")?;

    let stdout = child
        .stdout
        .take()
        .context("capturing `nix derivation show` output")?;

    let reader = std::io::BufReader::with_capacity(1 << 20, stdout);
    let parsed = serde_json::from_reader::<_, ShowOutput>(reader)
        .context("parsing `nix derivation show` JSON")?
        .into_map();

    let status = child.wait().context("waiting for nix")?;
    anyhow::ensure!(status.success(), "`nix derivation show` failed ({status})");

    let mut inv: Inventory = HashMap::new();
    for drv in parsed.into_values() {
        // Fetches (fixed-output derivations) are sources, not packages; anything without a
        // usable name isn't one either. Together these replace the whole hand-written
        // noise filter that the text-parsing approach needed.
        if let Some((pname, version)) = drv.package() {
            inv.entry(pname).or_default().insert(version);
        }
    }
    Ok(inv)
}

/// Compare two closures' package inventories.
///
/// A package is reported only when its *set of versions* differs, so a derivation that was
/// merely rebuilt (same pname, same version, new hash — which a nixpkgs bump does to
/// thousands of them) is correctly silent.
pub fn diff_inventories(before: &Inventory, after: &Inventory) -> Vec<PackageChange> {
    let mut names: BTreeSet<&String> = before.keys().collect();
    names.extend(after.keys());

    let mut out = Vec::new();
    for name in names {
        let old = before.get(name);
        let new = after.get(name);
        if old == new {
            continue;
        }
        let old_v: Vec<String> = old.map(|s| s.iter().cloned().collect()).unwrap_or_default();
        let new_v: Vec<String> = new.map(|s| s.iter().cloned().collect()).unwrap_or_default();

        let kind = match (old_v.is_empty(), new_v.is_empty()) {
            (true, true) => continue,
            (true, false) => ChangeKind::Added,
            (false, true) => ChangeKind::Removed,
            (false, false) => ChangeKind::Changed,
        };

        out.push(PackageChange {
            name: name.clone(),
            old: old_v,
            new: new_v,
            kind,
            // Not available from derivation metadata; diff-closures' size deltas were the
            // one thing lost in this move, and they were only ever advisory.
            size_delta: None,
            changelog: None,
            runtime: true,
        });
    }
    out
}

/// Full structured diff between two derivation closures.
pub async fn diff(current_drv: &str, candidate_drv: &str) -> Result<Vec<PackageChange>> {
    let (before, after) = tokio::try_join!(inventory(current_drv), inventory(candidate_drv))?;
    tracing::debug!(
        before = before.len(),
        after = after.len(),
        "package inventories"
    );
    Ok(diff_inventories(&before, &after))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inv(entries: &[(&str, &[&str])]) -> Inventory {
        entries
            .iter()
            .map(|(n, vs)| {
                (
                    n.to_string(),
                    vs.iter().map(|v| v.to_string()).collect::<BTreeSet<_>>(),
                )
            })
            .collect()
    }

    #[test]
    fn reports_a_real_upgrade() {
        let a = inv(&[("ffmpeg", &["8.1.1"])]);
        let b = inv(&[("ffmpeg", &["8.1.2"])]);
        let d = diff_inventories(&a, &b);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name, "ffmpeg");
        assert_eq!(d[0].old, vec!["8.1.1"]);
        assert_eq!(d[0].new, vec!["8.1.2"]);
        assert_eq!(d[0].kind, ChangeKind::Changed);
    }

    #[test]
    fn a_rebuild_at_the_same_version_is_not_a_change() {
        // A nixpkgs bump rebuilds thousands of derivations without changing their version.
        // The old text-parsing approach had no way to see that; this must stay silent.
        let a = inv(&[("openssl", &["3.6.3"]), ("zlib", &["1.3.2"])]);
        assert!(diff_inventories(&a, &a).is_empty());
    }

    #[test]
    fn an_installed_package_getting_a_new_version_is_an_upgrade_not_an_add() {
        // The bug that motivated this module: curl 8.20.0 was installed, and the old
        // approach reported `curl: ∅ -> 8.21.0` as though it were newly appearing.
        let a = inv(&[("curl", &["8.18.0", "8.20.0"])]);
        let b = inv(&[("curl", &["8.20.0", "8.21.0"])]);
        let d = diff_inventories(&a, &b);
        assert_eq!(d[0].kind, ChangeKind::Changed);
        assert_eq!(d[0].old, vec!["8.18.0", "8.20.0"]);
        assert_eq!(d[0].new, vec!["8.20.0", "8.21.0"]);
    }

    #[test]
    fn dropping_one_of_several_versions_reads_as_a_change() {
        // zfs-user 2.4.2 + 2.4.3 -> 2.4.3. Not a removal: ZFS stays.
        let a = inv(&[("zfs-user", &["2.4.2", "2.4.3"])]);
        let b = inv(&[("zfs-user", &["2.4.3"])]);
        let d = diff_inventories(&a, &b);
        assert_eq!(d[0].kind, ChangeKind::Changed);
        assert_eq!(d[0].old, vec!["2.4.2", "2.4.3"]);
        assert_eq!(d[0].new, vec!["2.4.3"]);
    }

    #[test]
    fn genuine_adds_and_removals() {
        let a = inv(&[("gone", &["1.0"])]);
        let b = inv(&[("fresh", &["2.0"])]);
        let d = diff_inventories(&a, &b);
        let fresh = d.iter().find(|c| c.name == "fresh").unwrap();
        let gone = d.iter().find(|c| c.name == "gone").unwrap();
        assert_eq!(fresh.kind, ChangeKind::Added);
        assert_eq!(gone.kind, ChangeKind::Removed);
    }

    #[test]
    fn accepts_both_json_shapes() {
        // Newer nix wraps the map in {"derivations": …}; older nix does not.
        let wrapped =
            r#"{"derivations":{"/nix/store/x.drv":{"env":{"pname":"curl","version":"8.21.0"}}}}"#;
        let flat = r#"{"/nix/store/x.drv":{"env":{"pname":"curl","version":"8.21.0"}}}"#;
        for raw in [wrapped, flat] {
            let m = serde_json::from_str::<ShowOutput>(raw).unwrap().into_map();
            assert_eq!(m.len(), 1);
            let d = m.into_values().next().unwrap();
            assert_eq!(d.env.pname.as_deref(), Some("curl"));
            assert_eq!(d.env.version.as_deref(), Some("8.21.0"));
        }
    }

    #[test]
    fn splits_names_on_the_first_version_boundary() {
        assert_eq!(
            split_name("curl-8.20.0"),
            Some(("curl".into(), "8.20.0".into()))
        );
        // The dash inside the package name must not be mistaken for the boundary.
        assert_eq!(
            split_name("zfs-user-2.4.2"),
            Some(("zfs-user".into(), "2.4.2".into()))
        );
        assert_eq!(split_name("hello"), None);
    }

    #[test]
    fn falls_back_to_name_when_pname_is_absent() {
        // The installed curl is declared with `name` and no pname/version. Dropping it
        // hid a real upgrade behind a bogus "new package" entry.
        let raw = r#"{"derivations":{"/nix/store/a.drv":{
            "env":{"name":"curl-8.20.0"},
            "outputs":{"out":{"path":"/nix/store/x"}}
        }}}"#;
        let d = serde_json::from_str::<ShowOutput>(raw)
            .unwrap()
            .into_map()
            .into_values()
            .next()
            .unwrap();
        assert_eq!(d.package(), Some(("curl".into(), "8.20.0".into())));
    }

    #[test]
    fn fixed_output_derivations_are_sources_not_packages() {
        // `curl-8.20.0.tar.xz` is a fetch: its output carries a hash. Structural, so it
        // does not depend on recognising archive extensions.
        let raw = r#"{"derivations":{"/nix/store/b.drv":{
            "env":{"name":"curl-8.20.0.tar.xz"},
            "outputs":{"out":{"hash":"sha256-abc","method":"nar"}}
        }}}"#;
        let d = serde_json::from_str::<ShowOutput>(raw)
            .unwrap()
            .into_map()
            .into_values()
            .next()
            .unwrap();
        assert!(d.is_fetch());
        assert_eq!(d.package(), None);
    }

    #[test]
    fn separates_packages_from_sources_in_a_mixed_closure() {
        // A realistic mixture: an explicit pname package, an older name-only package, a
        // fetched source, and a nameless helper. Only the first two are packages.
        let raw = r#"{"derivations":{
            "/nix/store/a.drv":{"env":{"pname":"brave","version":"1.92.144"},
                                "outputs":{"out":{"path":"/nix/store/x"}}},
            "/nix/store/b.drv":{"env":{"name":"curl-8.20.0"},
                                "outputs":{"out":{"path":"/nix/store/y"}}},
            "/nix/store/c.drv":{"env":{"name":"curl-8.20.0.tar.xz"},
                                "outputs":{"out":{"hash":"sha256-abc","method":"nar"}}},
            "/nix/store/d.drv":{"env":{},"outputs":{"out":{"path":"/nix/store/z"}}}
        }}"#;
        let mut pkgs: Vec<_> = serde_json::from_str::<ShowOutput>(raw)
            .unwrap()
            .into_map()
            .into_values()
            .filter_map(|d| d.package())
            .collect();
        pkgs.sort();
        assert_eq!(
            pkgs,
            vec![
                ("brave".to_string(), "1.92.144".to_string()),
                ("curl".to_string(), "8.20.0".to_string()),
            ]
        );
    }
}
