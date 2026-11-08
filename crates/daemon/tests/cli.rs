//! The CLI as a script would run it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A stand-in for `nix`: just enough for `check` to run to completion with no updates.
const FAKE_NIX: &str = r#"#!/bin/sh
case "$*" in
  *"flake metadata"*) echo '{"locks":{"root":"root","nodes":{"root":{"inputs":{"dep":"dep"}},"dep":{"locked":{"type":"path","path":"/x"},"original":{"type":"path","path":"/x"}}},"version":7}}' ;;
  *"flake update"*) exit 0 ;;
  *eval*) echo "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-toplevel.drv" ;;
  *) exit 1 ;;
esac
"#;

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nun-cli-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::create_dir_all(dir.join("flake")).unwrap();
    dir
}

fn write_executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Logging goes to stderr: on stdout it would come before the JSON and break any parser,
/// and a check logs as a matter of course (here a debug line about the evaluated drvs).
#[test]
fn check_json_prints_only_json_on_stdout() {
    let dir = scratch();
    write_executable(&dir.join("bin/nix"), FAKE_NIX);
    std::fs::write(dir.join("flake/flake.nix"), "{ outputs = _: { }; }").unwrap();
    std::fs::write(
        dir.join("flake/flake.lock"),
        r#"{"nodes":{"root":{"inputs":{}}},"root":"root","version":7}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "flake_path = {:?}\nhost_attr = \"h\"\n",
            dir.join("flake").display().to_string()
        ),
    )
    .unwrap();

    let path = format!(
        "{}:{}",
        dir.join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_nixos-update-notifier"))
        .args(["--config"])
        .arg(dir.join("config.toml"))
        .args(["check", "--json"])
        .env("PATH", path)
        .env("RUST_LOG", "debug")
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env("HOME", &dir)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "check failed:\n{stderr}");
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{stdout}"));
    assert!(parsed.is_array(), "{stdout}");
    assert!(
        stderr.contains("evaluated current vs candidate"),
        "{stderr}"
    );
    let _ = std::fs::remove_dir_all(dir);
}
