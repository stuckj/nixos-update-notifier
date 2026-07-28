# nixos-update-notifier

A system-tray update notifier for **flake-based NixOS** systems — think "the Ubuntu
update notifier, but for NixOS flakes." It lives in your tray, periodically checks whether
advancing your flake inputs would change your system, shows you a `name: old -> new`
package diff **without downloading anything**, and applies updates on request via an
authenticated `nixos-rebuild`.

> Status: v0.1. The core check/diff/apply mechanics are implemented and the no-download
> check path has been validated against a real flake config (see [Verification](#verification)).

![tray icon states](docs/screenshot-tray.png)
*Placeholder: tray icon showing idle / checking / updates-available states.*

![view updates window](docs/screenshot-updates.png)
*Placeholder: the "View updates" window listing package changes with changelog links.*

---

## Features

- **Native StatusNotifierItem (SNI) tray** via [`ksni`](https://crates.io/crates/ksni) —
  works on **KDE Plasma 6 / Wayland** (legacy X11 XEmbed trays do not).
- Tray icon reflects state: idle / checking / updates-available / error.
- Configurable background check interval; on-demand check via the menu, a `SIGUSR1`, or an
  optional systemd timer.
- Desktop notifications through `org.freedesktop.Notifications` (D-Bus, no `notify-send`).
- Tray menu: **Check now · View updates… · Apply updates · Dismiss until next check ·
  Settings · Quit**.
- **View updates** window (GTK4): each changed package as `name: old -> new` (added /
  removed / upgraded), with a clickable **changelog** link where nixpkgs exposes
  `meta.changelog`.
- **Apply**: copies the candidate `flake.lock` into your repo and runs
  `nixos-rebuild switch` under **polkit/`pkexec`** (never silent `sudo`, never auto-apply),
  with a backup + auto-restore on failure, and a reboot prompt when the kernel/initrd
  changed.
- **Pinned-input exclusion**: an `exclude_inputs` list so deliberately rev-pinned inputs
  (e.g. a `nixpkgs-kernel` that provides kernel + ZFS) are never advanced.
- Everything is **config-driven** — nothing is hardcoded to one machine.

---

## How it works (the crux)

Updating a flake system means advancing `flake.lock` (via `nix flake update <input>…`) and
rebuilding. A *check* must figure out what that would change **without touching your repo
and without downloading package builds**:

1. **Copy, don't mutate.** The flake repo is copied to a throwaway working dir under
   `$XDG_CACHE_HOME`. Your real repo is only ever written to on **Apply**.
2. **Baseline drv.** Evaluate the current `system.build.toplevel.drvPath` from the copy.
   Evaluating `.drvPath` *instantiates* the derivation but does **not** realise it — no
   substitutes are downloaded.
3. **Advance inputs.** Run `nix flake update <inputs>` in the copy (only the inputs you
   configured, minus `exclude_inputs`).
4. **Candidate drv.** Evaluate the toplevel `.drvPath` again. If it equals the baseline →
   no updates. If it differs → updates are available.
5. **Diff, offline.** Run `nix store diff-closures <baseline.drv> <candidate.drv>` over the
   two **derivation** paths and parse the result into a package list. Diffing *derivation*
   closures (not realised outputs) never downloads substitutes.
6. **Changelogs (best-effort).** For changed packages, resolve `meta.changelog` from a
   configurable nixpkgs ref. Coverage in nixpkgs is partial; misses degrade gracefully.

Only **Apply** realises anything (downloads + builds), and only after you click it.

### What "downloads nothing" really means

A check does **not** download or build any package outputs (the GB-scale traffic). It
*does* fetch the **source of the flake inputs you advance** — e.g. advancing `nixpkgs`
fetches the new nixpkgs tree (tens of MB), which is unavoidable because you can't evaluate
against a revision you don't have. This is tiny compared to realising a system closure.

### `diff-closures` reality (and why the list is filtered)

Diffing **derivation** closures is what keeps the check download-free, but the raw output
is messy — validated against a real `nixpkgs` bump:

- It is **ANSI-coloured even when redirected** (we strip escapes).
- Every node is a `.drv`, so upgrades read as `aws-c-http: 0.10.4.drv → 0.11.0.drv` (we
  strip `.drv`).
- It uses **two empty markers**: `∅` (U+2205, absent from the closure) and `ε` (U+03B5,
  present but versionless).
- The closure contains **source tarballs, patch files, CVE-named artefacts and toolchain
  bootstrap stages**, and the name/version heuristic mangles some of them
  (`CVE-2026-…​.patch` → name `CVE`). We filter these out.

After parsing + filtering, what remains is a clean, useful list (in the validated run:
**107** genuine changes like `brave`, `firefox-unwrapped`, `mesa`, `nix`, `libadwaita`,
`ffmpeg`, …). It **does** still include build-time libraries and split derivations (e.g.
the many `nix-*` components), because that is what a derivation-closure diff sees. That is
the honest price of a no-download check.

**Want the pristine runtime list?** `nixos-update-notifier check --exact` builds the
candidate (this **downloads**) and diffs realised **output** closures against
`/run/current-system` for the clean runtime-only view. It is never run in the background.

---

## Requirements

- NixOS managed as a flake, rebuilt with `nixos-rebuild switch --flake <path>#<host>`.
- A running **StatusNotifierHost** (KDE Plasma provides one; on wlroots compositors use
  something like `waybar`).
- `nix` with flakes enabled, `polkit`/`pkexec` for apply.

---

## Install

This flake exposes `packages.<system>.default`, a **home-manager module**, a **NixOS
module**, and a **devShell**.

> **Cargo.lock:** the package builds with `rustPlatform.buildRustPackage` using
> `cargoLock.lockFile = ./Cargo.lock`. Generate it once in the devShell
> (`nix develop -c cargo generate-lockfile`) and commit it before building the package.

### Run from a local clone (before it's published anywhere)

Point another flake at this repo with a path/`git+file` input:

```nix
# in your system flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    home-manager.url = "github:nix-community/home-manager";

    # Local clone as a flake input (either form works):
    nixos-update-notifier.url = "git+file:///home/you/dev/personal/nixos-update-notifier";
    # nixos-update-notifier.url = "path:/home/you/dev/personal/nixos-update-notifier";
    nixos-update-notifier.inputs.nixpkgs.follows = "nixpkgs";
  };
}
```

Later, publishing to GitHub is a one-line change:

```nix
nixos-update-notifier.url = "github:stuckj/nixos-update-notifier";
```

### Home-manager module (primary path — the tray is per-user)

```nix
# home configuration
{ inputs, ... }:
{
  imports = [ inputs.nixos-update-notifier.homeManagerModules.default ];

  services.nixos-update-notifier = {
    enable = true;
    flakePath = "/home/you/dev/personal/nixos-config";
    hostAttr = "nixos-x1";

    # Advance everything EXCEPT the rev-pinned kernel input:
    excludeInputs = [ "nixpkgs-kernel" ];
    # …or allow-list specific inputs instead:
    # updateInputs = [ "nixpkgs" "home-manager" ];

    interval = 21600;      # seconds (6h)
    notify = true;
    # nixpkgsRefForChangelogs = "github:NixOS/nixpkgs/nixos-unstable";

    # Optional extra systemd timer that also pokes a check on a schedule:
    # timer = { enable = true; onCalendar = "hourly"; };
  };
}
```

This installs the binary, writes `~/.config/nixos-update-notifier/config.toml`, and runs a
user systemd service bound to `graphical-session.target` (with a `PATH` that includes the
system `nix`/`nixos-rebuild` and the `pkexec` wrapper).

### NixOS module (optional)

Installs the package system-wide and can grant a trusted user **passwordless** apply:

```nix
{ inputs, ... }:
{
  imports = [ inputs.nixos-update-notifier.nixosModules.default ];

  services.nixos-update-notifier = {
    enable = true;
    # OFF by default → normal polkit admin prompt on each apply (the safe choice).
    # Read the security note below before enabling this.
    # polkit.passwordlessUsers = [ "you" ];
  };
}
```

---

## Security model

Worth understanding before you enable anything, and stated plainly rather than buried.

**Exactly one operation needs root:** `nixos-rebuild switch`, which activates the new
system (new profile generation, activation scripts, bootloader). Everything else runs as
your normal user — checking, diffing, changelog lookups, the tray, notifications, and the
GTK client. Even downloading and building doesn't need app-level root, because `nix` talks
to the (already root-owned) nix-daemon over a socket.

Consequently the privileged helper is deliberately tiny. It takes **no file paths to write
and no pass-through arguments** — only which flake ref to activate:

- The candidate `flake.lock` is backed up and installed by the **unprivileged** daemon.
  Those are your own files, and keeping root out of user-writable directories removes a
  whole class of symlink/TOCTOU problems by construction rather than by careful coding.
- There is deliberately **no `rebuild_extra_args`** option. Free-form arguments reaching
  root's `nixos-rebuild` (`--override-input`, `-I`, `--substituters`, …) would let anything
  that can write your config change what root evaluates — and the polkit prompt doesn't
  display arguments, so you couldn't see it happening.
- A failed or unauthorized apply restores the previous `flake.lock`, leaving the repo as
  it was.

**The inherent limit — please read before enabling `polkit.passwordlessUsers`.** Root
evaluates the flake in your checkout, and that checkout is writable by you. So anyone who
can execute code as your user can edit `flake.nix` and have root run it at the next apply.
That is inherent to "rebuild my system from my flake" — it is the same trust you extend by
running `sudo nixos-rebuild` from a repo you can edit.

The polkit prompt is what makes this safe in practice: you are present and approving.
**Enabling `polkit.passwordlessUsers` removes that check, and is therefore equivalent to
`NOPASSWD` sudo for that user.** It is off by default. Enable it only if you would also be
comfortable granting passwordless root.

---

## Configuration

Generated for you by the home-manager module; if running standalone, copy
[`config.example.toml`](config.example.toml) to
`~/.config/nixos-update-notifier/config.toml`. Key options:

| key | meaning |
|-----|---------|
| `flake_path` | absolute path to the flake repo |
| `host_attr` | the `nixosConfigurations.<name>` to evaluate/rebuild |
| `update_inputs` | inputs to advance (empty = all, minus excludes) |
| `exclude_inputs` | inputs to **never** advance (e.g. `nixpkgs-kernel`) |
| `interval` | check cadence in seconds (min 60) |
| `notify` | fire desktop notifications |
| `nixpkgs_ref_for_changelogs` | nixpkgs ref for `meta.changelog` lookups |

---

## Usage

- **Tray menu** covers everything. Left-click opens **View updates**.
- **CLI** (`nixos-update-notifier` — the daemon + headless commands):

```console
$ nixos-update-notifier check          # one-shot check; prints the diff; downloads nothing
$ nixos-update-notifier check --json   # machine-readable
$ nixos-update-notifier check --exact  # pristine runtime diff (DOWNLOADS: builds candidate)
$ nixos-update-notifier run            # run the tray daemon (default); exports the D-Bus service
```

- **GTK client** (`nixos-update-notifier-gtk` — talks to the running daemon over D-Bus;
  normally launched from the tray, but works standalone too):

```console
$ nixos-update-notifier-gtk updates    # the "View updates" window (live from the daemon)
$ nixos-update-notifier-gtk settings   # the settings editor
```

Trigger an out-of-band check of the running daemon:

```console
$ systemctl --user kill -s SIGUSR1 nixos-update-notifier.service
```

---

## Verification

These are the exact checks used to validate the mechanics (run against a real
`nixos-config` flake with inputs `nixpkgs`, `nixpkgs-kernel` (pinned), `home-manager`,
`sops-nix`, `disko`, `znapzend`, `claude-for-linux`; host `nixos-x1`).

### 1. The check path downloads nothing

```console
# Baseline drv (instantiate-only; watch for copy/download lines — there should be none)
$ nix eval --raw <repo>#nixosConfigurations.<host>.config.system.build.toplevel.drvPath

# In a COPY of the repo, advance an input and re-eval — still no build-output downloads:
$ cp -a --reflink=auto <repo> /tmp/cand && rm -rf /tmp/cand/.git
$ ( cd /tmp/cand && nix flake update home-manager )   # fetches input source only
$ nix eval --raw /tmp/cand#nixosConfigurations.<host>.config.system.build.toplevel.drvPath
```
Confirmed: no `copying path …`/`downloaded`/`substituting` lines during either eval. (A
cold eval of a full system took ~36 s; subsequent evals hit the nix eval cache.)

### 2. The offline diff

```console
$ nix store diff-closures <baseline.drv> <candidate.drv>
```
Confirmed download-free. Note the raw output is ANSI-coloured, `.drv`-suffixed, and uses
`∅`/`ε`; the app strips/normalises/filters it (see [How it works](#diff-closures-reality-and-why-the-list-is-filtered)).
The pure parser is unit-tested against captured real output:

```console
$ nix develop -c cargo test          # runs the parser/config/changelog unit tests
```

### 3. Changelog rendering

`meta.changelog` coverage is partial. Verified present: `firefox`, `git`, `ripgrep`,
`brave`. Verified absent: `vlc`, `obs-studio`. Check a package by hand:

```console
$ nix eval --json nixpkgs#firefox.meta.changelog
$ nix eval --json nixpkgs#vlc.meta.changelog     # errors/absent → no link shown
```

### 4. Applying safely

- Prefer a **VM** first: `nixos-rebuild build-vm --flake <repo>#<host>` and boot it.
- Or dry-run the activation: `nixos-rebuild dry-activate --flake <repo>#<host>`.
- The privileged helper backs up `flake.lock` to `flake.lock.bak.<epoch>` and **restores
  it automatically** if `nixos-rebuild` fails.
- Reboot is offered only when kernel/initrd/kernel-modules/systemd changed between
  `/run/booted-system` and `/run/current-system`.

### 5. Smoke test on a real Plasma session

The tray/SNI, notifications, D-Bus service, and GTK client can't be exercised headlessly.
Run the guided, **read-only** (never applies) smoke test from inside your Plasma 6 session:

```console
$ nix build
$ scripts/plasma-smoke-test.sh --bin-dir ./result/bin \
    --flake ~/dev/personal/nixos-config --host nixos-x1
# or against an already-running daemon (systemd user service):
$ scripts/plasma-smoke-test.sh
```

It checks the SNI host + notification daemon, that the daemon owns `org.nixos.UpdateNotifier`
and answers `GetStatus`/`GetUpdates`, that the tray item is registered, and that the GTK
client opens and connects — with a PASS/FAIL summary.

---

## Gotchas (things that bit us)

- **SNI on KDE Wayland:** you must use a native StatusNotifierItem. `ksni` (used here)
  speaks the SNI D-Bus protocol; X11 XEmbed trays silently don't appear on Plasma 6
  Wayland. The daemon needs a running StatusNotifierHost — under a bare systemd user
  service make sure it's ordered after `graphical-session.target` (the HM module does).
- **`diff-closures` over `.drv` paths is noisy and coloured.** It emits ANSI escapes even
  when not a TTY, suffixes every token with `.drv`, uses `∅` (absent) vs `ε` (versionless),
  and carries source/patch/toolchain artefacts with mangled names. Budget for parsing +
  filtering (done here) — don't expect a clean list from the raw command.
- **"No download" ≠ "no network."** Advancing an input fetches that input's *source*
  (small); only package *builds* are avoided. Slow-link users still pay the input-source
  fetch, not the closure.
- **Rev bump with zero package changes.** Advancing e.g. `home-manager` can change the
  system `.drv` without changing any package version — the tool reports "system update
  available (no package version changes)" rather than an empty "0 updates."
- **`pkexec` + wrappers.** The binary is `makeBinaryWrapper`-wrapped, so it re-execs its
  `.…-wrapped` path; the generated polkit rule matches the package `bin/` prefix rather
  than an exact filename.
- **Cargo.lock must be committed** for the Nix package to build reproducibly.

---

## Architecture

Three crates in a Cargo workspace, split so the logic and the daemon carry **no GUI
dependency** — which is what lets CI test them on a bare runner with no system libraries
(ksni 0.3 / zbus / tokio are pure Rust; only the GTK crate links C libraries):

- **`nun-core`** (lib) — config, nix orchestration, the offline diff parser, changelog
  resolution, and the privileged apply. No GTK, no ksni. Fully unit-tested.
- **`nixos-update-notifier`** (bin) — the long-lived **daemon/service**: the SNI tray, the
  periodic checker, notifications, and a D-Bus interface (`org.nixos.UpdateNotifier1`) it
  exports on the session bus.
- **`nixos-update-notifier-gtk`** (bin) — the **GTK client**. Launched independently (from
  the tray or by hand), it drives the daemon purely over D-Bus (`CheckNow`, `Apply`,
  `GetUpdates`, `GetStatus`) and repaints from the daemon's state — so the updater runs as
  a standalone service and the GUI is a thin client, not a spawned data dump.

```
crates/
  core/   src/{config,nix,check,diff,changelog,apply}.rs, lib.rs   +  tests/integration.rs
  daemon/ src/{main,daemon,tray,notify,state,dbus}.rs
  gtk/    src/{main,ui,client}.rs
nix/      package.nix, hm-module.nix, nixos-module.nix
scripts/  set-version.sh (release), plasma-smoke-test.sh (manual Plasma smoke test)
.github/  workflows/{ci,release}.yml
```

Releasing (stable + canary) is documented in [`RELEASING.md`](RELEASING.md).

## Development

```console
$ nix develop                        # rust toolchain + gtk4 + pkg-config + nvd
$ cargo test -p nun-core             # pure-logic unit tests (no system deps)
$ cargo clippy --workspace -- -D warnings
$ cargo run -p nixos-update-notifier -- check    # exercise the no-download check
$ nix build                          # build both binaries via the flake
# Integration tests that shell out to real `nix` (guarded behind #[ignore]):
$ cargo test -p nun-core --test integration -- --ignored
```

## Continuous integration & releases

- **`ci.yml`** runs three tiers on every push/PR: (1) `rustfmt` + `clippy -D warnings` +
  unit tests for `nun-core`/`nixos-update-notifier` on a bare runner (no apt packages);
  (2) `nix flake check` + `nix build` (builds the GTK client + both Nix modules, runs the
  in-sandbox suite); (3) the fixture-flake **integration tests** that drive real `nix`
  invocations and lock the `diff-closures` output format against version drift.
- **`release.yml`** is a manual `workflow_dispatch` (`version`, optional `commit`) modelled
  on the mkvdup flow (PRs #200 + #207). A `sync-version` job writes the version into the
  source **on the released ref** (so the tag reports its own version); `build` builds from
  that commit; `release` creates the **tag and GitHub release together** (via
  `target_commitish`) *only after the build passes*, so a failed build never leaves a
  dangling tag. A version containing `-canary.` (e.g. `1.2.0-canary.1`) is a **pre-release**.
  - **Canaries are cut from development branches** — the bump + tag land on that branch.
    There's no canary channel to publish: with a flake the ref is the selector, so a canary
    installs straight from its tag or branch,
    `nix profile install 'github:stuckj/nixos-update-notifier/<branch-or-tag>#default'`.
  - There is **no vendorHash to refresh** (`cargoLock.lockFile` vendors from the committed
    `Cargo.lock`), so the Go-modules hash-refresh machinery from #207 isn't needed here.
  - Full details in [`RELEASING.md`](RELEASING.md).

---

## License

MIT — see [LICENSE](LICENSE).
