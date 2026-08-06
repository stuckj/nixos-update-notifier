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

/// Hand every rendered flake ref to nix and check it means what the lock said.
///
/// String equality against an expected ref proves nothing about how nix READS it, which is
/// where the subtle failures live: a `narHash` on a git ref stays inside the URL and
/// silently changes the remote, and a ref whose rev is not recognised as a rev is not
/// pinned at all. `builtins.parseFlakeRef` is nix's own parser, so this asks nix directly —
/// and it needs no network.
#[tokio::test]
#[ignore = "requires the `nix` CLI; run in CI with --ignored"]
async fn rendered_flake_refs_parse_back_to_what_was_locked() {
    fn parse(r: &str) -> serde_json::Value {
        let out = Command::new("nix")
            .args([
                "--extra-experimental-features",
                "nix-command flakes",
                "eval",
                "--json",
                "--expr",
                &format!("builtins.parseFlakeRef {}", nix_str(r)),
            ])
            .output()
            .expect("spawn nix eval");
        assert!(
            out.status.success(),
            "nix could not parse {r:?}:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("parseFlakeRef json")
    }
    fn nix_str(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
    fn lock_with(attrs: &str) -> String {
        format!(
            r#"{{"version":7,"root":"root","nodes":{{
                 "root":{{"inputs":{{"nixpkgs":"nixpkgs"}}}},
                 "nixpkgs":{{"locked":{attrs}}}}}}}"#
        )
    }
    // A real-shaped revision: nix classifies a 40-char hex string as a `rev`, and anything
    // else as a `ref` — so a stand-in like "abc" would look pinned while pinning nothing.
    const REV: &str = "04607e1165ac22c5fde6dcc54c9e0b3c0487c555";

    let github = lock_with(&format!(
        r#"{{"type":"github","owner":"NixOS","repo":"nixpkgs","rev":"{REV}",
             "narHash":"sha256-fKCq5jphd6l/Ms6gc3dptkw/TcKLYub9lQE5g6rbbkc=","dir":"sub/dir"}}"#
    ));
    let r = nun_core::lock::input_flake_ref(&github, "nixpkgs").expect("github ref");
    let p = parse(&r);
    assert_eq!(p["type"], "github");
    assert_eq!(p["owner"], "NixOS");
    assert_eq!(p["repo"], "nixpkgs");
    assert_eq!(p["rev"], REV, "must parse as a rev, not a ref: {r}");
    assert_eq!(p["dir"], "sub/dir");
    assert_eq!(
        p["narHash"],
        "sha256-fKCq5jphd6l/Ms6gc3dptkw/TcKLYub9lQE5g6rbbkc="
    );

    // The git scheme lifts only certain parameters out of the query; everything else stays
    // in the URL. A narHash here would become part of the address nix fetches from.
    let git = lock_with(&format!(
        r#"{{"type":"git","url":"https://ex.com/nixpkgs.git","rev":"{REV}","narHash":"sha256-zzz="}}"#
    ));
    let r = nun_core::lock::input_flake_ref(&git, "nixpkgs").expect("git ref");
    let p = parse(&r);
    assert_eq!(p["type"], "git");
    assert_eq!(p["rev"], REV);
    assert_eq!(
        p["url"], "https://ex.com/nixpkgs.git",
        "the fetch URL must be untouched, got {r}"
    );

    // A local checkout is expected to drift; a narHash would make one edit a hard error on
    // every future check.
    let path = lock_with(r#"{"type":"path","path":"/nix/store/abc-source","narHash":"sha256-q="}"#);
    let r = nun_core::lock::input_flake_ref(&path, "nixpkgs").expect("path ref");
    let p = parse(&r);
    assert_eq!(p["type"], "path");
    assert_eq!(p["path"], "/nix/store/abc-source");
    assert!(
        p.get("narHash").is_none(),
        "path refs must not be hashed: {r}"
    );

    // An unprefixed URL parses as a tarball, so a `file` input has to say so.
    let file =
        lock_with(r#"{"type":"file","url":"https://ex.com/pkgs.json","narHash":"sha256-w="}"#);
    let r = nun_core::lock::input_flake_ref(&file, "nixpkgs").expect("file ref");
    assert_eq!(parse(&r)["type"], "file");

    // A tarball's narHash is the whole pin, and this scheme shares git's hazard of leaving
    // unrecognised parameters inside the URL.
    let tarball = lock_with(
        r#"{"type":"tarball","url":"https://ex.com/n.tar.xz?id=1","narHash":"sha256-t="}"#,
    );
    let r = nun_core::lock::input_flake_ref(&tarball, "nixpkgs").expect("tarball ref");
    let p = parse(&r);
    assert_eq!(p["type"], "tarball");
    assert_eq!(p["narHash"], "sha256-t=", "narHash must be lifted out: {r}");
    assert_eq!(
        p["url"], "https://ex.com/n.tar.xz?id=1",
        "the fetch URL must keep its own query and nothing else, got {r}"
    );

    // A GitLab SUBGROUP puts a `/` in the owner. Unencoded, `gitlab:grp/sub/nixpkgs/<rev>`
    // does not fail — it parses as project `grp/sub` on a branch named `nixpkgs/<rev>`, so
    // the wrong repo is fetched and the revision pin is silently gone.
    let subgroup = lock_with(&format!(
        r#"{{"type":"gitlab","owner":"grp/sub","repo":"nixpkgs","rev":"{REV}",
             "host":"gitlab.example.com"}}"#
    ));
    let r = nun_core::lock::input_flake_ref(&subgroup, "nixpkgs").expect("subgroup ref");
    let p = parse(&r);
    assert_eq!(p["type"], "gitlab");
    assert_eq!(p["owner"], "grp/sub", "subgroup owner mangled: {r}");
    assert_eq!(p["repo"], "nixpkgs");
    assert_eq!(p["rev"], REV, "must still be pinned to a rev: {r}");
    assert_eq!(p["host"], "gitlab.example.com");

    // A host carrying a PORT cannot be written as a URL-form ref at all — nix rejects both
    // `host=h:8443` and `host=h%3A8443` — so there must be no ref rather than a wrong one.
    let ported = lock_with(&format!(
        r#"{{"type":"gitlab","owner":"grp","repo":"nixpkgs","rev":"{REV}",
             "host":"gitlab.example.com:8443"}}"#
    ));
    assert_eq!(nun_core::lock::input_flake_ref(&ported, "nixpkgs"), None);
}

