# Contributing

Architecture, internals, and development workflow. For installing and using the tool, see
[README.md](README.md); for cutting a release, [RELEASING.md](RELEASING.md).

---

## Development setup

```console
$ nix develop                        # rust toolchain + gtk4 + pkg-config + nvd + shellcheck
$ cargo test -p nun-core             # pure-logic unit tests (no system deps)
$ cargo clippy --workspace --all-targets -- -D warnings
$ cargo fmt --all
$ cargo run -p nixos-update-notifier -- check   # exercise the no-download check
$ nix build                          # build both binaries via the flake
```

### Before pushing

Run everything the `lint + unit tests` CI job runs, in its order. `cargo fmt` is the easy
one to skip, and it fails the job before clippy or the tests get a chance to run — so a
formatting slip hides whatever else might be broken:

```console
$ cargo fmt --all -- --check \
    && cargo clippy -p nun-core -p nixos-update-notifier --all-targets -- -D warnings \
    && cargo test -p nun-core -p nixos-update-notifier \
    && shellcheck scripts/*.sh
```

`Cargo.lock` must be committed: the package builds with `rustPlatform.buildRustPackage`
using `cargoLock.lockFile = ./Cargo.lock`.

---

## Architecture

Three crates in a Cargo workspace. The split exists so the logic and the daemon carry **no
GUI dependency**, which is what lets CI test them on a bare runner with no system libraries
(ksni 0.3, zbus and tokio are pure Rust; only the GTK crate links C libraries).

```
crates/
  core/   src/{config,nix,check,diff,changelog,apply,lock,failure,progress,release}.rs, lib.rs
          tests/integration.rs + tests/fixtures/
  daemon/ src/{main,daemon,tray,notify,state,dbus,persist}.rs
  gtk/    src/{main,ui,client}.rs
nix/      package.nix, hm-module.nix, nixos-module.nix
scripts/  set-version.sh, plasma-smoke-test.sh
```

- **`nun-core`** (lib) — config, nix orchestration, the offline diff parser, changelog
  resolution, lock inspection, and the privileged rebuild. No GTK, no ksni.
- **`nixos-update-notifier`** (bin) — the daemon: SNI tray, periodic checker,
  notifications, and the `org.nixos.UpdateNotifier1` D-Bus service. Also the headless CLI.
- **`nixos-update-notifier-gtk`** (bin) — the GTK4 client. Launched separately, it drives
  the daemon purely over D-Bus and holds no update logic of its own.

### Why the daemon and client are separate processes

The daemon is a standalone service; the GUI is a thin client that can come and go. This
also keeps GTK's main-thread requirements from tangling with the tokio/ksni event loop.

The client talks to the daemon over D-Bus and repaints from a short poll. `GetStatus`,
`GetUpdates` and `GetWarnings` carry the status and the package list; everything else the
windows show (the current apply, why the last check failed, release upgrade state) comes as
one JSON document from `GetDetails`, whose type (`nun_core::ipc::Details`) both sides
share. Methods that change something (`Apply`, `CancelApply`, `Dismiss`, `RevertLock`,
`PreviewUpgrade`, `Upgrade`, `DismissRelease`) check that the caller is one of our binaries.

