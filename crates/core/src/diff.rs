//! Parse `nix store diff-closures` output into structured package changes.
//!
//! We run `nix store diff-closures <current.drv> <candidate.drv>` over *derivation*
//! paths (not realised outputs), which walks the derivation closures and never downloads
//! a substitute. The trade-offs — validated against real output — are:
//!   * every node in a derivation closure is a `.drv`, so genuine upgrades appear as
//!     `aws-c-http: 0.10.4.drv → 0.11.0.drv` (we strip the `.drv`);
//!   * the closure also contains source tarballs, patch files and CVE-named artefacts
//!     whose name/version heuristic is mangled (`CVE: 2026-…​.patch → …`), plus toolchain
//!     / bootstrap churn — all of which we filter out;
//!   * the output is ANSI-coloured even when redirected, and uses two empty markers:
//!     `∅` (U+2205, absent from the closure) and `ε` (U+03B5, present but versionless).
//!
//! After stripping/filtering, what remains is a clean, useful `name: old -> new` list.
//! For a pristine *runtime* list (no build-time deps, no toolchain), the caller can opt
//! into the exact path (`diff_exact`), which realises the candidate — and therefore
//! downloads — and is never run in the background.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

const ARROW: char = '\u{2192}'; // →
const ABSENT: char = '\u{2205}'; // ∅  (not in closure)
const VERSIONLESS: char = '\u{03b5}'; // ε  (in closure, no version)

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Removed,
    /// Version changed (upgrade or downgrade); `old`/`new` both non-empty.
    Changed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageChange {
    /// Derivation name as reported by diff-closures (also used as a best-effort nixpkgs
    /// attr for changelog lookup).
    pub name: String,
    pub old: Vec<String>,
    pub new: Vec<String>,
    pub kind: ChangeKind,
    /// Human size delta string if diff-closures reported one (e.g. `-1.2 MiB`).
    pub size_delta: Option<String>,
    /// Filled in later by the changelog resolver; `None` until then / if unavailable.
    pub changelog: Option<String>,
}

impl PackageChange {
    /// One-line `name: old -> new` rendering for notifications / plain output.
    pub fn render_line(&self) -> String {
        format!("{}: {}", self.name, self.render_line_versions())
    }

    /// Just the `old -> new` version transition, without the package name.
    pub fn render_line_versions(&self) -> String {
        let old = join_versions(&self.old);
        let new = join_versions(&self.new);
        match self.kind {
            ChangeKind::Added => format!("(new) -> {new}"),
            ChangeKind::Removed => format!("{old} -> (removed)"),
            ChangeKind::Changed => format!("{old} -> {new}"),
        }
    }

    /// Whether this looks like a real, user-relevant package change rather than
    /// source/patch/toolchain closure noise.
    fn is_meaningful(&self) -> bool {
        if is_noise_name(&self.name) {
            return false;
        }
        match self.kind {
            // Require a version-like token on both sides — this is what separates
            // `firefox: 152.0.5 -> 152.0.6` from source-tarball churn.
            ChangeKind::Changed => {
                self.old.iter().any(|v| looks_like_version(v))
                    && self.new.iter().any(|v| looks_like_version(v))
            }
            ChangeKind::Added => self.new.iter().any(|v| looks_like_version(v)),
            ChangeKind::Removed => self.old.iter().any(|v| looks_like_version(v)),
        }
    }
}

fn join_versions(v: &[String]) -> String {
    if v.is_empty() {
        "∅".to_string()
    } else {
        v.join(", ")
    }
}

/// Run diff-closures over two store paths (`.drv` paths for the no-download path), parse,
/// and filter to meaningful changes. Returns changes sorted by name.
pub async fn diff_closures(current: &str, candidate: &str) -> Result<Vec<PackageChange>> {
    let text = run_diff_closures(current, candidate).await?;
    let mut changes = parse_diff_closures(&text);
    changes.retain(PackageChange::is_meaningful);
    Ok(changes)
}

async fn run_diff_closures(current: &str, candidate: &str) -> Result<String> {
    let output = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "store",
            "diff-closures",
            current,
            candidate,
        ])
        .output()
        .await
        .context("spawning `nix store diff-closures`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("`nix store diff-closures` failed:\n{}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Exact runtime diff: realise the candidate system output and diff *output* closures
/// against the running system. This DOWNLOADS substitutes and must only be invoked from
/// an explicit user action — never a background check. `candidate_drv` is the toplevel
/// `.drvPath`; we build it and diff the running system against the built output.
pub async fn diff_exact(candidate_drv: &str) -> Result<Vec<PackageChange>> {
    // Build the candidate toplevel (downloads). `^*` selects all outputs of the drv.
    let out = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "build",
            "--no-link",
            "--print-out-paths",
            &format!("{candidate_drv}^*"),
        ])
        .output()
        .await
        .context("spawning `nix build` for exact diff")?;
    anyhow::ensure!(
        out.status.success(),
        "building candidate for exact diff failed:\n{}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let candidate_out = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    anyhow::ensure!(
        !candidate_out.is_empty(),
        "no output path from candidate build"
    );

    // Output closures give the clean runtime list; the same parser applies (no `.drv`).
    let text = run_diff_closures("/run/current-system", &candidate_out).await?;
    let mut changes = parse_diff_closures(&text);
    changes.retain(PackageChange::is_meaningful);
    Ok(changes)
}

