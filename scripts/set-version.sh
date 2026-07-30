#!/usr/bin/env bash
# Write a release version into the source tree, so the tagged commit reports the version
# on the tin (`--version`, and the Nix store path). Shared by the release workflow and
# usable by hand.
#
# Rust note: unlike a Go module's vendorHash, there is nothing hash-shaped to refresh here.
# `nix/package.nix` uses `cargoLock.lockFile = ../Cargo.lock`, which vendors deterministically
# from the committed lockfile — so a canary cut from any branch builds as long as Cargo.lock
# is committed (it always travels with the branch). This script only rewrites the version.
#
# Usage: scripts/set-version.sh <version>   e.g. scripts/set-version.sh 1.2.0
set -euo pipefail

VERSION="${1:?usage: set-version.sh <version>}"

# Same shape the release workflow validates: bare semver-ish, no leading 'v'.
if [[ ! "$VERSION" =~ ^[0-9]+(\.[0-9]+)*([.-][0-9A-Za-z]+)*$ ]]; then
  echo "error: invalid version '$VERSION' (expected e.g. 1.2.3 or 1.2.3-canary.1)" >&2
  exit 1
fi

root="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cargo_toml="$root/Cargo.toml"
package_nix="$root/nix/package.nix"

# 1) Workspace crate version (feeds every crate via `version.workspace = true`, hence the
#    binaries' `--version`). Scope the substitution to the [workspace.package] table so we
#    never touch dependency versions in [workspace.dependencies].
sed -i -E '/^\[workspace\.package\]/,/^\[/ s/^version = ".*"/version = "'"$VERSION"'"/' "$cargo_toml"

# 2) Nix package version (the store-path label).
sed -i -E 's/^  version = "[^"]*";/  version = "'"$VERSION"'";/' "$package_nix"

# 3) Keep Cargo.lock's workspace-member entries in sync so an offline `nix build` doesn't
#    trip over a version mismatch. `--workspace` relocks only the workspace members; a
#    version-only bump like this does not upgrade registry dependencies (verified: it
#    reports "Locking N packages" for the members and leaves the rest unchanged).
if command -v cargo >/dev/null 2>&1; then
  (cd "$root" && cargo update --workspace >/dev/null 2>&1) || true
fi

# Verify the edits actually landed.
grep -qE "^version = \"$VERSION\"" <(sed -n '/^\[workspace\.package\]/,/^\[/p' "$cargo_toml") \
  || { echo "error: Cargo.toml version not updated" >&2; exit 1; }
grep -q "version = \"$VERSION\";" "$package_nix" \
  || { echo "error: package.nix version not updated" >&2; exit 1; }

echo "set version to $VERSION (Cargo.toml, nix/package.nix, Cargo.lock)"
