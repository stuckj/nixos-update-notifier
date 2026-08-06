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
//! `pname` and `version` — in its environment, or, once it sets `__structuredAttrs`, in the
//! structured metadata beside it (see `Attrs`; missing that second home is what made an
//! upgraded `bind` read as removed). Crucially, derivations that are NOT packages (sources,
//! patches, internal helpers) carry neither, so the noise filters disappear by construction
//! rather than by pattern-matching — in a real system closure, only 6,847 of 16,422
//! derivations are packages.

use crate::diff::{ChangeKind, PackageChange};
use anyhow::{bail, Context, Result};
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
    /// A `__structuredAttrs = true` derivation keeps its attributes HERE, not in `env` —
    /// see `Attrs`.
    #[serde(rename = "structuredAttrs")]
    structured_attrs: Option<Attrs>,
}

#[derive(Deserialize, Default)]
struct Env {
    pname: Option<String>,
    version: Option<String>,
    name: Option<String>,
    /// The same structured attributes, as an embedded JSON *string*. This is how the
    /// `.drv` itself stores them, and what `nix derivation show` emitted before it grew
    /// the top-level `structuredAttrs` field.
    #[serde(rename = "__json")]
    json: Option<String>,
}

/// The attributes of a `__structuredAttrs = true` derivation.
///
/// Such a derivation gets its attributes through a JSON file instead of the environment,
/// so its `env` holds nothing but the output paths — no `pname`, no `version`, not even
/// `name`. Reading only `env` therefore makes the package *disappear from the inventory*,
/// and a package that is structured on one side of a diff and not the other reads as a
/// removal: nixpkgs flipping `bind` to structured attrs reported `bind 9.20.23 -> (removed)`
/// on a machine where it was being upgraded to 9.20.26. Packages structured on both sides
/// (firefox, gh) were invisible instead — their upgrades were silently never reported.
///
/// nixpkgs is migrating packages to structured attrs steadily, so this is not a corner
/// case: 215 of ~6,800 packages in one real system closure, and climbing.
#[derive(Deserialize, Default)]
struct Attrs {
    // Unlike `env` (always string -> string), these come from arbitrary nix values, so they
    // are typed loosely and then accepted only if they turn out to be strings — nothing is
    // converted. A derivation with, say, a numeric `version` is simply not a package we can
    // name, which must not fail the parse for the whole closure either.
    pname: Option<serde_json::Value>,
    version: Option<serde_json::Value>,
}

impl Attrs {
    fn package(&self) -> Option<(String, String)> {
        let p = self.pname.as_ref()?.as_str()?;
        let v = self.version.as_ref()?.as_str()?;
        (!p.is_empty() && !v.is_empty()).then(|| (p.to_string(), v.to_string()))
    }
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

    /// `(pname, version)` from structured attributes, in whichever shape this nix reports
    /// them. Both shapes are tried: a `structuredAttrs` that carries no usable pname does
    /// not stop us reading one out of `__json`.
    fn structured_package(&self) -> Option<(String, String)> {
        if let Some(pkg) = self.structured_attrs.as_ref().and_then(Attrs::package) {
            return Some(pkg);
        }
        // Older nix leaves the attributes as embedded JSON instead. Trust `__json` only on
        // a derivation whose env looks structured — i.e. carries none of the three names —
        // so an ordinary derivation that happens to define an attribute called `__json`
        // cannot claim a pname that isn't its own. Unparseable means "not a package we can
        // name", which is already the outcome for a derivation with no metadata.
        if self.env.pname.is_some() || self.env.version.is_some() || self.env.name.is_some() {
            return None;
        }
        serde_json::from_str::<Attrs>(self.env.json.as_deref()?)
            .ok()?
            .package()
    }

