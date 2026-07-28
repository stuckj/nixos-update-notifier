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
}
