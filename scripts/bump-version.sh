#!/usr/bin/env bash
# Bump the lumux release version, refresh the lockfile, and commit + tag it.
#
#   scripts/bump-version.sh X.Y.Z
#
# Rewrites the workspace version *and* the four internal dependency pins in
# Cargo.toml — `cargo publish` fails if those drift from the workspace version,
# and it's the step that's easiest to forget (a hand edit that bumps only the
# workspace version leaves the pins behind). Then it runs a build (which both
# refreshes Cargo.lock and proves the pins resolve), verifies `lumux --version`
# reports the new value, and creates a `chore(release): vX.Y.Z` commit + tag.
#
# `lumux --version` itself needs no edit — it derives from CARGO_PKG_VERSION at
# compile time. Pushing the tag (which triggers the release CI) is left to you:
#   git push && git push origin vX.Y.Z
set -euo pipefail

CARGO="${CARGO:-cargo}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

err() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

NEW="${1:-}"
[ -n "$NEW" ] || { echo "usage: scripts/bump-version.sh X.Y.Z" >&2; exit 1; }

# --- validate --------------------------------------------------------------
echo "$NEW" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$' \
  || err "VERSION '$NEW' is not semver (X.Y.Z)"

CUR="$(awk -F'"' '/^\[workspace.package\]/{p=1} p&&/^version[[:space:]]*=/{print $2; exit}' Cargo.toml)"
[ -n "$CUR" ] || err "could not read the current version from Cargo.toml"
[ "$CUR" != "$NEW" ] || err "already at $NEW"

git diff --quiet && git diff --cached --quiet \
  || err "working tree not clean — commit or stash first"

# --- rewrite the version strings -------------------------------------------
# Two independent rewrites, because a prior hand-edit may have bumped the
# workspace version while leaving the internal pins behind (they can hold a
# DIFFERENT old value than $CUR):
#   1. the bare workspace `version = "..."` line  -> $NEW
#   2. every internal dep pin `... path = "crates/..." , version = "..."` -> $NEW
echo "Bumping workspace $CUR -> $NEW (and realigning internal pins)"
sed -i -E "s/^version = \".*\"/version = \"$NEW\"/" Cargo.toml
sed -i -E "/path = \"crates\//s/version = \"[^\"]*\"/version = \"$NEW\"/" Cargo.toml

# Sanity-check exactly the expected lines now read $NEW (1 workspace + 4 pins).
moved="$(grep -cE "version = \"$NEW\"" Cargo.toml || true)"
[ "$moved" -ge 5 ] || err "expected >=5 version strings at $NEW, found $moved — check Cargo.toml"
# And no internal pin is left stranded at a different version.
if grep -E 'path = "crates\/' Cargo.toml | grep -qv "version = \"$NEW\""; then
  err "an internal crates/* pin is not at $NEW — check Cargo.toml"
fi

# --- refresh lockfile + prove the pins resolve -----------------------------
echo "Building to refresh Cargo.lock and verify the pins resolve..."
"$CARGO" build --workspace

reported="$("$CARGO" run -q -p lumux -- --version 2>/dev/null || true)"
echo "  lumux reports: ${reported:-<unknown>}"
case "$reported" in
  *"$NEW"*) ;;
  *) err "lumux --version reported '$reported', expected to contain $NEW" ;;
esac

# --- commit + tag ----------------------------------------------------------
git add Cargo.toml
git ls-files --error-unmatch Cargo.lock >/dev/null 2>&1 && git add Cargo.lock || true
git commit -m "chore(release): v$NEW"
git tag "v$NEW"

echo
echo "Tagged v$NEW. To trigger the release build, push it:"
echo "    git push && git push origin v$NEW"