    /// `(pname, version)` for a derivation that builds a package, else `None`.
    ///
    /// Prefers the explicit `pname`/`version` attributes, from `env` or — for a
    /// `__structuredAttrs` derivation, whose `env` has neither — from `Attrs`. Packages
    /// declared the older way, with just `name = "curl-8.20.0"`, carry none of them, and
    /// silently dropping them would hide real upgrades (this is exactly what happened to
    /// the installed curl). For those we split `name` on nixpkgs' own convention: the first
    /// `-` followed by a digit separates package from version.
    ///
    /// That `name` split is deliberately NOT applied to structured attributes. There, a
    /// bare `name` is the mark of a trivial builder rather than a package — 7,103 of them
    /// in one system closure, including vendored rust crates (`regex-automata-0.4.13`) and
    /// the system toplevel itself, all of which would split into plausible-looking
    /// "packages" and bury the real changes.
    fn package(&self) -> Option<(String, String)> {
        if self.is_fetch() {
            return None;
        }
        if let (Some(p), Some(v)) = (&self.env.pname, &self.env.version) {
            if !p.is_empty() && !v.is_empty() {
                return Some((p.clone(), v.clone()));
            }
        }
        if let Some(pkg) = self.structured_package() {
            return Some(pkg);
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

/// `nix derivation show` gained a `{"derivations": …, "version": N}` wrapper in newer
/// releases; older ones emit the map of derivations directly. Accept both so a nix upgrade
/// doesn't break us.
///
/// This is deserialized by hand rather than with `#[serde(untagged)]`, for two reasons.
///
/// Untagged tries each variant in turn, and `Drv` — every field optional, to tolerate the
/// metadata-less derivations that fill a closure — accepts any JSON object at all. So an
/// unrecognised wrapper would not fail: it would match the "flat" variant as a one-entry
/// map of nothing, and we would report an EMPTY inventory rather than an error. Every
/// package on the other side of the diff would then read as added or removed. An
/// inventory this code cannot understand must be loud, not empty.
///
/// Untagged also buffers: serde has to materialise the whole document into its internal
/// representation before it can try the variants, which for a system closure means holding
/// ~70 MB of JSON as a tree of `Content` in a daemon that otherwise idles in single-digit
/// MB. Dispatching on the key streams instead, and keeps only what we extract.
struct ShowOutput(HashMap<String, Drv>);

impl ShowOutput {
    fn into_map(self) -> HashMap<String, Drv> {
        self.0
    }
}

impl<'de> Deserialize<'de> for ShowOutput {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;

        impl<'de> serde::de::Visitor<'de> for V {
            type Value = ShowOutput;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("`nix derivation show` output: a map of derivations, or a {\"derivations\": …} wrapper")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<ShowOutput, M::Error> {
                let mut wrapped = None;
                let mut flat = HashMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    if key == "derivations" {
                        wrapped = Some(map.next_value::<HashMap<String, Drv>>()?);
                    } else if key.ends_with(".drv") {
                        // The older shape: every key is a derivation, named by store path
                        // (older nix) or basename (newer). Keying on the suffix is what
                        // tells a derivation entry from a wrapper field, since `Drv` itself
                        // cannot — it accepts any object.
                        flat.insert(key, map.next_value::<Drv>()?);
                    } else {
                        // A wrapper field we don't know: the format number today, whatever
                        // a later nix adds beside it tomorrow. Skipped rather than rejected
                        // — refusing these would make the next format addition fatal to
                        // every update check, which is the breakage this shape-tolerance
                        // exists to prevent.
                        map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
                // Whatever we return must be non-empty. Since unknown fields are skipped,
                // these arms are what stand between an unrecognised document and a silently
                // EMPTY inventory — which would report every package on the other side of
                // the diff as added or removed. A shape we cannot read must be loud.
                match (wrapped, flat.is_empty()) {
                    (Some(_), false) => Err(serde::de::Error::custom(
                        "both a `derivations` wrapper and loose derivation entries",
                    )),
                    (Some(d), true) if d.is_empty() => Err(serde::de::Error::custom(
                        "`derivations` is empty; a closure holds at least the derivation asked about",
                    )),
                    (Some(d), true) => Ok(ShowOutput(d)),
                    (None, false) => Ok(ShowOutput(flat)),
                    (None, true) => Err(serde::de::Error::missing_field("derivations")),
                }
            }
        }

        d.deserialize_map(V)
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
    // Deliberately NOT `?` — the child has to be reaped, and its exit status weighed
    // alongside any parse error, before either is reported. When nix fails it writes nothing
    // to stdout, so `?` here would blame the JSON ("EOF while parsing a value") for what is
    // really a failed nix invocation, and would drop the `Child` unwaited, leaving a zombie
    // in a daemon that retries on a timer. `from_reader` consumes the reader either way, so
    // the pipe is closed by the time we wait and the child cannot block writing into it.
    let parsed = serde_json::from_reader::<_, ShowOutput>(reader);

    let status = child.wait().context("waiting for nix")?;

    // `ShowOutput` has already refused an empty map: a closure holds at least the
    // derivation asked about, so emptiness there means we misread the output.
    match (status.success(), parsed) {
        (true, Ok(p)) => Ok(inventory_from(p.into_map())),
        (true, Err(e)) => Err(e).context("parsing `nix derivation show` JSON"),
        (false, Ok(_)) => bail!("`nix derivation show` failed ({status})"),
        // Both went wrong, and causation runs either way: nix failing leaves no JSON to
        // parse, while our aborting the parse closes the pipe and kills nix with SIGPIPE.
        // Name both rather than guessing — dropping either one points at the wrong thing.
        (false, Err(e)) => Err(e).context(format!(
            "`nix derivation show` failed ({status}) and its output did not parse"
        )),
    }
}

/// Reduce parsed derivations to the package inventory.
///
/// Split out from the reader so the classification can be tested against `nix derivation
/// show` JSON — the real shape, hand-written — rather than only through hand-built
/// inventories. Real nix output reaches it through the integration tests.
fn inventory_from(parsed: HashMap<String, Drv>) -> Inventory {
    let mut inv: Inventory = HashMap::new();
    for drv in parsed.into_values() {
        // Fetches (fixed-output derivations) are sources, not packages; anything without a
        // usable name isn't one either. Together these replace the whole hand-written
        // noise filter that the text-parsing approach needed.
        if let Some((pname, version)) = drv.package() {
            inv.entry(pname).or_default().insert(version);
        }
    }
    inv
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

/// Diff two inventories, refusing outright if either came back empty.
///
/// The shape checks in `ShowOutput` catch a document we cannot parse, but not one we parse
/// happily and understand nothing of: a future nix that keeps the wrapper while restructuring
/// the derivation objects themselves would yield thousands of `Drv`s and zero packages. The
/// diff would then be catastrophically wrong rather than merely incomplete — every package
/// on the other side reported as added or removed. The closures compared here always hold
/// packages: in production both sides are a NixOS system toplevel.
fn diff_checked(before: &Inventory, after: &Inventory) -> Result<Vec<PackageChange>> {
    anyhow::ensure!(
        !before.is_empty() && !after.is_empty(),
        "read no packages from a system closure ({} before, {} after) — \
         the derivation metadata is not in a shape this version understands",
        before.len(),
        after.len()
    );
    Ok(diff_inventories(before, after))
}

/// Full structured diff between two derivation closures.
pub async fn diff(current_drv: &str, candidate_drv: &str) -> Result<Vec<PackageChange>> {
    let (before, after) = tokio::try_join!(inventory(current_drv), inventory(candidate_drv))?;
    tracing::debug!(
        before = before.len(),
        after = after.len(),
        "package inventories"
    );
    diff_checked(&before, &after)
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

    fn drv(raw: &str) -> Drv {
        serde_json::from_str::<ShowOutput>(raw)
            .unwrap()
            .into_map()
            .into_values()
            .next()
            .unwrap()
    }

    #[test]
    fn reads_structured_attrs_derivations() {
        // bind, after nixpkgs flipped it to `__structuredAttrs = true`: its env holds only
        // output paths. Reading env alone dropped it from the inventory entirely, which is
        // what made an upgrade look like a removal.
        let d = drv(r#"{"derivations":{"/nix/store/a.drv":{
            "name":"bind-9.20.26",
            "env":{"out":"/nix/store/x","lib":"/nix/store/y"},
            "outputs":{"out":{"path":"/nix/store/x"},"lib":{"path":"/nix/store/y"}},
            "structuredAttrs":{"pname":"bind","version":"9.20.26","doCheck":true}
        }}}"#);
        assert_eq!(d.package(), Some(("bind".into(), "9.20.26".into())));
    }

    #[test]
    fn reads_structured_attrs_from_the_older_embedded_json() {
        // Before nix lifted them into a top-level field, the same attributes arrived as a
        // JSON string in `env.__json` — the shape the .drv itself stores.
        let d = drv(r#"{"derivations":{"/nix/store/a.drv":{
            "env":{"out":"/nix/store/x",
                   "__json":"{\"pname\":\"bind\",\"version\":\"9.20.26\"}"},
            "outputs":{"out":{"path":"/nix/store/x"}}
        }}}"#);
        assert_eq!(d.package(), Some(("bind".into(), "9.20.26".into())));
    }

    fn inventory_of(raw: &str) -> Inventory {
        inventory_from(serde_json::from_str::<ShowOutput>(raw).unwrap().into_map())
    }

    #[test]
    fn a_package_turning_structured_is_an_upgrade_not_a_removal() {
        // The reported bug, end to end, through the real parse: same package, same closure,
        // only the derivation style changed between the two nixpkgs revisions. Both sides
        // are the shapes nix actually emits, so this fails on the unfixed code (the `after`
        // inventory comes back empty and bind reads as removed) rather than passing on a
        // hand-built inventory that never exercises the fix.
        let before = inventory_of(
            r#"{"derivations":{"/nix/store/a.drv":{
                "env":{"pname":"bind","version":"9.20.23","out":"/nix/store/x"},
                "outputs":{"out":{"path":"/nix/store/x"}}
            }},"version":3}"#,
        );
        let after = inventory_of(
            r#"{"derivations":{"/nix/store/b.drv":{
                "name":"bind-9.20.26",
                "env":{"out":"/nix/store/y","lib":"/nix/store/z"},
                "outputs":{"out":{"path":"/nix/store/y"},"lib":{"path":"/nix/store/z"}},
                "structuredAttrs":{"pname":"bind","version":"9.20.26"}
            }},"version":4}"#,
        );

        let d = diff_inventories(&before, &after);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name, "bind");
        assert_eq!(d[0].kind, ChangeKind::Changed);
        assert_eq!(d[0].old, vec!["9.20.23"]);
        assert_eq!(d[0].new, vec!["9.20.26"]);
    }

    #[test]
    fn an_unrecognised_top_level_shape_errors_rather_than_reading_as_empty() {
        // `Drv` accepts any JSON object, so a wrapper we don't know must not quietly parse
        // as a map of nothing: an empty inventory makes every package on the other side of
        // the diff look added or removed.
        for raw in [
            r#"{"drvs":{"/nix/store/a.drv":{}}}"#,
            r#"{}"#,
            r#"{"version":4}"#,
            // Not a map at all.
            r#"[]"#,
            r#""derivations""#,
            r#"null"#,
            // Both shapes at once: taking one and dropping the other would lose packages.
            r#"{"derivations":{"/nix/store/a.drv":{}},"/nix/store/b.drv":{}}"#,
            // A wrapper we can read but that holds nothing. `nix derivation show -r` always
            // reports at least the derivation asked about, so this is a misread too.
            r#"{"derivations":{}}"#,
            r#"{"derivations":{},"version":4}"#,
        ] {
            assert!(
                serde_json::from_str::<ShowOutput>(raw).is_err(),
                "should not have parsed: {raw}"
            );
        }
        // A `structuredAttrs` of the wrong type is a hard error too, not a silent drop.
        assert!(serde_json::from_str::<ShowOutput>(
            r#"{"derivations":{"/nix/store/a.drv":{"structuredAttrs":"nope"}}}"#
        )
        .is_err());
    }

    #[test]
    fn unknown_wrapper_fields_are_tolerated() {
        // `version` is the format number, not a package version. It and anything a later
        // nix adds beside it must be skipped, not rejected: erroring on an unknown sibling
        // would make the next format addition fatal to every update check. Order must not
        // matter either.
        for raw in [
            r#"{"derivations":{"/nix/store/a.drv":{"env":{"pname":"curl","version":"8.21.0"}}},"version":4}"#,
            r#"{"version":4,"derivations":{"/nix/store/a.drv":{"env":{"pname":"curl","version":"8.21.0"}}}}"#,
            r#"{"version":5,"system":"x86_64-linux","derivations":{"/nix/store/a.drv":{"env":{"pname":"curl","version":"8.21.0"}}},"extra":{"nested":[1,2]}}"#,
        ] {
            let inv = inventory_of(raw);
            assert_eq!(inv.len(), 1, "parsing: {raw}");
            assert!(inv.contains_key("curl"), "parsing: {raw}");
        }
    }

    #[test]
    fn reads_the_key_shapes_both_nix_versions_emit() {
        // Newer nix keys the inner map by BASENAME; older nix used full store paths, as the
        // rest of these tests do. Structured attributes must also be read in the older
        // wrapper-less shape, not just inside `derivations`.
        let basenames = inventory_of(
            r#"{"derivations":{"i5caygjf19b887m7q2ix8590bfrn8d24-bind-9.20.26.drv":{
                "env":{"out":"/nix/store/x"},
                "outputs":{"out":{"path":"/nix/store/x"}},
                "structuredAttrs":{"pname":"bind","version":"9.20.26"}
            }},"version":4}"#,
        );
        let flat = inventory_of(
            r#"{"/nix/store/i5caygjf19b887m7q2ix8590bfrn8d24-bind-9.20.26.drv":{
                "env":{"out":"/nix/store/x"},
                "outputs":{"out":{"path":"/nix/store/x"}},
                "structuredAttrs":{"pname":"bind","version":"9.20.26"}
            }}"#,
        );
        // Also flat, but keyed by basename — the combination that actually exercises the
        // suffix dispatch, since the wrapped branch never consults it.
        let flat_basename = inventory_of(
            r#"{"i5caygjf19b887m7q2ix8590bfrn8d24-bind-9.20.26.drv":{
                "env":{"out":"/nix/store/x"},
                "outputs":{"out":{"path":"/nix/store/x"}},
                "structuredAttrs":{"pname":"bind","version":"9.20.26"}
            }}"#,
        );
        assert_eq!(basenames, flat);
        assert_eq!(basenames, flat_basename);
        assert_eq!(basenames["bind"].iter().next().unwrap(), "9.20.26");
    }

    #[test]
    fn an_empty_inventory_is_refused_rather_than_diffed() {
        // Parsing can succeed while we understand nothing of what we parsed. Diffing that
        // would report every package on the other side as added or removed. The check lives
        // in `diff_checked` — the function `diff()` actually calls — so that removing it
        // fails a test rather than quietly widening what gets reported.
        let full = inv(&[("curl", &["8.21.0"])]);
        let empty = Inventory::new();
        assert!(diff_checked(&full, &full).is_ok());
        assert!(diff_checked(&empty, &full).is_err());
        assert!(diff_checked(&full, &empty).is_err());
        assert!(diff_checked(&empty, &empty).is_err());
    }

    #[test]
    fn structured_fetches_are_still_sources_not_packages() {
        // Real ones exist (facetimehd-calibration, mbrola-voices): fixed-output AND
        // carrying pname/version in structured attrs. `is_fetch` has to win.
        let d = drv(r#"{"derivations":{"/nix/store/a.drv":{
            "name":"mbrola-voices-0-unstable-2020-03-30",
            "env":{"out":"/nix/store/x"},
            "outputs":{"out":{"hash":"sha256-abc","method":"nar"}},
            "structuredAttrs":{"pname":"mbrola-voices","version":"0-unstable-2020-03-30"}
        }}}"#);
        assert!(d.is_fetch());
        assert_eq!(d.package(), None);
    }

    #[test]
    fn half_a_structured_name_is_not_a_package() {
        for attrs in [
            r#"{"pname":"half"}"#,
            r#"{"version":"1.0"}"#,
            r#"{"pname":"","version":"1.0"}"#,
            r#"{"pname":"half","version":""}"#,
        ] {
            let raw = format!(
                r#"{{"derivations":{{"/nix/store/a.drv":{{
                    "env":{{"out":"/nix/store/x"}},
                    "outputs":{{"out":{{"path":"/nix/store/x"}}}},
                    "structuredAttrs":{attrs}
                }}}}}}"#
            );
            assert_eq!(drv(&raw).package(), None, "attrs: {attrs}");
        }
    }

    #[test]
    fn embedded_json_is_only_trusted_on_a_structured_env() {
        // An ordinary derivation that happens to define an attribute called `__json` must
        // not be able to claim a pname that isn't its own — its `name` still wins.
        let d = drv(r#"{"derivations":{"/nix/store/a.drv":{
            "env":{"name":"curl-8.20.0","__json":"{\"pname\":\"other\",\"version\":\"1.0\"}"},
            "outputs":{"out":{"path":"/nix/store/x"}}
        }}}"#);
        assert_eq!(d.package(), Some(("curl".into(), "8.20.0".into())));
    }

    #[test]
    fn a_structured_attrs_without_a_pname_falls_through_to_embedded_json() {
        let d = drv(r#"{"derivations":{"/nix/store/a.drv":{
            "env":{"out":"/nix/store/x",
                   "__json":"{\"pname\":\"bind\",\"version\":\"9.20.26\"}"},
            "outputs":{"out":{"path":"/nix/store/x"}},
            "structuredAttrs":{"doCheck":true}
        }}}"#);
        assert_eq!(d.package(), Some(("bind".into(), "9.20.26".into())));
    }

    #[test]
    fn unparseable_embedded_json_is_not_a_package() {
        let d = drv(r#"{"derivations":{"/nix/store/a.drv":{
            "env":{"out":"/nix/store/x","__json":"{not json"},
            "outputs":{"out":{"path":"/nix/store/x"}}
        }}}"#);
        assert_eq!(d.package(), None);
    }

    #[test]
    fn structured_trivial_builders_are_not_packages() {
        // A structured derivation with only a `name` is a builder, not a package. Splitting
        // it would admit thousands of vendored crates and the system toplevel itself.
        for raw in [
            r#"{"derivations":{"/nix/store/a.drv":{
                "name":"regex-automata-0.4.13",
                "env":{"out":"/nix/store/x"},
                "outputs":{"out":{"path":"/nix/store/x"}},
                "structuredAttrs":{"name":"regex-automata-0.4.13"}
            }}}"#,
            r#"{"derivations":{"/nix/store/b.drv":{
                "name":"nixos-system-host-26.05",
                "env":{"out":"/nix/store/y"},
                "outputs":{"out":{"path":"/nix/store/y"}},
                "structuredAttrs":{"name":"nixos-system-host-26.05","system":"x86_64-linux"}
            }}}"#,
        ] {
            assert_eq!(drv(raw).package(), None);
        }
    }

    #[test]
    fn odd_structured_values_do_not_break_the_parse() {
        // `structuredAttrs` holds arbitrary nix values, unlike env's string -> string. A
        // non-string version must yield "not a package", never a failed parse of the whole
        // closure.
        let d = drv(r#"{"derivations":{"/nix/store/a.drv":{
            "env":{"out":"/nix/store/x"},
            "outputs":{"out":{"path":"/nix/store/x"}},
            "structuredAttrs":{"pname":"weird","version":[1,2,3]}
        }}}"#);
        assert_eq!(d.package(), None);
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