The bus name is also the single-instance lock. It is requested *without* replacement on
either side (zbus's default flags allow it), before the tray is registered; a second
`run` asks the first to show its window (`ShowUpdates`) and exits.

### Keep the command loop non-blocking

Checks, applies and upgrade previews run in spawned tasks and report back through channels
that `select!` handles alongside commands. Awaiting a check inline blocks every menu action
for its whole duration — minutes, on a cold evaluation — and the daemon simply looks broken:
clicks queue up silently and then all fire at once when the check finishes. There's a
regression probe for this in the smoke test.

A check and an apply must not overlap in effect. A check compares against the repo's
`flake.lock`, which an apply replaces, so no check starts while an apply runs, and a check
that was already running when an apply began is discarded rather than allowed to report
"up to date" against the new lock (or overwrite the "applying" status). Each apply carries
an increasing id, and the progress window follows one id, so it never mistakes an earlier
apply's end for the current one's.

---

## How the check works

Updating a flake system means advancing `flake.lock` and rebuilding. A *check* must work out
what that would change **without touching your repo and without downloading package
builds**:

1. **Copy, don't mutate.** The flake repo is copied to a per-process working dir under
   `$XDG_CACHE_HOME`. The real repo is only written on **Apply**.
2. **Baseline drv.** Evaluate the current `system.build.toplevel.drvPath` from the copy.
   Evaluating `.drvPath` *instantiates* the derivation but does not realise it — nothing is
   downloaded.
3. **Advance inputs.** Run `nix flake update <input>` in the copy, one input at a time
   (see below), honoring the include/exclude lists.
4. **Candidate drv.** Evaluate the toplevel `.drvPath` again. Same path → no updates.
5. **Diff, offline.** `nix store diff-closures <baseline.drv> <candidate.drv>` over the two
   **derivation** paths. Diffing derivation closures never downloads substitutes.
6. **Changelogs.** Best-effort `meta.changelog` lookups, in a single evaluation, against the
   nixpkgs the candidate lock pins — the revision the update would install, already fetched,
   so this stays within "downloads nothing" and the links describe the version being offered.

### "Downloads nothing" — precisely

A check downloads no package *outputs*. It does fetch the *source of the inputs it
advances* (a new nixpkgs tree is tens of MB), because you cannot evaluate against a
revision you don't have. That's small next to realising a system closure.

### Inputs are advanced one at a time

`nix flake update a b c` is atomic: one failing input means none advance and the whole check
errors. In practice that hides everything behind a bare failure — a flake input pointing at
a local fork that has been moved (`error: Git repository "…" does not exist`) would mask
100+ pending nixpkgs updates. Per-input updates cost extra nix invocations but let one
broken input be reported (surfaced as `failed_inputs`, shown as a ⚠ banner) while everything
else is still checked.

### `diff-closures` reality

Diffing derivation closures is what keeps checks download-free, but the raw output is messy
— all of this is verified against real output, not assumed:

- It is **ANSI-coloured even when redirected**, so escapes must be stripped.
- Every node is a `.drv`, so upgrades read as `aws-c-http: 0.10.4.drv → 0.11.0.drv`.
- It uses **two empty markers**: `∅` (U+2205, absent) and `ε` (U+03B5, present but
  versionless).
- The closure carries **source tarballs, patches, CVE-named artefacts and toolchain
  bootstrap stages**, and the name/version heuristic mangles some (`CVE-2026-….patch` →
  name `CVE`). Binary sources leak too, e.g. `Claude: <sha>.dmg -> <sha>.dmg`.

`diff.rs` strips and filters all of that. What survives is a clean list — in a validated run
against a real config, ~107 genuine changes out of 169 raw lines. It still includes
build-time libraries and split derivations (the many `nix-*` components), because that is
what a derivation-closure diff sees. That's the honest cost of a no-download check.

For a pristine runtime list, `check --exact` builds the candidate (downloading) and diffs
realised **output** closures. It is never run in the background.

### Package changes vs. configuration changes

A changed toplevel derivation does not imply a package update. A home-manager bump can
change 10 of 20,000 derivations — activation script, units, `/etc` — with
`diff-closures` reporting nothing at all, because nothing changed name or version. Those
are separate states (`SystemChangesOnly` vs `UpdatesAvailable`) so a config regeneration
doesn't badge the tray as if packages had updates.

### "Up to date" is about the running system

Comparing the repo's lock with a newer one only says whether the *lock* is current. The
baseline's output path is also compared with `/run/current-system`; if they differ — a lock
advanced without a switch, or configuration edits not yet rebuilt — the state is
`NotApplied`, offering a rebuild of the repo as it is instead of a false "up to date" — and
a revert to the newest `flake.lock.bak.*` only when evaluating with that backup gives the
running system exactly. In that state the check also lists what differs: the same
inventory diff, from the running system's derivation (`nix derivation show -r` resolves an
output path to the derivation that built it) to the repo's.

### Failures

A failed check keeps its error chain and nix's stderr (`failure.rs`): the step that failed,
nix's last `error:` line, a hint for the common nixpkgs refusals (insecure, unfree, broken),
and the full text for the window's "Show details" and Copy. A failed apply gets the same
from its log.

---

## Release upgrades

A check only advances inputs within the branches `flake.nix` names. Moving to a new NixOS
release rewrites those branches, so it is a separate, explicit operation (`release.rs`):

- **Detect.** Alongside each check, `git ls-remote` lists nixpkgs' `nixos-YY.MM` branches
  and compares them with the `original.ref` of the first configured release input. A branch
  exists weeks before its release, so a release counts as out from the first day of the
  following month. Support ends roughly a month after the next release.
- **Preview** (on request). The release inputs' branch names are rewritten in a scratch copy
  of `flake.nix` — literal replacement of e.g. `nixos-26.05`, refused if the branch is not
  written literally or another, non-retargeted input tracks the same branch — relocked, and
  evaluated against the user's tree with `--override-input` for the relocked inputs (the
  repo's own `flake.nix` still names the old branch). The diff is the same offline one a
  check makes.
- **Upgrade.** The daemon backs up and installs the new `flake.nix` and lock, unprivileged,
  then runs the same rebuild as an apply; both files are restored if it fails. It refuses if
  `flake.nix` or `flake.lock` changed since the preview. `system.stateVersion` is never touched.

## Apply progress

