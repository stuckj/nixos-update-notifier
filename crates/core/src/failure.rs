//! Turning a failed check (or apply) into something the window can show.
//!
//! A failure reaches us as an `anyhow` chain whose outermost context names the step
//! ("evaluating candidate toplevel drvPath") and whose innermost message is the captured
//! stderr of the `nix` command that failed. Nix's useful sentence is the last `error:` line
//! of that output; everything above it is evaluation trace.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    /// What was being done, e.g. "evaluating candidate toplevel drvPath".
    pub step: String,
    /// Nix's final `error:` line, without the prefix. Empty if there was none.
    pub summary: String,
    /// The full error chain, including nix's stderr, for "Show details" / "Copy".
    pub details: String,
    /// A one-line suggested fix, for failures we recognise.
    pub hint: Option<String>,
}

impl Failure {
    /// Build from an error chain. `step` is the outermost context.
    pub fn from_error(e: &anyhow::Error) -> Self {
        let details = format!("{e:#}");
        let step = e.to_string();
        let step = step
            .lines()
            .next()
            .unwrap_or_default()
            .trim_end_matches(':');
        let summary = last_error_line(&details).unwrap_or_default();
        Self {
            step: step.to_string(),
            hint: hint_for(&details),
            summary,
            details,
        }
    }

    /// Build from free-form output, e.g. the tail of the apply log.
    pub fn from_output(step: &str, output: &str) -> Self {
        Self {
            step: step.to_string(),
            summary: last_error_line(output).unwrap_or_default(),
            details: output.to_string(),
            hint: hint_for(output),
        }
    }

    /// One line for a notification body: the summary if nix gave one, else the step.
    pub fn headline(&self) -> &str {
        if self.summary.is_empty() {
            &self.step
        } else {
            &self.summary
        }
    }
}

/// The last non-empty `error: …` line, with the prefix removed.
///
/// Nix often prints a bare `error:` header followed by indented trace lines and the real
/// message as a later, indented `error: …`, so the bare header is skipped. `Error: …` is
/// how our own privileged helper reports (a cancelled rebuild, say), after nix is done.
pub fn last_error_line(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim_start();
            l.strip_prefix("error:")
                .or_else(|| l.strip_prefix("Error:"))
        })
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map(str::to_string)
}

/// The text between the first pair of single quotes after `marker`'s position.
fn quoted_before(text: &str, marker: &str) -> Option<String> {
    let line = text.lines().find(|l| l.contains(marker))?;
    let start = line.find('\'')? + 1;
    let end = start + line[start..].find('\'')?;
    Some(line[start..end].to_string())
}

