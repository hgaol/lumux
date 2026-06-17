#!/usr/bin/env bash
# Publish all lumux crates to crates.io.
#
#   scripts/publish-cargo.sh            # dry-run, then prompt before uploading
#   scripts/publish-cargo.sh --yes      # skip the prompt (for CI / non-interactive)
#   scripts/publish-cargo.sh --dry-run  # verify only, never upload
#
# Uses `cargo publish --workspace`, which orders the crates by their dependency
# graph and uploads them in one shot (cargo 1.83+). Publishing is IRREVERSIBLE —
# a version can be yanked but never deleted, and never re-uploaded — so this runs
# a dry-run and asks for confirmation first.
#
# Prerequisites:
#   - `cargo login` has stored a crates.io token (or pass CARGO_REGISTRY_TOKEN).
#   - The release is tagged and pushed (see scripts/bump-version.sh); crates.io
#     should match a real release.
set -euo pipefail

CARGO="${CARGO:-cargo}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

err()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
note() { printf '\033[1;33m%s\033[0m\n' "$*"; }

DRY_ONLY=0
ASSUME_YES=0
for a in "$@"; do
  case "$a" in
    --dry-run) DRY_ONLY=1 ;;
    --yes|-y)  ASSUME_YES=1 ;;
    *) err "unknown argument: $a (use --dry-run or --yes)" ;;
  esac
done

VERSION="$(awk -F'"' '/^\[workspace.package\]/{p=1} p&&/^version[[:space:]]*=/{print $2; exit}' Cargo.toml)"
[ -n "$VERSION" ] || err "could not read the workspace version from Cargo.toml"

# --- guard: internal dependency pins must match the workspace version ------
# A hand-edit that bumps only [workspace.package] leaves the `crates/*` pins
# behind; cargo publish then fails partway through. Catch it before uploading
# anything (a partial publish can't be undone).
if grep -E 'path = "crates\/' Cargo.toml | grep -qv "version = \"$VERSION\""; then
  echo "internal dependency pins are not all at $VERSION:" >&2
  grep -nE 'path = "crates\/' Cargo.toml >&2
  err "realign them first (scripts/bump-version.sh $VERSION) before publishing"
fi

# --- guard: clean tree -----------------------------------------------------
git diff --quiet && git diff --cached --quiet \
  || err "working tree not clean — commit or stash first (publish should match a tagged release)"

note "Publishing lumux $VERSION to crates.io"

# --- dry run (always) ------------------------------------------------------
echo "Running dry-run (build + package, no upload)..."
"$CARGO" publish --workspace --dry-run

if [ "$DRY_ONLY" -eq 1 ]; then
  echo "Dry-run only — nothing uploaded."
  exit 0
fi

# --- confirm (irreversible) ------------------------------------------------
if [ "$ASSUME_YES" -ne 1 ]; then
  printf '\n\033[1;31mThis uploads lumux %s to crates.io and CANNOT be undone.\033[0m\n' "$VERSION"
  printf 'Type the version (%s) to confirm: ' "$VERSION"
  read -r reply
  [ "$reply" = "$VERSION" ] || err "confirmation did not match — aborted"
fi

# --- publish ---------------------------------------------------------------
echo "Publishing all crates (dependency order handled by --workspace)..."
"$CARGO" publish --workspace

echo
note "Published lumux $VERSION."
echo "Verify:  https://crates.io/crates/lumux/$VERSION"
echo "Docs build asynchronously at https://docs.rs/lumux/$VERSION (a few minutes)."