nix only draws its progress bar on a TTY. `apply-privileged` passes the constant
`--log-format internal-json` to `nixos-rebuild`, which forwards it to nix, so the apply log
carries nix's `@nix {json}` activity events. Root does no parsing: the GTK client folds the
events into download/build totals (`progress.rs`, totals computed the way nix's own
progress bar does) and shows the plain messages as the log. The fixtures under
`tests/fixtures/nix-log/` are real nix output.

---

## The privilege boundary

Only `nixos-rebuild switch` needs root. The design keeps root's surface minimal:

- The unprivileged daemon does the lock (and, for a release upgrade, `flake.nix`) backup,
  install and restore — they operate on user-owned files. Root never opens a path inside a
  user-writable directory, which eliminates a symlink/TOCTOU class **by construction**
  rather than by careful coding.
- `apply-privileged` takes only `--repo` and `--host` (and the cancel flag it only reads).
  No lock path, no pass-through args; the flags it adds itself are constants:
  `--log-format internal-json`, and `--no-update-lock-file` so that a lock not covering
  `flake.nix` fails the rebuild rather than being rewritten in the repo by root.
  There is deliberately no `rebuild_extra_args`: free-form arguments into root's
  `nixos-rebuild` (`--override-input`, `-I`, `--substituters`) would let anything that can
  write the config change what root evaluates, invisibly to the polkit prompt.
- The generated polkit rule matches the daemon binary by **exact path**, both its wrapper
  and the `makeBinaryWrapper` `.…-wrapped` sibling that `current_exe()` resolves to. A
  prefix match would also authorize the GTK client.
- `lock.rs` refuses a candidate that would advance an excluded input. This is a safety net
  against a bug in the check path — the failure mode being an unbootable machine — not a
  security control.

The inherent limit (root evaluates a user-writable flake) is documented for users in the
README; it cannot be fixed without changing what the tool is for.

---

## Testing

| Tier | Command | Needs |
|---|---|---|
| Unit | `cargo test -p nun-core -p nixos-update-notifier` | nothing |
| MSRV | `RUSTUP_TOOLCHAIN=<rust-version> cargo check --locked -p nun-core -p nixos-update-notifier --all-targets` | the toolchain named by `rust-version` in `Cargo.toml` |
| Integration | `cargo test -p nun-core --test integration -- --ignored` | real `nix` |
| Smoke | `scripts/plasma-smoke-test.sh` | a live Plasma session |
| Smoke (VM) | `nix build -L .#vmTests.plasma-smoke` | KVM |

Integration tests drive real `nix` against committed fixture flakes and pin the
`diff-closures` output format so a future nix release can't silently break the parser. They
are `#[ignore]`d so a plain `cargo test` works where `nix` is absent.

The smoke test covers what unit tests can't — SNI registration, the D-Bus service and its
caller checks, mid-check responsiveness, icon-name resolution, and the GTK client. It is
strictly read-only and never applies an update:

```console
$ nix build
$ scripts/plasma-smoke-test.sh --bin-dir ./result/bin \
    --flake ~/dev/personal/nixos-config --host nixos-x1
```

CI runs the same script unattended (`--non-interactive`) in a NixOS VM with a real Plasma 6
Wayland session (`nix/tests/plasma-smoke.nix`). There, the three questions become checks:

- **Tray icon:** the item is registered with Plasma's StatusNotifierWatcher.
- **Notifications:** the notification server accepted one and returned an id.
- **GTK client:** it called the daemon over D-Bus and stayed up, and OCR finds its "Pending
  updates" window on screen.

What the VM can't judge is whether the icon *looks* right in your theme and panel. So still
run the script by hand before merging changes to the tray's appearance. Most of the bugs
found here so far were invisible to unit tests: a blank tray icon logs nothing anywhere, and
a daemon that ignores its menu looks identical to a working one until you click.

---

## CI

`ci.yml` runs four jobs on every push and PR:

1. **lint + unit tests** — `rustfmt`, `clippy -D warnings`, unit tests and `shellcheck`, on
   a bare runner with no apt packages (this is what the crate split buys).
2. **nix flake check + build** — builds the GTK client and both Nix modules.
3. **Plasma smoke test (NixOS VM)** — the smoke test above, in a Plasma 6 VM on KVM. Its
   screenshots are uploaded as the `plasma-smoke-screenshots` artifact, including when it
   fails.
4. **fixture-flake integration** — the real-`nix` tests.

The VM test is kept out of `checks` so `nix flake check` still works without KVM.

---

## Conventions

- Everything must be config-driven — nothing hardcoded to a particular machine or user.
- Prefer eliminating a failure class structurally over guarding against it. Several fixes
  here took that shape (root doing no file I/O; single-instance windows via GTK rather than
  bookkeeping).
- When a claim about `nix` behaviour matters, verify it against real output and record what
  you saw in a comment or test. Most of the parser's rules exist because the real output
  contradicted a reasonable assumption.
