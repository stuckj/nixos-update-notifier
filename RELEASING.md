# Releasing

Releases are cut by the **Release** workflow (`.github/workflows/release.yml`), a manual
`workflow_dispatch`. The design follows [`stuckj/mkvdup`](https://github.com/stuckj/mkvdup)
(PRs #200 and #207), adapted for a Rust + Nix flake.

## Stable release

1. Make sure `main` is green and at the commit you want to ship.
2. Actions → **Release** → *Run workflow*, with **branch = `main`** and
   **version = `1.2.0`** (no `v` prefix).
3. The workflow:
   - `prepare` validates the version and checks the **remote** for an existing `v1.2.0` tag.
   - `sync-version` runs `scripts/set-version.sh 1.2.0` (writes the version into
     `Cargo.toml`, `nix/package.nix`, and `Cargo.lock`), commits it to `main` as
     `Release v1.2.0 [skip ci]`, and outputs that commit.
   - `build` builds the package from the bumped commit and assembles a tarball.
   - `release` creates the **tag `v1.2.0` at the bumped commit** and the GitHub release —
     so the tag only exists once the build has passed, and it carries its own version.

Install a stable release from the flake:

```console
$ nix profile install github:stuckj/nixos-update-notifier/v1.2.0#default
# or pin it as a flake input:
#   inputs.nixos-update-notifier.url = "github:stuckj/nixos-update-notifier/v1.2.0";
```

## Canary release

Canaries exist to test **unmerged branch code** with a real binary, so they are cut **from
a development branch**, not from `main`.

1. Push your work to a branch (e.g. `feat/foo`).
2. Actions → **Release** → *Run workflow*, with **branch = `feat/foo`** and
   **version = `1.2.0-canary.1`**.
3. Same flow as above, except `sync-version` commits the bump to **`feat/foo`** and the tag
   `v1.2.0-canary.1` is created there. Because the version contains `-canary.`, the GitHub
   release is marked **pre-release**.

There is **no canary channel to publish** — with a Nix flake the ref *is* the selector, so
a canary installs directly from its tag or even its branch:

```console
$ nix profile install github:stuckj/nixos-update-notifier/v1.2.0-canary.1#default
# straight from the branch, no tag needed:
$ nix profile install 'github:stuckj/nixos-update-notifier/feat/foo#default'
```

## Notes

- **No vendorHash to refresh.** `nix/package.nix` uses `cargoLock.lockFile = ../Cargo.lock`,
  which vendors deterministically from the committed lockfile. A dependency bump updates
  `Cargo.lock`, which travels with the branch — so a canary from any branch builds without a
  separate hash-refresh job (the problem PR #207's `nix-canary-hash.yml` solves for Go
  modules simply does not arise here). Just keep `Cargo.lock` committed.
- **Releasing an older commit.** You can pass an explicit `commit` input. If it isn't the
  branch head, `sync-version` does **not** write a version bump (it would land on the head,
  not on your commit); the tag is created at exactly that commit, which then reports its
  existing in-tree version.
- **Branch protection.** `sync-version` pushes the bump commit to the released branch using
  the default `GITHUB_TOKEN`. If `main` is protected against Actions pushes, either allow
  it for this workflow or cut stable releases in a way that permits the bump commit.
- `scripts/set-version.sh <version>` can also be run by hand to bump the version locally.
