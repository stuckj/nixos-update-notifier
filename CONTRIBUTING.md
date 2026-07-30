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
  core/   src/{config,nix,check,diff,changelog,apply,lock}.rs, lib.rs
          tests/integration.rs + tests/fixtures/
  daemon/ src/{main,daemon,tray,notify,state,dbus}.rs
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

The client talks to the daemon via `CheckNow`, `Apply`, `Dismiss`, `GetUpdates`,
`GetStatus`, and `GetWarnings`, and repaints from a short poll.

### Keep the command loop non-blocking

Checks run in a spawned task and report back through a channel that `select!` handles
alongside commands. Awaiting a check inline blocks every menu action for its whole duration
— minutes, on a cold evaluation — and the daemon simply looks broken: clicks queue up
silently and then all fire at once when the check finishes. There's a regression probe for
this in the smoke test.

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
6. **Changelogs.** Best-effort `meta.changelog` lookups against a configurable nixpkgs ref.

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

---

## The privilege boundary

Only `nixos-rebuild switch` needs root. The design keeps root's surface minimal:

- The unprivileged daemon does the lock backup, install and restore — they operate on
  user-owned files. Root never opens a path inside a user-writable directory, which
  eliminates a symlink/TOCTOU class **by construction** rather than by careful coding.
- `apply-privileged` takes only `--repo` and `--host`. No lock path, no pass-through args.
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
| Integration | `cargo test -p nun-core --test integration -- --ignored` | real `nix` |
| Smoke | `scripts/plasma-smoke-test.sh` | a live Plasma session |

Integration tests drive real `nix` against committed fixture flakes and pin the
`diff-closures` output format so a future nix release can't silently break the parser. They
are `#[ignore]`d so a plain `cargo test` works where `nix` is absent.

The smoke test covers what can't be tested headlessly — SNI registration, the D-Bus
service and its caller checks, mid-check responsiveness, icon-name resolution, and the GTK
client. It is strictly read-only and never applies an update:

```console
$ nix build
$ scripts/plasma-smoke-test.sh --bin-dir ./result/bin \
    --flake ~/dev/personal/nixos-config --host nixos-x1
```

Please run it before merging anything that touches the tray, the D-Bus interface, or the
windows. Most of the bugs found in this project so far were invisible to unit tests and CI:
a blank tray icon logs nothing anywhere, and a daemon that ignores its menu looks identical
to a working one until you click.

---

## CI

`ci.yml` runs three jobs on every push and PR:

1. **lint + unit tests** — `rustfmt`, `clippy -D warnings`, unit tests and `shellcheck`, on
   a bare runner with no apt packages (this is what the crate split buys).
2. **nix flake check + build** — builds the GTK client and both Nix modules.
3. **fixture-flake integration** — the real-`nix` tests.

No graphical environment is involved anywhere; the GUI is only ever compiled.

---

## Conventions

- Everything must be config-driven — nothing hardcoded to a particular machine or user.
- Prefer eliminating a failure class structurally over guarding against it. Several fixes
  here took that shape (root doing no file I/O; single-instance windows via GTK rather than
  bookkeeping).
- When a claim about `nix` behaviour matters, verify it against real output and record what
  you saw in a comment or test. Most of the parser's rules exist because the real output
  contradicted a reasonable assumption.
