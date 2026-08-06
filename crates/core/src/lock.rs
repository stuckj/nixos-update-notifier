//! `flake.lock` inspection: which top-level inputs actually moved, and enforcement of the
//! pinned-input guarantee.
//!
//! This is a SAFETY net rather than a security boundary. `check.rs` already restricts
//! `nix flake update` to the configured inputs, so in normal operation an excluded input
//! cannot move. The point here is to catch the case where that logic is wrong — a bug, a
//! surprising `follows` edge, a hand-edited candidate — *before* the new lock is installed
//! and rebuilt. The motivating example is a `nixpkgs-kernel` input rev-pinned to keep a
//! working kernel + ZFS pairing: silently advancing it can leave a machine unbootable, so
//! refusing to apply is much better than discovering it at boot.
//!
//! (The privilege boundary deliberately sits elsewhere: root only runs `nixos-rebuild` and
//! never inspects or writes the lock — see the `apply` module.)

use anyhow::{Context, Result};
use serde_json::Value;

/// The `locked` entry of a top-level input, if the lock records one.
///
/// A flake.lock maps `nodes.<root>.inputs.<name>` to a node key, and that node carries the
/// resolved `locked` attrs (`rev`, `narHash`, …). An input that `follows` another is
/// recorded as an array path instead of a node-key string; those have no `locked` of their
/// own, so they are reported as `None` and simply not compared.
fn locked_of<'a>(lock: &'a Value, input: &str) -> Option<&'a Value> {
    let root_key = lock.get("root")?.as_str()?;
    let node_key = lock
        .get("nodes")?
        .get(root_key)?
        .get("inputs")?
        .get(input)?
        .as_str()?;
    lock.get("nodes")?.get(node_key)?.get("locked")
}

/// Resolve an input reference — a node key, or a `follows` path — to its node key.
///
/// A `follows` is recorded as a path rooted at the *root* node, e.g. `["hm", "nixpkgs"]`
/// meaning "whatever `hm`'s `nixpkgs` input resolves to", and each step can itself be
/// another follows. The depth guard is against a malformed or hand-edited lock: a cycle
/// here would otherwise recurse forever.
fn resolve_ref(lock: &Value, r: &Value, depth: u8) -> Option<String> {
    if depth > 16 {
        return None;
    }
    match r {
        Value::String(key) => Some(key.clone()),
        Value::Array(segments) => {
            let mut cur = lock.get("root")?.as_str()?.to_string();
            for seg in segments {
                let next = lock
                    .get("nodes")?
                    .get(&cur)?
                    .get("inputs")?
                    .get(seg.as_str()?)?;
                cur = resolve_ref(lock, next, depth + 1)?;
            }
            Some(cur)
        }
        _ => None,
    }
}

/// Percent-encode the characters that would otherwise be read as ref syntax.
///
/// A GitLab subgroup puts a `/` in the owner, and `gitlab:grp/sub/nixpkgs/<rev>` does not
/// fail — it parses as the project `grp/sub` on a *branch* named `nixpkgs/<rev>`, so the
/// wrong repository is fetched and the revision pin is gone. nix emits the encoded form
/// itself and decodes it on the way back in, so encoding is what round-trips.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Append query parameters to a flake ref that may already carry some.
fn with_params(base: &str, params: &[(&str, &str)]) -> String {
    let mut out = base.to_string();
    let mut sep = if base.contains('?') { '&' } else { '?' };
    for (k, v) in params {
        out.push(sep);
        out.push_str(k);
        out.push('=');
        out.push_str(v);
        sep = '&';
    }
    out
}

