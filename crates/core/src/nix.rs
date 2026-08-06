//! Thin, async wrappers around the `nix` CLI.
//!
//! Every invocation here is *evaluation-only* (no `nix build`, no realisation), so a
//! background check never downloads substitutes. The one place we intentionally realise
//! anything is `apply` (see `apply.rs`), which runs under an explicit user action.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::Path;
use tokio::process::Command;

/// Run a command, capturing stdout. Fails with stderr attached on non-zero exit.
async fn run_capture(cmd: &mut Command) -> Result<String> {
    let output = cmd
        .output()
        .await
        .with_context(|| format!("spawning {:?}", cmd.as_std().get_program()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "command {:?} failed ({}):\n{}",
            cmd.as_std().get_program(),
            output.status,
            stderr.trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Common flags applied to every `nix` invocation: enable flakes without requiring the
/// user's nix.conf to opt in, and stay offline-friendly.
fn nix_base() -> Command {
    let mut c = Command::new("nix");
    c.args([
        "--extra-experimental-features",
        "nix-command flakes",
        // Keep output machine-parseable and quiet.
        "--no-warn-dirty",
    ]);
    c
}

/// List the input names declared in a flake's lock (top-level nodes).
///
/// Uses `nix flake metadata --json` and reads `.locks.nodes.<root>.inputs`, which maps
/// each declared input name to its lock node. This is evaluation-free (just reads the
/// lock), so it is cheap and offline.
pub async fn flake_input_names(flake_dir: &Path) -> Result<Vec<String>> {
    let mut c = nix_base();
    c.args(["flake", "metadata", "--json"]);
    c.arg(flake_dir);
    let json = run_capture(&mut c).await?;
    let meta: serde_json::Value =
        serde_json::from_str(&json).context("parsing `nix flake metadata --json`")?;

    let root_key = meta
        .get("locks")
        .and_then(|l| l.get("root"))
        .and_then(|r| r.as_str())
        .unwrap_or("root");

    let inputs = meta
        .get("locks")
        .and_then(|l| l.get("nodes"))
        .and_then(|n| n.get(root_key))
        .and_then(|root| root.get("inputs"))
        .and_then(|i| i.as_object());

    let mut names: Vec<String> = match inputs {
        Some(map) => map.keys().cloned().collect(),
        None => Vec::new(),
    };
    names.sort();
    Ok(names)
}

/// Advance the given inputs in the flake located at `flake_dir`, rewriting its
/// `flake.lock` *in place*. Callers must only ever point this at a throwaway copy during
/// a check — never at the user's real repo.
///
/// With no inputs, this is a no-op (we never advance "everything" implicitly here; the
/// caller resolves the effective set first). Returns the inputs that could NOT be
/// advanced, with the reason; an empty vec means all of them advanced.
///
/// Inputs are updated ONE AT A TIME rather than in a single `nix flake update a b c`.
/// A single invocation is atomic: if any one input fails, none of them advance and the
/// whole check errors out. That is a bad trade in practice — a flake input pointing at a
/// local fork that has been moved or deleted (`error: Git repository "…" does not exist`)
/// would then hide pending nixpkgs updates behind a bare "check failed". Per-input updates
/// cost a few more nix invocations but let one broken input be reported while everything
/// else still gets checked.
pub async fn flake_update_inputs(
    flake_dir: &Path,
    inputs: &[String],
) -> Result<Vec<(String, String)>> {
    let mut failures = Vec::new();
    for input in inputs {
        let mut c = nix_base();
        c.current_dir(flake_dir);
        // `nix flake update <input>` (Nix 2.19+) advances exactly the named input.
        c.args(["flake", "update", input]);
        if let Err(e) = run_capture(&mut c).await {
            // Keep only the most specific line of nix's multi-line error for display.
            let reason = e
                .to_string()
                .lines()
                .rfind(|l| l.trim_start().starts_with("error:"))
                .unwrap_or("update failed")
                .trim()
                .trim_start_matches("error:")
                .trim()
                .to_string();
            tracing::warn!(input = %input, %reason, "could not advance flake input");
            failures.push((input.clone(), reason));
        }
    }
    Ok(failures)
}

/// A store path's basename with the `<hash>-` prefix removed, e.g. `ffmpeg-8.1.1-lib`.
pub fn strip_hash(store_path: &str) -> Option<String> {
    let base = store_path.rsplit('/').next()?;
    let rest = base.split_once('-')?.1;
    (!rest.is_empty()).then(|| rest.to_string())
}

/// The set of entries in a store path's closure, for asking "is this package here?".
///
/// Reads an already-realised or already-instantiated path, so it downloads nothing.
#[derive(Debug, Default, Clone)]
pub struct ClosureNames(Vec<String>);

impl ClosureNames {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether a package of this name is in the closure.
    ///
    /// Matching is by prefix rather than by reconstructing the package name, because
    /// deriving a name from a store path is unreliable: multi-output packages put the
    /// output AFTER the version (`ffmpeg-8.1.1-lib`, `ffmpeg-8.1.1-data`), so peeling
    /// trailing version components leaves `ffmpeg-8.1.1-lib` and never matches `ffmpeg`.
    /// That silently misclassified every multi-output package as build-time-only.
    ///
    /// The trailing `-` matters: it keeps `curl` from matching `curlftpfs-0.9`, and `go`
    /// from matching `gobject-introspection-1.2`.
    pub fn contains_package(&self, name: &str) -> bool {
        let prefix = format!("{name}-");
        self.0.iter().any(|e| e == name || e.starts_with(&prefix))
    }
}

/// Read the closure of a store path.
pub async fn closure_names(path: &str) -> ClosureNames {
    let out = Command::new("nix-store")
        .args(["-q", "--requisites", path])
        .output()
        .await;
    let Ok(out) = out else {
        tracing::warn!("could not query closure of {path}");
        return ClosureNames::default();
    };
    if !out.status.success() {
        tracing::warn!("nix-store -q --requisites {path} failed");
        return ClosureNames::default();
    }
    ClosureNames(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(strip_hash)
            .collect(),
    )
}

/// Evaluate the `.drvPath` of a host's `system.build.toplevel` for a given flake ref.
///
/// Evaluating `drvPath` *instantiates* the derivation (writes the `.drv` to the store)
/// but does NOT realise it — no outputs are built or substituted, so nothing is
/// downloaded. This is the offline signal we compare to decide "updates available".
/// With `reference_lock`, the flake is evaluated against THAT lock file instead of its own.
/// That is what lets the candidate be evaluated straight from the user's real repo — no
/// copy of the tree required — while the repo itself stays untouched.
pub async fn toplevel_drv_path(
    flake_ref: &str,
    host_attr: &str,
    reference_lock: Option<&Path>,
) -> Result<String> {
    let attr =
        format!("{flake_ref}#nixosConfigurations.{host_attr}.config.system.build.toplevel.drvPath");
    let mut c = nix_base();
    c.args(["eval", "--raw", &attr]);
    if let Some(lock) = reference_lock {
        c.arg("--reference-lock-file").arg(lock);
    }
    let out = run_capture(&mut c).await?;
    let path = out.trim().to_string();
    if path.is_empty() {
        bail!("empty drvPath evaluating {attr}");
    }
    Ok(path)
}

/// The system double this machine evaluates for, e.g. `x86_64-linux`.
///
/// `builtins.currentSystem` is impure by definition, hence the flag; the evaluation itself
/// is a single builtin and loads nothing.
pub async fn current_system_double() -> Result<String> {
    let mut c = nix_base();
    c.args([
        "eval",
        "--impure",
        "--raw",
        "--expr",
        "builtins.currentSystem",
    ]);
    let out = run_capture(&mut c).await?;
    let s = out.trim().to_string();
    if s.is_empty() {
        bail!("empty result from builtins.currentSystem");
    }
    Ok(s)
}

/// Render a Nix string literal, so a package name can never be read as syntax.
///
/// `${` is the one that matters: it opens an interpolation inside a Nix string, and a name
/// carrying it would otherwise be evaluated rather than looked up.
fn nix_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '$' => out.push_str("\\$"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// The most generated Nix source we will put in one `--apply` argument.
///
/// Linux caps a *single* argv element at `MAX_ARG_STRLEN` = 128 KiB; one byte over and the
/// spawn fails outright with `E2BIG`, which here would mean losing every changelog at once.
/// That is not a theoretical margin: a nixpkgs bump touching most of a real system closure
/// produced 4,533 names and 89 KB of expression, already two thirds of the limit.
///
/// So: high enough that such a bump is still a single evaluation, with room to spare for
/// the fixed ~283 bytes of surrounding expression and then some.
const MAX_EXPR_BYTES: usize = 96 * 1024;

/// Split names into batches whose generated expression stays within `budget`.
///
/// A name too large to fit on its own still gets its own batch rather than being dropped or
/// looping forever; nix will simply refuse that one batch.
fn batches(pnames: &[String], budget: usize) -> Vec<&[String]> {
    // Exactly what the name will occupy, escaping included, plus its separator. An estimate
    // would have to be an over-estimate, and over-estimating splits batches that fit.
    let cost = |n: &String| nix_string(n).len() + 1;
    let mut out = Vec::new();
    let (mut start, mut size) = (0, 0);
    for (i, n) in pnames.iter().enumerate() {
        if i > start && size + cost(n) > budget {
            out.push(&pnames[start..i]);
            start = i;
            size = 0;
        }
        size += cost(n);
    }
    if start < pnames.len() {
        out.push(&pnames[start..]);
    }
    out
}

/// Resolve `meta.changelog` for a set of package names against one nixpkgs.
///
/// One `nix eval` for the whole set, rather than one per name. Nixpkgs is a single lazily
/// evaluated attribute set: loading it costs the same whether one field or fifty are read
/// from it, so a process per package paid that cost N times over. It was also the worst
/// shape for the machine — nix's evaluator is single-threaded, so the old bounded fan-out
/// ran N nixpkgs loads at once (up to 8) rather than getting through them any faster.
///
/// Batching only kicks in past `MAX_EXPR_BYTES`; a realistic update is a single evaluation.
/// It also bounds the damage from an evaluation error that `changelog_expr` cannot catch:
/// one batch's links are lost instead of all of them.
///
/// Returns pname -> URL for those that have one; everything else is simply absent. Failure
/// is never fatal: changelogs are decoration, and a check that cannot resolve them still
/// reports the update correctly.
pub async fn meta_changelogs(
    nixpkgs_ref: &str,
    system: &str,
    pnames: &[String],
) -> HashMap<String, String> {
    let attr = format!("{nixpkgs_ref}#legacyPackages.{system}");
    let mut out = HashMap::new();

    for batch in batches(pnames, MAX_EXPR_BYTES) {
        let mut c = nix_base();
        // A `path:` ref can name a vendored nixpkgs or a local fork inside the user's own
        // tree, and evaluating a flake whose lock is missing or incomplete makes nix WRITE
        // one there. A check must not put a file in the user's working copy to read two
        // metadata fields. (Only here — `flake_update_inputs` exists to write locks.)
        c.args([
            "eval",
            "--json",
            "--no-write-lock-file",
            &attr,
            "--apply",
            &changelog_expr(batch),
        ]);

        let output = match c.output().await {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("could not run nix eval for changelogs: {e}");
                continue;
            }
        };
        if !output.status.success() {
            // Loud: it costs this whole batch of links, and silently missing changelogs
            // look just like nixpkgs not having any.
            tracing::warn!(
                "no changelogs for {} package(s): lookup against {nixpkgs_ref} failed: {}",
                batch.len(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
            continue;
        }

        out.extend(changelogs_from_json(&output.stdout));
    }
    out
}

/// The Nix function applied to a package set to read many `meta.changelog`s at once.
///
/// Everything here exists to keep one bad package from costing a whole batch of links,
/// since this is one evaluation shared by all of them:
///   * `or null` — the name simply isn't in this nixpkgs.
///   * `tryEval` — it is, but throws on access (aliases that error out, unfree or
///     unsupported packages). Catches `throw` and failed `assert`, and nothing else:
///     `abort`, infinite recursion and type errors still take the batch down, which is
///     what `MAX_EXPR_BYTES` batching bounds. No nixpkgs package does that in practice.
///   * `filter isString` — forces each list element INSIDE the `tryEval`, where a throwing
///     element is still catchable, and keeps only the usable ones.
///   * the type guard — `--json` cannot serialise a function or a path, and would fail the
///     whole evaluation rather than that one entry. It doubles as the forcing the values
///     need: `isString` fully evaluates a string, and `filter` forces the list's spine and
///     every element, so nothing is left for `--json` to force outside the `tryEval`.
fn changelog_expr(pnames: &[String]) -> String {
    let names: Vec<String> = pnames.iter().map(|p| nix_string(p)).collect();
    format!(
        "pkgs: builtins.listToAttrs (map (n: {{ \
         name = n; \
         value = let r = builtins.tryEval (\
         let v = pkgs.${{n}}.meta.changelog or null; \
         in if builtins.isString v then v \
         else if builtins.isList v then builtins.filter builtins.isString v \
         else null); \
         in if r.success then r.value else null; \
         }}) [ {} ])",
        names.join(" ")
    )
}

/// Map the evaluation's JSON to pname -> URL, keeping only usable entries.
fn changelogs_from_json(stdout: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let val: serde_json::Value = serde_json::from_slice(stdout).unwrap_or(serde_json::Value::Null);
    let Some(map) = val.as_object() else {
        return out;
    };
    for (pname, v) in map {
        let url = match v {
            serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
            // Some packages set changelog to a list of URLs.
            serde_json::Value::Array(a) => a
                .iter()
                .find_map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_string)),
            _ => None,
        };
        if let Some(url) = url {
            out.insert(pname.clone(), url);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{nix_string, strip_hash, ClosureNames};

    #[test]
    fn the_generated_expression_has_the_shape_we_think() {
        // It is Nix source assembled by `format!`, handed to `--apply`. A missing space or
        // an unbalanced paren is a syntax error that costs every changelog at once, and the
        // integration test is the only other thing that would catch it.
        let e = super::changelog_expr(&["firefox".to_string(), "gh".to_string()]);
        assert!(
            e.starts_with("pkgs: builtins.listToAttrs (map (n: {"),
            "{e}"
        );
        assert!(e.ends_with(r#"[ "firefox" "gh" ])"#), "{e}");
        // The guards that keep one bad package from taking the batch down.
        assert!(e.contains("builtins.tryEval"), "{e}");
        assert!(e.contains("builtins.isString"), "{e}");
        assert!(e.contains("builtins.filter builtins.isString"), "{e}");
        assert!(e.contains("or null"), "{e}");
        // `${n}` must survive `format!` as a Nix interpolation of the loop variable.
        assert!(e.contains("pkgs.${n}.meta.changelog"), "{e}");
        assert_eq!(
            e.matches('(').count(),
            e.matches(')').count(),
            "unbalanced parens: {e}"
        );
    }

    #[test]
    fn reads_the_result_shapes_and_ignores_the_useless_ones() {
        let got = super::changelogs_from_json(
            br#"{"a":"https://x/a",
                 "b":["https://x/b1","https://x/b2"],
                 "c":null,
                 "d":"",
                 "e":["", "https://x/e"],
                 "f":[],
                 "g":42}"#,
        );
        assert_eq!(got.get("a").map(String::as_str), Some("https://x/a"));
        // First usable element of a list.
        assert_eq!(got.get("b").map(String::as_str), Some("https://x/b1"));
        // An empty string is not a link, in a list or on its own.
        assert_eq!(got.get("e").map(String::as_str), Some("https://x/e"));
        for absent in ["c", "d", "f", "g"] {
            assert!(!got.contains_key(absent), "expected no entry for {absent}");
        }
    }

    /// `MAX_ARG_STRLEN`: the kernel's cap on a SINGLE argv element.
    const ARGV_MAX: usize = 128 * 1024;

    #[test]
    fn the_budget_cannot_exceed_the_argv_limit() {
        // The constant itself is the guarantee — a batch is allowed to fill it entirely,
        // and the expression adds its fixed wrapper on top. If this ever fails, every
        // changelog is lost at spawn time with E2BIG, not degraded.
        let wrapper = super::changelog_expr(&[]).len();
        assert!(
            super::MAX_EXPR_BYTES + wrapper < ARGV_MAX,
            "budget {} + wrapper {wrapper} would exceed MAX_ARG_STRLEN",
            super::MAX_EXPR_BYTES
        );
    }

    #[test]
    fn batches_stay_under_the_argv_limit() {
        // Names made entirely of characters that ESCAPE, so the rendered bytes are the
        // worst case rather than half of it — otherwise this passes on slack alone.
        let many: Vec<String> = (0..5000)
            .map(|i| format!("${}\"", "$".repeat(i % 40)))
            .collect();

        let split = super::batches(&many, super::MAX_EXPR_BYTES);
        assert!(split.len() > 1, "expected this to need splitting");
        for b in &split {
            assert!(
                super::changelog_expr(b).len() < ARGV_MAX,
                "batch expression exceeds MAX_ARG_STRLEN"
            );
        }
        // Nothing dropped, nothing duplicated, order preserved.
        let flat: Vec<&String> = split.iter().flat_map(|b| b.iter()).collect();
        assert_eq!(flat.len(), many.len());
        assert!(flat.iter().zip(many.iter()).all(|(a, b)| *a == b));
    }

    #[test]
    fn a_realistic_update_is_a_single_evaluation() {
        // The point of batching is the cliff, not routine splitting. The measured worst
        // case — a nixpkgs bump touching a whole real system closure — was 4,533 names, so
        // a set that size must still be one nix process. (These stand-in names are a little
        // longer than the real ones, which rendered to ~89 KB.)
        let many: Vec<String> = (0..4533).map(|i| format!("some-pkg-name-{i}")).collect();
        let rendered: usize = many.iter().map(|n| super::nix_string(n).len() + 1).sum();
        assert!(
            (80 * 1024..96 * 1024).contains(&rendered),
            "test set no longer models the measured worst case ({rendered} bytes)"
        );
        assert_eq!(super::batches(&many, super::MAX_EXPR_BYTES).len(), 1);
    }

    #[test]
    fn a_name_too_big_to_batch_still_gets_its_own_batch() {
        // Must not drop it and must not loop forever.
        let huge = "x".repeat(200);
        for names in [
            vec!["a".to_string(), huge.clone(), "b".to_string()],
            // Oversize FIRST: without the `i > start` guard this emits an empty leading
            // batch, i.e. a wasted nix process evaluating nothing.
            vec![huge.clone(), "a".to_string()],
        ] {
            let split = super::batches(&names, 64);
            assert_eq!(split.iter().flat_map(|b| b.iter()).count(), names.len());
            assert!(split.iter().any(|b| b.len() == 1 && b[0] == huge));
            assert!(
                split.iter().all(|b| !b.is_empty()),
                "empty batch: {split:?}"
            );
        }
    }

    #[test]
    fn no_names_means_no_evaluation() {
        assert!(super::batches(&[], super::MAX_EXPR_BYTES).is_empty());
    }

    #[test]
    fn unparseable_output_yields_no_changelogs_rather_than_panicking() {
        assert!(super::changelogs_from_json(b"not json").is_empty());
        assert!(super::changelogs_from_json(b"[]").is_empty());
        assert!(super::changelogs_from_json(b"").is_empty());
    }

    #[test]
    fn package_names_cannot_escape_into_nix_syntax() {
        assert_eq!(nix_string("firefox"), r#""firefox""#);
        // `${` opens an interpolation inside a Nix string: unescaped, this name would be
        // evaluated instead of looked up.
        assert_eq!(nix_string("${pkgs.hello}"), r#""\${pkgs.hello}""#);
        assert_eq!(nix_string(r#"a"b"#), r#""a\"b""#);
        assert_eq!(nix_string(r"a\b"), r#""a\\b""#);
    }

    fn closure(entries: &[&str]) -> ClosureNames {
        ClosureNames(entries.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn strips_the_hash_prefix() {
        assert_eq!(
            strip_hash("/nix/store/abc123-ffmpeg-8.1.1-lib").as_deref(),
            Some("ffmpeg-8.1.1-lib")
        );
        assert_eq!(strip_hash("").as_deref(), None);
        assert_eq!(strip_hash("/nix/store/nodashhere").as_deref(), None);
    }

    #[test]
    fn finds_multi_output_packages() {
        // The real shapes from a NixOS closure: the OUTPUT comes after the version, which
        // is what broke name-reconstruction and misreported ffmpeg as build-time only.
        let c = closure(&[
            "ffmpeg-8.1.1-lib",
            "ffmpeg-8.1.1-data",
            "ffmpeg-headless-8.1.1-lib",
        ]);
        assert!(c.contains_package("ffmpeg"));
        assert!(c.contains_package("ffmpeg-headless"));
    }

    #[test]
    fn finds_plain_versioned_packages() {
        let c = closure(&["curl-8.20.0", "expat-2.8.1", "zfs-user-2.4.3"]);
        assert!(c.contains_package("curl"));
        assert!(c.contains_package("expat"));
        assert!(c.contains_package("zfs-user"));
    }

    #[test]
    fn does_not_match_a_longer_unrelated_name() {
        // The trailing dash is what keeps these apart.
        let c = closure(&["curlftpfs-0.9", "gobject-introspection-1.2"]);
        assert!(!c.contains_package("curl"));
        assert!(!c.contains_package("go"));
    }

    #[test]
    fn reports_absent_packages() {
        // go is a build input; a system without Go installed has no go-* runtime path.
        let c = closure(&["brave-1.92.144", "mesa-26.1.5"]);
        assert!(!c.contains_package("go"));
        assert!(c.contains_package("brave"));
    }
}
