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
pub async fn toplevel_drv_path(flake_ref: &str, host_attr: &str) -> Result<String> {
    let attr =
        format!("{flake_ref}#nixosConfigurations.{host_attr}.config.system.build.toplevel.drvPath");
    let mut c = nix_base();
    c.args(["eval", "--raw", &attr]);
    let out = run_capture(&mut c).await?;
    let path = out.trim().to_string();
    if path.is_empty() {
        bail!("empty drvPath evaluating {attr}");
    }
    Ok(path)
}

/// Look up `meta.changelog` for a package attribute in a given nixpkgs ref.
///
/// Returns `Ok(None)` when the attribute exists but has no changelog (common in
/// nixpkgs), and `Err` only for hard failures. The attribute path is `<pname>` at the
/// top level of the nixpkgs ref, e.g. `nixpkgs#firefox.meta.changelog`.
pub async fn meta_changelog(nixpkgs_ref: &str, pname: &str) -> Result<Option<String>> {
    let attr = format!("{nixpkgs_ref}#{pname}.meta.changelog");
    let mut c = nix_base();
    // `--json` so we can distinguish null / missing from a real string.
    c.args(["eval", "--json", &attr]);
    let output = c
        .output()
        .await
        .context("spawning nix eval for meta.changelog")?;
    if !output.status.success() {
        // Attribute doesn't exist / has no meta — treat as "no changelog", not fatal.
        return Ok(None);
    }
    let val: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or(serde_json::Value::Null);
    match val {
        serde_json::Value::String(s) if !s.is_empty() => Ok(Some(s)),
        // Some packages set changelog to a list of URLs.
        serde_json::Value::Array(a) => {
            Ok(a.into_iter().find_map(|v| v.as_str().map(str::to_string)))
        }
        _ => Ok(None),
    }
}

/// Resolve `meta.changelog` for many package names in one batch to amortise nix startup.
///
/// Runs lookups concurrently (bounded) and returns a map of pname -> changelog URL for
/// those that have one. Missing/absent entries are simply omitted.
pub async fn meta_changelogs(nixpkgs_ref: &str, pnames: &[String]) -> HashMap<String, String> {
    use tokio::task::JoinSet;

    let mut set = JoinSet::new();
    // Bound concurrency so we don't spawn hundreds of `nix eval` processes at once.
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(8));

    for pname in pnames {
        let pname = pname.clone();
        let nixpkgs_ref = nixpkgs_ref.to_string();
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = sem.acquire_owned().await.ok()?;
            match meta_changelog(&nixpkgs_ref, &pname).await {
                Ok(Some(url)) => Some((pname, url)),
                _ => None,
            }
        });
    }

    let mut out = HashMap::new();
    while let Some(res) = set.join_next().await {
        if let Ok(Some((pname, url))) = res {
            out.insert(pname, url);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{strip_hash, ClosureNames};

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