/// A flake ref for a top-level input, pinned to exactly what the lock records.
///
/// This is how changelog lookups reach the nixpkgs a pending update would actually install,
/// instead of the registry alias `nixpkgs` — which resolves to a channel tarball URL, so it
/// both downloads (a check must not) and describes a revision unrelated to either side of
/// the diff.
///
/// Unlike `locked_of`, a `follows` is resolved rather than ignored: a flake whose `nixpkgs`
/// follows another input still has one definite revision, and refusing to name it would
/// silently cost those users every changelog link.
///
/// Only inputs that can be named exactly, without a registry lookup, are handled.
/// `indirect` is a registry lookup by definition; a relative `path` resolves against the
/// process's working directory, which is not the flake's, so it would name something
/// arbitrary. Both return `None`, and the caller does without changelogs rather than
/// guessing. (An absolute `path` IS handled, though it pins no revision — it is the user's
/// own tree, and reaching it costs nothing.)
pub fn input_flake_ref(lock: &str, input: &str) -> Option<String> {
    let lock: Value = serde_json::from_str(lock).ok()?;
    let root_key = lock.get("root")?.as_str()?;
    let input_ref = lock
        .get("nodes")?
        .get(root_key)?
        .get("inputs")?
        .get(input)?;
    let node_key = resolve_ref(&lock, input_ref, 0)?;
    let locked = lock.get("nodes")?.get(&node_key)?.get("locked")?;
    let get = |k: &str| locked.get(k).and_then(Value::as_str);

    // `dir` selects a flake in a SUBDIRECTORY of the fetched tree. Dropping it silently
    // yields the repository root — a different flake, whose `legacyPackages` is not the one
    // being locked. It applies to every type below.
    let dir = get("dir");
    let nar = get("narHash");
    let mut params: Vec<(&str, &str)> = Vec::new();

    let ty = get("type")?;
    let base = match ty {
        // owner/repo/rev already determines the content; `narHash` is carried anyway so nix
        // can satisfy the ref from a store path it already has, without refetching.
        // A self-hosted instance records `host`, which must be carried or the ref silently
        // points at the public one.
        "github" | "gitlab" | "sourcehut" => {
            let b = format!(
                "{ty}:{}/{}/{}",
                encode(get("owner")?),
                encode(get("repo")?),
                encode(get("rev")?)
            );
            if let Some(h) = get("host") {
                // nix's URL form cannot express a host carrying a port: both `host=h:8443`
                // and the percent-encoded `host=h%3A8443` are rejected outright ("is not an
                // absolute path"). Only the attrset input form can say it, so there is no
                // ref to render — and no ref is better than one naming a different server.
                if !h
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
                {
                    return None;
                }
                params.push(("host", h));
            }
            b
        }
        // NO narHash here. nix's git scheme lifts only `rev`/`ref`/`shallow`/`submodules`
        // out of the query and leaves anything else IN the URL, so a narHash would end up
        // part of the address handed to `git fetch` — a remote that does not exist. It
        // would not even buy anything: unlike a forge ref, the hash is not checked there.
        "git" => {
            params.push(("rev", get("rev")?));
            format!("git+{}", get("url")?)
        }
        // Not content-addressed by its URL, so the hash is what pins it — hence `narHash?`
        // rather than the optional treatment it gets above.
        "tarball" => {
            params.push(("narHash", get("narHash")?));
            get("url")?.to_string()
        }
        // A plain file is NOT a tarball to nix: an unprefixed URL parses as `tarball`, so
        // the scheme has to be spelled out or the ref changes type.
        "file" => {
            params.push(("narHash", get("narHash")?));
            format!("file+{}", get("url")?)
        }
        // Also no narHash: a local checkout is expected to drift, and nix treats a
        // mismatch as a hard error. An input pinned in `exclude_inputs` is never re-locked,
        // so one edit would fail every check from then on — for a lookup that is decoration.
        "path" => {
            let p = get("path")?;
            if !p.starts_with('/') {
                return None;
            }
            // Unlike git/tarball/file, whose locks store an already-encoded URL, a `path`
            // lock stores the path DECODED. A directory with a space then renders a ref nix
            // refuses outright, and one containing `?` would silently truncate to a
            // different directory — so each segment is encoded, the separators are not.
            let encoded: Vec<String> = p.split('/').map(encode).collect();
            format!("path:{}", encoded.join("/"))
        }
        _ => return None,
    };

    // Only where nix both honours it and it cannot backfire: a forge ref is content-
    // addressed by rev, so the hash merely lets nix reuse a store path it already has.
    // (tarball/file pushed theirs above, as the thing that pins them at all.)
    if matches!(ty, "github" | "gitlab" | "sourcehut") {
        if let Some(h) = nar {
            params.push(("narHash", h));
        }
    }
    // Encoded for the same reason as `path`: a lock stores `dir` decoded, and an `&` in a
    // directory name would end the parameter early — silently selecting a different
    // subdirectory rather than failing.
    let dir = dir.map(encode);
    if let Some(d) = &dir {
        params.push(("dir", d));
    }
    Some(with_params(&base, &params))
}