/// The changelog lookup against the real `nix`, over a fixture package set.
///
/// The expression is generated Nix source handed to `--apply`, so its syntax and its
/// failure handling can only really be checked by evaluating it. The fixture stands in for
/// nixpkgs to keep that offline and quick; what it exercises is the guarding, since this is
/// ONE evaluation for the whole batch and anything that escapes costs every link at once.
#[tokio::test]
#[ignore = "requires the `nix` CLI; run in CI with --ignored"]
async fn changelog_lookup_survives_hostile_metadata() {
    let dir = copy_to_temp(&fixture("fakepkgs"), "changelog");
    let system = nun_core::nix::current_system_double()
        .await
        .expect("current system");

    let names: Vec<String> = [
        "plain",
        "listed",
        "nochangelog",
        "emptychangelog",
        "nullchangelog",
        "throwing",
        "throwinglist",
        "functionchangelog",
        "mixedlist",
        "not-in-this-package-set",
        // Nothing in a name may be read as Nix syntax.
        "${plain}",
        "quote\"and\\backslash",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    let got =
        nun_core::nix::meta_changelogs(&format!("path:{}", dir.display()), &system, &names).await;

    assert_eq!(
        got.get("plain").map(String::as_str),
        Some("https://example.invalid/plain")
    );
    assert_eq!(
        got.get("listed").map(String::as_str),
        Some("https://example.invalid/first")
    );
    // The one usable element of a list that also holds an unserialisable value.
    assert_eq!(
        got.get("mixedlist").map(String::as_str),
        Some("https://example.invalid/good")
    );
    // Everything else contributes nothing — and, crucially, takes nothing else down.
    for absent in [
        "nochangelog",
        "emptychangelog",
        "nullchangelog",
        "throwing",
        "throwinglist",
        "functionchangelog",
        "not-in-this-package-set",
        "${plain}",
        "quote\"and\\backslash",
    ] {
        assert!(!got.contains_key(absent), "expected no entry for {absent}");
    }
}

/// The inventory diff — the path production actually uses — against real `nix` output.
///
/// The unit tests feed it hand-written JSON, which cannot catch the one thing this module
/// exists to survive: nix changing what it emits. This exercises the real wrapper shape and
/// key style, the subprocess wiring, and above all a `__structuredAttrs` derivation, whose
/// metadata nix moved out of the environment — the drift that made an upgraded package
/// report as removed.
#[tokio::test]
#[ignore = "requires the `nix` CLI; run in CI with --ignored"]
async fn pkgs_diff_reads_structured_attrs_from_real_nix() {
    let dir = copy_to_temp(&fixture("hostflake"), "pkgs");
    let old = eval_drv(&dir, "structuredOld");
    let new = eval_drv(&dir, "structuredNew");
    assert_ne!(old, new, "the two candidate drvs should differ");

    let changes = nun_core::pkgs::diff(&old, &new).await.expect("pkgs::diff");

    let bar = changes
        .iter()
        .find(|c| c.name == "bar")
        .unwrap_or_else(|| panic!("expected a `bar` change, got: {changes:?}"));
    assert_eq!(bar.old, vec!["1.0"]);
    assert_eq!(bar.new, vec!["2.0"]);
    assert_eq!(bar.kind, nun_core::diff::ChangeKind::Changed);
}
