//! Integration tests that drive the REAL `nix` CLI against a tiny committed fixture flake.
//!
//! They are `#[ignore]`d so a plain `cargo test` (which may run where `nix` is absent)
//! skips them; CI runs them explicitly with `--ignored` inside the Nix devShell. Their job
//! is to guard against `nix` output-format drift — especially the `diff-closures` format
//! the parser depends on.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Copy a fixture flake to a fresh temp dir so `nix` can write `flake.lock` without
/// dirtying the committed tree.
fn copy_to_temp(src: &Path, tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dst = std::env::temp_dir().join(format!("nun-it-{tag}-{nanos}"));
    let status = Command::new("cp")
        .arg("-a")
        .arg(src)
        .arg(&dst)
        .status()
        .expect("cp fixture");
    assert!(status.success(), "copying fixture failed");
    dst
}

fn eval_drv(flake_dir: &Path, attr: &str) -> String {
    let installable = format!(
        "{}#packages.x86_64-linux.{attr}.drvPath",
        flake_dir.display()
    );
    let out = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "--no-warn-dirty",
            "eval",
            "--raw",
            &installable,
        ])
        .output()
        .expect("spawn nix eval");
    assert!(
        out.status.success(),
        "nix eval {installable} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
#[ignore = "requires the `nix` CLI; run in CI with --ignored"]
async fn lists_flake_inputs() {
    let dir = copy_to_temp(&fixture("hostflake"), "inputs");
    let names = nun_core::nix::flake_input_names(&dir)
        .await
        .expect("flake_input_names");
    assert_eq!(names, vec!["dep1".to_string(), "dep2".to_string()]);
}

#[tokio::test]
#[ignore = "requires the `nix` CLI; run in CI with --ignored"]
async fn diff_closures_parses_real_nix_output() {
    let dir = copy_to_temp(&fixture("hostflake"), "diff");
    let old = eval_drv(&dir, "parentOld");
    let new = eval_drv(&dir, "parentNew");
    assert_ne!(old, new, "the two candidate drvs should differ");

    let changes = nun_core::diff::diff_closures(&old, &new)
        .await
        .expect("diff_closures");

    // The `foo` sub-derivation changed 1.0 -> 2.0; the parser must surface exactly that.
    let foo = changes
        .iter()
        .find(|c| c.name == "foo")
        .unwrap_or_else(|| panic!("expected a `foo` change, got: {changes:?}"));
    assert_eq!(foo.old, vec!["1.0"]);
    assert_eq!(foo.new, vec!["2.0"]);
    assert!(matches!(foo.kind, nun_core::diff::ChangeKind::Changed));
}