/// Names of the top-level inputs declared in a lock.
fn input_names(lock: &Value) -> Vec<String> {
    let Some(root_key) = lock.get("root").and_then(Value::as_str) else {
        return Vec::new();
    };
    lock.get("nodes")
        .and_then(|n| n.get(root_key))
        .and_then(|r| r.get("inputs"))
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// Top-level inputs whose locked revision differs between two lock files, sorted.
///
/// Inputs present in only one of the two are also reported as changed.
pub fn changed_inputs(before: &str, after: &str) -> Result<Vec<String>> {
    let before: Value = serde_json::from_str(before).context("parsing current flake.lock")?;
    let after: Value = serde_json::from_str(after).context("parsing candidate flake.lock")?;

    let mut names: Vec<String> = input_names(&before);
    for n in input_names(&after) {
        if !names.contains(&n) {
            names.push(n);
        }
    }

    let mut changed: Vec<String> = names
        .into_iter()
        .filter(|name| locked_of(&before, name) != locked_of(&after, name))
        .collect();
    changed.sort();
    Ok(changed)
}

/// Refuse the candidate if any excluded (pinned) input moved.
///
/// Returns the list of inputs that did change, so the caller can log what will be applied.
pub fn ensure_pinned_unchanged(
    before: &str,
    after: &str,
    excluded: &[String],
) -> Result<Vec<String>> {
    let changed = changed_inputs(before, after)?;

    let violated: Vec<&String> = excluded.iter().filter(|e| changed.contains(e)).collect();
    anyhow::ensure!(
        violated.is_empty(),
        "refusing to apply: excluded input(s) {:?} would be advanced by this candidate lock. \
         These are pinned in exclude_inputs and must never move; this indicates a bug in the \
         check path or a hand-edited lock.",
        violated
    );

    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal but realistically-shaped lock.
    fn lock(entries: &[(&str, &str)]) -> String {
        let inputs: Vec<String> = entries
            .iter()
            .map(|(name, _)| format!(r#""{name}": "{name}""#))
            .collect();
        let nodes: Vec<String> = entries
            .iter()
            .map(|(name, rev)| {
                format!(
                    r#""{name}": {{ "locked": {{ "type": "github", "owner": "o", "repo": "{name}", "rev": "{rev}", "narHash": "sha256-{rev}" }} }}"#
                )
            })
            .collect();
        format!(
            r#"{{ "version": 7, "root": "root",
                  "nodes": {{ "root": {{ "inputs": {{ {} }} }}, {} }} }}"#,
            inputs.join(", "),
            nodes.join(", ")
        )
    }

    #[test]
    fn detects_a_changed_input() {
        let a = lock(&[("nixpkgs", "aaa"), ("home-manager", "bbb")]);
        let b = lock(&[("nixpkgs", "ccc"), ("home-manager", "bbb")]);
        assert_eq!(changed_inputs(&a, &b).unwrap(), vec!["nixpkgs"]);
    }

    #[test]
    fn reports_nothing_when_identical() {
        let a = lock(&[("nixpkgs", "aaa"), ("nixpkgs-kernel", "pinned")]);
        assert!(changed_inputs(&a, &a).unwrap().is_empty());
    }

    #[test]
    fn added_and_removed_inputs_count_as_changed() {
        let a = lock(&[("nixpkgs", "aaa")]);
        let b = lock(&[("nixpkgs", "aaa"), ("disko", "ddd")]);
        assert_eq!(changed_inputs(&a, &b).unwrap(), vec!["disko"]);
        // …and in the other direction.
        assert_eq!(changed_inputs(&b, &a).unwrap(), vec!["disko"]);
    }

    #[test]
    fn allows_a_candidate_that_leaves_the_pin_alone() {
        let a = lock(&[("nixpkgs", "aaa"), ("nixpkgs-kernel", "pinned")]);
        let b = lock(&[("nixpkgs", "ccc"), ("nixpkgs-kernel", "pinned")]);
        let changed = ensure_pinned_unchanged(&a, &b, &["nixpkgs-kernel".to_string()]).unwrap();
        assert_eq!(changed, vec!["nixpkgs"]);
    }

    #[test]
    fn refuses_a_candidate_that_moves_the_pin() {
        // The ZFS-on-root scenario: the kernel input must never advance.
        let a = lock(&[("nixpkgs", "aaa"), ("nixpkgs-kernel", "pinned")]);
        let b = lock(&[("nixpkgs", "aaa"), ("nixpkgs-kernel", "MOVED")]);
        let err = ensure_pinned_unchanged(&a, &b, &["nixpkgs-kernel".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("nixpkgs-kernel"), "unexpected error: {err}");
        assert!(err.contains("refusing to apply"), "unexpected error: {err}");
    }

    #[test]
    fn narhash_only_change_is_still_a_change() {
        let a = r#"{"version":7,"root":"root","nodes":{"root":{"inputs":{"x":"x"}},
                    "x":{"locked":{"rev":"r","narHash":"sha256-one"}}}}"#;
        let b = r#"{"version":7,"root":"root","nodes":{"root":{"inputs":{"x":"x"}},
                    "x":{"locked":{"rev":"r","narHash":"sha256-two"}}}}"#;
        assert_eq!(changed_inputs(a, b).unwrap(), vec!["x"]);
    }

    #[test]
    fn follows_inputs_do_not_panic() {
        // `nixpkgs` here is a follows path (array), which has no `locked` of its own.
        let a = r#"{"version":7,"root":"root","nodes":{"root":{"inputs":{"hm":"hm","nixpkgs":["hm","nixpkgs"]}},
                    "hm":{"locked":{"rev":"a"}}}}"#;
        let b = r#"{"version":7,"root":"root","nodes":{"root":{"inputs":{"hm":"hm","nixpkgs":["hm","nixpkgs"]}},
                    "hm":{"locked":{"rev":"b"}}}}"#;
        assert_eq!(changed_inputs(a, b).unwrap(), vec!["hm"]);
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        assert!(changed_inputs("{not json", "{}").is_err());
    }

    /// A lock with one input whose `locked` attrs are given verbatim.
    fn locked_lock(attrs: &str) -> String {
        format!(
            r#"{{ "version": 7, "root": "root", "nodes": {{
                 "root": {{ "inputs": {{ "nixpkgs": "nixpkgs" }} }},
                 "nixpkgs": {{ "locked": {attrs} }} }} }}"#
        )
    }

    #[test]
    fn renders_a_locked_ref_for_the_common_forge_types() {
        // `narHash` is carried so nix can satisfy the ref from a store path it already has.
        let github = locked_lock(
            r#"{"type":"github","owner":"NixOS","repo":"nixpkgs","rev":"abc123","narHash":"sha256-x"}"#,
        );
        assert_eq!(
            input_flake_ref(&github, "nixpkgs").as_deref(),
            Some("github:NixOS/nixpkgs/abc123?narHash=sha256-x")
        );

        // A self-hosted instance: dropping `host` would silently point at the public one.
        let selfhosted = locked_lock(
            r#"{"type":"gitlab","owner":"grp","repo":"nixpkgs","rev":"def456","host":"gitlab.example.com"}"#,
        );
        assert_eq!(
            input_flake_ref(&selfhosted, "nixpkgs").as_deref(),
            Some("gitlab:grp/nixpkgs/def456?host=gitlab.example.com")
        );
    }

    #[test]
    fn encodes_what_would_otherwise_be_read_as_ref_syntax() {
        // A GitLab subgroup puts a `/` in the owner. Unencoded this does not fail — it
        // parses as project `grp/sub` on a BRANCH named `nixpkgs/<rev>`, so the wrong repo
        // is fetched and the pin is gone. (The integration test proves that against nix
        // itself; this one keeps a revert from passing a plain `cargo test`.)
        let sub = locked_lock(
            r#"{"type":"gitlab","owner":"grp/sub","repo":"nixpkgs","rev":"abc","host":"gl.example.com"}"#,
        );
        assert_eq!(
            input_flake_ref(&sub, "nixpkgs").as_deref(),
            Some("gitlab:grp%2Fsub/nixpkgs/abc?host=gl.example.com")
        );

        // A host carrying a port cannot be written as a URL-form ref at all — nix rejects
        // both the raw and the percent-encoded form — so there must be no ref rather than
        // one naming a different server.
        let ported = locked_lock(
            r#"{"type":"gitlab","owner":"grp","repo":"nixpkgs","rev":"abc","host":"gl.example.com:8443"}"#,
        );
        assert_eq!(input_flake_ref(&ported, "nixpkgs"), None);

        // A `path` lock stores its path decoded, so a space has to be encoded on the way
        // out; the separators must not be.
        let spaced = locked_lock(r#"{"type":"path","path":"/home/u/My Configs/nixpkgs"}"#);
        assert_eq!(
            input_flake_ref(&spaced, "nixpkgs").as_deref(),
            Some("path:/home/u/My%20Configs/nixpkgs")
        );
    }

    #[test]
    fn carries_the_subdirectory_a_flake_lives_in() {
        // Dropping `dir` yields the repository ROOT — a different flake, whose
        // `legacyPackages` is not the one that was locked.
        let sub = locked_lock(
            r#"{"type":"github","owner":"o","repo":"r","rev":"abc","dir":"nixpkgs-subdir"}"#,
        );
        assert_eq!(
            input_flake_ref(&sub, "nixpkgs").as_deref(),
            Some("github:o/r/abc?dir=nixpkgs-subdir")
        );
    }

    #[test]
    fn a_file_input_keeps_its_scheme() {
        // An unprefixed URL parses as `tarball`; only `file+` keeps the type it was locked
        // with.
        let f =
            locked_lock(r#"{"type":"file","url":"https://ex.com/x.json","narHash":"sha256-z"}"#);
        assert_eq!(
            input_flake_ref(&f, "nixpkgs").as_deref(),
            Some("file+https://ex.com/x.json?narHash=sha256-z")
        );
    }

    #[test]
    fn resolves_a_follows_to_the_input_it_points_at() {
        // A root `nixpkgs` that follows another input still has one definite revision;
        // refusing to name it would cost those users every changelog link.
        let lock = r#"{"version":7,"root":"root","nodes":{
            "root":{"inputs":{"hm":"hm","nixpkgs":["hm","nixpkgs"]}},
            "hm":{"inputs":{"nixpkgs":"nixpkgs_2"},
                  "locked":{"type":"github","owner":"nix-community","repo":"home-manager","rev":"hhh"}},
            "nixpkgs_2":{"locked":{"type":"github","owner":"NixOS","repo":"nixpkgs","rev":"nnn"}}}}"#;
        assert_eq!(
            input_flake_ref(lock, "nixpkgs").as_deref(),
            Some("github:NixOS/nixpkgs/nnn")
        );
    }

    #[test]
    fn a_cyclic_follows_terminates() {
        // Only reachable from a malformed or hand-edited lock, but it must not recurse
        // forever.
        let lock = r#"{"version":7,"root":"root","nodes":{
            "root":{"inputs":{"a":["b","a"],"b":["a","b"]}}}}"#;
        assert_eq!(input_flake_ref(lock, "a"), None);
    }

    #[test]
    fn renders_git_tarball_and_path_refs() {
        let git = locked_lock(r#"{"type":"git","url":"https://ex.com/n.git","rev":"aaa"}"#);
        assert_eq!(
            input_flake_ref(&git, "nixpkgs").as_deref(),
            Some("git+https://ex.com/n.git?rev=aaa")
        );

        // An existing query string must be extended, not broken with a second `?`.
        let git_q =
            locked_lock(r#"{"type":"git","url":"https://ex.com/n.git?ref=main","rev":"aaa"}"#);
        assert_eq!(
            input_flake_ref(&git_q, "nixpkgs").as_deref(),
            Some("git+https://ex.com/n.git?ref=main&rev=aaa")
        );

        // A tarball is not content-addressed by its URL, so the hash is what pins it.
        let tarball = locked_lock(
            r#"{"type":"tarball","url":"https://ex.com/n.tar.xz","narHash":"sha256-y"}"#,
        );
        assert_eq!(
            input_flake_ref(&tarball, "nixpkgs").as_deref(),
            Some("https://ex.com/n.tar.xz?narHash=sha256-y")
        );

        let path = locked_lock(r#"{"type":"path","path":"/nix/store/abc-source"}"#);
        assert_eq!(
            input_flake_ref(&path, "nixpkgs").as_deref(),
            Some("path:/nix/store/abc-source")
        );

        // A RELATIVE path resolves against the process's working directory, which is not
        // the flake's — the ref would name something arbitrary, so there is none to give.
        let relative = locked_lock(r#"{"type":"path","path":"./vendored-nixpkgs"}"#);
        assert_eq!(input_flake_ref(&relative, "nixpkgs"), None);
    }

    #[test]
    fn refuses_to_invent_a_ref_it_cannot_pin() {
        // `indirect` is a registry lookup — unpinned by definition, and resolving it is
        // exactly the download this exists to avoid.
        let indirect = locked_lock(r#"{"type":"indirect","id":"nixpkgs"}"#);
        assert_eq!(input_flake_ref(&indirect, "nixpkgs"), None);

        // Missing the field that pins it.
        let no_rev = locked_lock(r#"{"type":"github","owner":"NixOS","repo":"nixpkgs"}"#);
        assert_eq!(input_flake_ref(&no_rev, "nixpkgs"), None);

        let unknown = locked_lock(r#"{"type":"mercurial","url":"https://ex.com/n"}"#);
        assert_eq!(input_flake_ref(&unknown, "nixpkgs"), None);
    }

    #[test]
    fn missing_input_and_bad_lock_yield_none() {
        let l = locked_lock(r#"{"type":"github","owner":"NixOS","repo":"nixpkgs","rev":"abc"}"#);
        assert_eq!(input_flake_ref(&l, "not-an-input"), None);
        assert_eq!(input_flake_ref("{not json", "nixpkgs"), None);
        // A `follows` input has no `locked` of its own.
        let follows = r#"{"version":7,"root":"root","nodes":{"root":{"inputs":{"nixpkgs":["hm","nixpkgs"]}},
                          "hm":{"locked":{"type":"github","owner":"o","repo":"hm","rev":"r"}}}}"#;
        assert_eq!(input_flake_ref(follows, "nixpkgs"), None);
    }
}