/// Pure parser — unit-tested against representative real output. Never panics; entries it
/// can't understand are skipped. Does NOT filter noise (see `is_meaningful`).
pub fn parse_diff_closures(text: &str) -> Vec<PackageChange> {
    let mut changes = Vec::new();

    for raw in text.lines() {
        let line = strip_ansi(raw);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, rest)) = line.split_once(": ") else {
            continue;
        };
        let name = name.trim().to_string();
        if name.is_empty() {
            continue;
        }

        let (versions_part, size_delta) = split_size_delta(rest);

        let normalised = versions_part.replace("->", &ARROW.to_string());
        let Some((before, after)) = normalised.split_once(ARROW) else {
            // No arrow → pure size churn (unchanged version); not a version change.
            continue;
        };

        let old = parse_version_set(before);
        let new = parse_version_set(after);

        let kind = match (old.is_empty(), new.is_empty()) {
            (true, true) => continue,
            (true, false) => ChangeKind::Added,
            (false, true) => ChangeKind::Removed,
            (false, false) => ChangeKind::Changed,
        };

        changes.push(PackageChange {
            name,
            old,
            new,
            kind,
            size_delta,
            changelog: None,
        });
    }

    changes.sort_by(|a, b| a.name.cmp(&b.name));
    changes.dedup();
    changes
}