/// Suggest a fix for the nixpkgs refusals that account for most evaluation failures after
/// an input bump. Wording follows the option names in the nixpkgs manual.
pub fn hint_for(text: &str) -> Option<String> {
    if text.contains("because it is marked as insecure") {
        let pkg = quoted_before(text, "marked as insecure")?;
        return Some(format!(
            "Add \"{pkg}\" to nixpkgs.config.permittedInsecurePackages, or stop using \
             the package that depends on it."
        ));
    }
    if text.contains("has an unfree license") {
        let pkg = quoted_before(text, "has an unfree license");
        return Some(match pkg {
            Some(p) => format!(
                "\"{p}\" is unfree: allow it with nixpkgs.config.allowUnfreePredicate \
                 (or allowUnfree = true)."
            ),
            None => "An unfree package is refused: see nixpkgs.config.allowUnfree.".into(),
        });
    }
    if text.contains("is marked as broken") {
        let pkg = quoted_before(text, "is marked as broken");
        return Some(match pkg {
            Some(p) => format!(
                "\"{p}\" is marked broken in this nixpkgs: wait for a fix, pin it from an \
                 older nixpkgs, or set nixpkgs.config.allowBroken."
            ),
            None => "A package is marked broken in this nixpkgs.".into(),
        });
    }
    if text.contains("is not available on the requested hostPlatform") {
        return Some(
            "A package does not support this platform any more; remove it or pin it from \
             an older nixpkgs."
                .into(),
        );
    }
    if text.contains("hash mismatch in fixed-output derivation") {
        return Some(
            "A fixed-output hash no longer matches what was downloaded; update the hash \
             in your configuration (or retry if the source is flaky)."
                .into(),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape of the real failure reported in issue #25.
    const INSECURE: &str = "command \"nix\" failed (exit status: 1):
error:
       … while evaluating the option `system.build.toplevel':

       … while evaluating definitions from `/nix/store/x-source/nixos/modules/system/activation/top-level.nix':

       error: Refusing to evaluate package 'electron-42.11.18' in /nix/store/y-source/pkgs/development/tools/electron/binary/generic.nix:50 because it is marked as insecure

       Known issues:
        - Electron version 42.11.18 is EOL";

    #[test]
    fn summarises_an_insecure_package_refusal() {
        let e = anyhow::anyhow!(INSECURE).context("evaluating candidate toplevel drvPath");
        let f = Failure::from_error(&e);
        assert_eq!(f.step, "evaluating candidate toplevel drvPath");
        assert!(
            f.summary
                .starts_with("Refusing to evaluate package 'electron-42.11.18'"),
            "{}",
            f.summary
        );
        assert!(f.summary.ends_with("because it is marked as insecure"));
        assert!(f.details.contains("Known issues"));
        let hint = f.hint.expect("hint");
        assert!(hint.contains("\"electron-42.11.18\""), "{hint}");
        assert!(hint.contains("permittedInsecurePackages"), "{hint}");
    }

    #[test]
    fn a_bare_error_header_is_not_the_summary() {
        assert_eq!(last_error_line("error:\n   trace\n"), None);
        assert_eq!(
            last_error_line("error:\n  … x\n  error: the real one\n").as_deref(),
            Some("the real one")
        );
    }

    #[test]
    fn the_helpers_own_final_error_wins() {
        let log = "error: interrupted by the user\nError: apply cancelled before activation; the system was not changed\n";
        assert_eq!(
            Failure::from_output("applying", log).summary,
            "apply cancelled before activation; the system was not changed"
        );
    }

    /// The tail of a failed apply log: nix's error, then the helper's own exit report.
    #[test]
    fn a_failed_rebuild_is_summarised_by_nix_not_by_the_exit_status() {
        let failed =
            crate::apply::RebuildFailed(std::os::unix::process::ExitStatusExt::from_raw(256));
        let log = format!(
            "error: Refusing to evaluate package 'electron-42.11.18' in /x.nix:50 because it is marked as insecure\n{failed}\n"
        );
        let f = Failure::from_output("applying", &log);
        assert!(
            f.summary.starts_with("Refusing to evaluate"),
            "{}",
            f.summary
        );
    }

    #[test]
    fn recognises_unfree_and_broken() {
        let unfree = "error: Package 'slack-4.0' in /nix/store/z.nix:1 has an unfree license ('unfree'), refusing to evaluate.";
        assert!(hint_for(unfree).unwrap().contains("\"slack-4.0\""));
        let broken = "error: Package 'foo-1.2' in /nix/store/z.nix:1 is marked as broken, refusing to evaluate.";
        assert!(hint_for(broken).unwrap().contains("\"foo-1.2\""));
    }

    #[test]
    fn unrecognised_failures_get_no_hint() {
        assert_eq!(hint_for("error: something else entirely"), None);
        let f = Failure::from_output("applying updates", "no error lines here");
        assert_eq!(f.summary, "");
        assert_eq!(f.headline(), "applying updates");
    }

    #[test]
    fn step_is_the_outermost_context_only() {
        let e = anyhow::anyhow!("inner").context("outer step");
        assert_eq!(Failure::from_error(&e).step, "outer step");
    }
}
