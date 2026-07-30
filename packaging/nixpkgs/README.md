# Submitting to nixpkgs

Everything here is **prepared, not submitted**. `package.nix` builds successfully as written
(verified against nixpkgs master via `callPackage`), but it is pinned to a commit rather than
a release tag, and the maintainer entry is deliberately blank. Both are on you — see below.

## Read this first: nixpkgs' automation policy

nixpkgs does not forbid AI-assisted contributions, but it does require two things, quoted
from its `CONTRIBUTING.md`:

> Every contribution to Nixpkgs and related development venues, including code,
> documentation, and communication on GitHub and Matrix, must have a **responsible person in
> the loop** who is accountable for that contribution and reviews it before submission, and
> must **transparently disclose** any non-trivial use of automation to produce it, including
> but not limited to LLM-based AI tools.

Two consequences that shape how this gets submitted:

1. **You are the accountable party, and you must actually review it.** The policy is explicit
   that for LLM output, confidence in the tool's logic is *not* enough — the material needs
   manual review or programmatic verification. Read `package.nix` and understand every line
   before opening a PR. If a reviewer asks why something is the way it is, that answer has to
   be yours.
2. **Disclose it in the commit.** The policy asks for an `Assisted-by:` trailer naming the
   tool and model version:

   ```
   Assisted-by: Claude Code (claude-opus-5)
   ```

   Do not use `Co-Authored-By:` for this — that asserts authorship, which is a different
   claim than disclosure, and it is not what the policy asks for.

## Prerequisites

### 1. Cut a release tag

nixpkgs packages a *versioned* source, and `version = "0-unstable-<date>"` with a commit rev
is the convention for packaging something that has no release yet. It is accepted, but a real
tag is better: it gives `nix-update` something to follow and reviewers something stable.

The release workflow is `workflow_dispatch`-only, so nothing is tagged automatically:

```console
$ gh workflow run release.yml -f version=0.1.0
```

Then update `package.nix`:

```nix
  version = "0.1.0";

  src = fetchFromGitHub {
    owner = "stuckj";
    repo = "nixos-update-notifier";
    tag = "v0.1.0";           # replaces `rev = "<sha>"`
    hash = "";                # recompute — see below
  };
```

Recompute both hashes; the source hash changes because the archive contents change, and the
vendor hash changes with any dependency change:

```console
$ nix-prefetch-url --unpack https://github.com/stuckj/nixos-update-notifier/archive/v0.1.0.tar.gz
$ nix hash convert --hash-algo sha256 --to sri <base32-output>
# then set cargoHash to lib.fakeHash, build, and copy the "got:" hash from the error
```

### 2. Add yourself to the maintainer list

`meta.maintainers` is required for new packages and is intentionally left `[ ]` here — adding
a person to `maintainers/maintainer-list.nix` is a claim about that person, so it should be
made by them. In your nixpkgs checkout, add an entry and set:

```nix
  maintainers = with lib.maintainers; [ your-handle ];
```

## Submitting

```console
$ git clone https://github.com/NixOS/nixpkgs   # or your fork
$ mkdir -p pkgs/by-name/ni/nixos-update-notifier
$ cp package.nix pkgs/by-name/ni/nixos-update-notifier/package.nix
```

The `ni` shard is the lowercased first two letters of the attribute name, which is how
`pkgs/by-name` is organised.

Verify before opening anything:

```console
$ nix-build -A nixos-update-notifier
$ nix-shell -p nixpkgs-review --run "nixpkgs-review rev HEAD"
```

Commit message — the `pname: init at version` prefix is not just convention, it triggers
CI builds:

```
nixos-update-notifier: init at 0.1.0

System-tray update notifier for flake-based NixOS, in the spirit of Ubuntu's
update-notifier: periodic checks that download nothing (it compares derivation
closures rather than realising them), desktop notifications, and an apply that
goes through polkit.

Assisted-by: Claude Code (claude-opus-5)
```

## Things a reviewer is likely to raise

Worth having answers ready, because these are the non-obvious parts of the derivation:

- **`dontWrapGApps = true` plus a manual `wrapProgram`.** The two binaries need different
  things: the GTK client needs the GApps environment (icon themes, GSettings schemas), while
  the daemon only needs `coreutils` on PATH. Letting the hook wrap both would double-wrap the
  daemon for no reason.
- **`nix` and `nixos-rebuild` are deliberately NOT dependencies.** They must come from the
  running system, or the tool would evaluate and rebuild with a different nix than the one
  that owns the store. This is a correctness requirement, not an oversight.
- **The GUI is a separate binary in the same output.** The daemon re-execs its sibling from
  its own `bin/`, which is why both live in one package rather than being split.
- **Tests are not run in the build.** The unit tests pass in CI on a bare runner; the
  integration tests need a real `nix` and are `#[ignore]`d, so they cannot run in the sandbox.
  If a reviewer wants `checkPhase` enabled, `cargo test -p nun-core -p nixos-update-notifier`
  is the sandbox-safe subset.
- **A NixOS/home-manager module is not part of this submission.** Upstreaming the module is a
  separate, larger review (it would live in `nixos/modules/`), and the flake modules in this
  repo remain the supported path meanwhile.