/// Remove ANSI SGR escape sequences (`\x1b[…m`). diff-closures colours its output even
/// when stdout is not a TTY, so this is mandatory.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip until the final byte of the sequence (a letter, typically 'm').
            for e in chars.by_ref() {
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Peel a trailing size delta off the versions portion. Sizes look like `-1.2 MiB`,
/// `29.3 KiB` (positive deltas carry NO sign in real output), optionally preceded by
/// `, `. Returns `(versions, Some(size))` or `(whole, None)`.
fn split_size_delta(rest: &str) -> (&str, Option<String>) {
    let trimmed = rest.trim_end();
    let is_unit = |t: &str| {
        matches!(
            t,
            "B" | "KiB" | "MiB" | "GiB" | "TiB" | "PiB" | "bytes" | "byte"
        )
    };
    // The size delta, when present, is the final "<number> <unit>" token pair.
    let mut it = trimmed.rsplitn(2, ' ');
    let unit = it.next().unwrap_or("");
    let head = it.next().unwrap_or("");
    if is_unit(unit) {
        if let Some(num_start) = head.rfind([' ', ',']) {
            let num = head[num_start + 1..].trim();
            let signed_or_digit = num
                .chars()
                .next()
                .map(|c| c.is_ascii_digit() || c == '+' || c == '-')
                .unwrap_or(false);
            if !num.is_empty() && signed_or_digit {
                let versions = head[..num_start].trim_end().trim_end_matches(',');
                return (versions.trim_end(), Some(format!("{num} {unit}")));
            }
        }
    }
    (trimmed, None)
}

/// Parse a comma-separated version set, stripping `.drv` suffixes, dropping the empty
/// markers `∅` / `ε`, and discarding source-artefact tokens (tarballs/patches) that
/// diff-closures sometimes lists alongside the real version (e.g. `1.8.2, 1.8.2.tar.gz`).
fn parse_version_set(s: &str) -> Vec<String> {
    s.split(',')
        .map(|v| v.trim().trim_end_matches(".drv").trim())
        .filter(|v| {
            !v.is_empty()
                && *v != ABSENT.to_string()
                && *v != VERSIONLESS.to_string()
                && !is_artefact_token(v)
        })
        .map(String::from)
        .collect()
}

/// A token that is a source/patch/archive artefact rather than a real version string.
fn is_artefact_token(t: &str) -> bool {
    const BAD: [&str; 9] = [
        ".patch", ".diff", ".tar", ".tgz", ".zip", ".xz", ".gz", ".bz2", ".zst",
    ];
    BAD.iter().any(|ext| t.contains(ext))
}

/// Heuristic: does a token look like a real version (not a source/patch filename)?
fn looks_like_version(tok: &str) -> bool {
    let t = tok.trim();
    t.chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
        && !is_artefact_token(t)
}

/// Names that are closure noise rather than user-facing packages.
fn is_noise_name(name: &str) -> bool {
    let n = name;
    // Source/patch/archive artefacts carried in the derivation closure.
    if is_artefact_token(n) {
        return true;
    }
    // Mangled CVE-patch grouping.
    if n == "CVE" {
        return true;
    }
    // Toolchain bootstrap stages (pure build-time churn).
    if n.starts_with("bootstrap-") {
        return true;
    }
    // The system toplevel itself is not a "package".
    if n.starts_with("nixos-system-") {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- pure format parsing (colours, ε/∅, .drv, sizes) ----

    #[test]
    fn strips_ansi_and_parses_real_upgrade() {
        // As emitted by nix (coloured), .drv-suffixed versions.
        let out = "aws-c-http: 0.10.4.drv \u{2192} 0.11.0.drv\n";
        let c = parse_diff_closures(out);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "aws-c-http");
        assert_eq!(c[0].old, vec!["0.10.4"]);
        assert_eq!(c[0].new, vec!["0.11.0"]);
        assert_eq!(c[0].kind, ChangeKind::Changed);
        assert!(c[0].is_meaningful());
    }

    #[test]
    fn parses_colored_size_without_sign() {
        // Real line: positive size delta carries no '+' and is wrapped in colour codes.
        let out = "binutils: 2.44.drv \u{2192} 2.45.drv, \u{1b}[31;1m11.1 KiB\u{1b}[0m\n";
        let c = parse_diff_closures(out);
        assert_eq!(c[0].name, "binutils");
        assert_eq!(c[0].old, vec!["2.44"]);
        assert_eq!(c[0].new, vec!["2.45"]);
        assert_eq!(c[0].size_delta.as_deref(), Some("11.1 KiB"));
    }

    #[test]
    fn parses_negative_size() {
        let out = "foo: 1.0.drv \u{2192} 1.1.drv, \u{1b}[32;1m-10.5 KiB\u{1b}[0m\n";
        let c = parse_diff_closures(out);
        assert_eq!(c[0].size_delta.as_deref(), Some("-10.5 KiB"));
        assert_eq!(c[0].new, vec!["1.1"]);
    }

    #[test]
    fn size_only_line_has_no_arrow_and_is_skipped() {
        let out = "bash: \u{1b}[31;1m25.5 KiB\u{1b}[0m\n";
        let c = parse_diff_closures(out);
        assert!(c.is_empty());
    }

    #[test]
    fn handles_absent_and_versionless_markers() {
        // ∅ = absent, ε = versionless. Neither yields a usable version.
        let out = "somesrc.drv: \u{03b5} \u{2192} \u{2205}, \u{1b}[32;1m-10.4 KiB\u{1b}[0m\n\
                   newthing: \u{2205} \u{2192} 1.0.drv\n";
        let c = parse_diff_closures(out);
        // somesrc.drv → ε→∅ becomes empty→empty → skipped entirely by parser.
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "newthing");
        assert_eq!(c[0].kind, ChangeKind::Added);
        assert_eq!(c[0].new, vec!["1.0"]);
    }

    #[test]
    fn multi_version_new_set_not_mistaken_for_size() {
        // gnused: 4.9-binlore → 4.10-binlore, 4.10  (comma is a second version, not size)
        let out = "gnused: 4.9-binlore.drv \u{2192} 4.10-binlore.drv, 4.10.drv\n";
        let c = parse_diff_closures(out);
        assert_eq!(c[0].name, "gnused");
        assert_eq!(c[0].new, vec!["4.10-binlore", "4.10"]);
        assert_eq!(c[0].size_delta, None);
    }

    // ---- noise filtering (is_meaningful) ----

    #[test]
    fn filters_source_patch_and_cve_noise() {
        let out = "\
CVE: 2026-32316.patch \u{2192} 2026-33947.patch\n\
Python: \u{2205} \u{2192} 3.13.14.tar.xz\n\
0001-fix-march-x86: \u{2205} \u{2192} 64-v4.patch\n\
firefox-unwrapped: 152.0.5.drv \u{2192} 152.0.6.drv\n\
bootstrap-stage2-stdenv-linux.drv: 1.drv \u{2192} 2.drv\n";
        let mut c = parse_diff_closures(out);
        c.retain(PackageChange::is_meaningful);
        let names: Vec<&str> = c.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["firefox-unwrapped"]);
    }

    #[test]
    fn realistic_batch_keeps_only_real_upgrades() {
        let out = "\
brave: 1.91.180.drv \u{2192} 1.92.139.drv\n\
mesa: 26.1.4.drv \u{2192} 26.1.5.drv\n\
nix: 2.34.7.drv \u{2192} 2.34.8.drv\n\
absolute_shlib_path.patch: \u{2205} x4 \u{2192} \u{2205} x5\n\
Compress-Raw-Zlib: 2.206.tar.gz.drv \u{2192} \u{2205}\n";
        let mut c = parse_diff_closures(out);
        c.retain(PackageChange::is_meaningful);
        let names: Vec<&str> = c.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["brave", "mesa", "nix"]);
    }
}
