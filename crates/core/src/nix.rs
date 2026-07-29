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

/// Package name of a store path, with the hash prefix and any trailing version stripped.
///
/// `/nix/store/<hash>-zfs-user-2.4.3` -> `zfs-user`, `…-go-1.24.13` -> `go`. Returns
/// `None` for paths that don't have the expected shape.
pub fn package_name_of(store_path: &str) -> Option<String> {
    let base = store_path.rsplit('/').next()?;
    // Drop the `<hash>-` prefix.
    let rest = base.split_once('-')?.1;
    let mut parts: Vec<&str> = rest.split('-').collect();
    // Peel trailing version-ish components ("2.4.3", "1.24.13", "7.0.14").
    while parts.len() > 1 {
        let last = parts[parts.len() - 1];
        if last.starts_with(|c: char| c.is_ascii_digit()) {
            parts.pop();
        } else {
            break;
        }
    }
    let name = parts.join("-");
    (!name.is_empty()).then_some(name)
}

/// Package names present in the closure of a store path.
///
/// Used to tell apart changes that affect software actually installed on the running
/// system from build-time-only churn, and to spot a "removed" package that is really just
/// superseded by another version of the same name. Reads an already-realised or
/// already-instantiated path, so it downloads nothing.
pub async fn closure_package_names(path: &str) -> std::collections::HashSet<String> {
    let out = Command::new("nix-store")
        .args(["-q", "--requisites", path])
        .output()
        .await;
    let Ok(out) = out else {
        tracing::warn!("could not query closure of {path}");
        return Default::default();
    };
    if !out.status.success() {
        tracing::warn!("nix-store -q --requisites {path} failed");
        return Default::default();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(package_name_of)
        .collect()
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
    use super::package_name_of;

    #[test]
    fn strips_hash_and_version() {
        // Real shapes seen in a NixOS system closure.
        assert_eq!(
            package_name_of("/nix/store/abc123-zfs-user-2.4.3").as_deref(),
            Some("zfs-user")
        );
        assert_eq!(
            package_name_of("/nix/store/abc123-go-1.24.13").as_deref(),
            Some("go")
        );
        assert_eq!(
            package_name_of("/nix/store/abc123-zfs-kernel-2.4.3-7.0.14").as_deref(),
            Some("zfs-kernel")
        );
    }

    #[test]
    fn keeps_names_that_merely_contain_digits() {
        // The trailing component must *start* with a digit to count as a version.
        assert_eq!(
            package_name_of("/nix/store/abc123-python3").as_deref(),
            Some("python3")
        );
        assert_eq!(
            package_name_of("/nix/store/abc123-nixos-system-nixos-x1").as_deref(),
            Some("nixos-system-nixos-x1")
        );
    }

    #[test]
    fn handles_odd_input_without_panicking() {
        assert_eq!(package_name_of("").as_deref(), None);
        assert_eq!(package_name_of("/nix/store/nodashhere").as_deref(), None);
    }
}
