#!/usr/bin/env bash
# Generate distribution manifests (Homebrew / Scoop / winget) for a tagged
# lumux release. Everything here is *derived* from the release version and the
# published GitHub Release assets, so it is generated on demand rather than
# checked in (stale SHA256s in a committed manifest are a classic footgun).
#
# Usage:
#   scripts/gen-packaging.sh [VERSION]
#
# VERSION defaults to the workspace version in Cargo.toml. The script downloads
# the four published `.sha256` sidecars for the matching `v$VERSION` release,
# then writes manifests under dist/packaging/ (gitignored). Copy the relevant
# file into each target repo (homebrew tap, scoop bucket, winget-pkgs fork)
# yourself.
set -euo pipefail

REPO="hgaol/lumux"
OWNER="hgaol"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/dist/packaging"

# --- version: arg, else parse workspace Cargo.toml -------------------------
VERSION="${1:-}"
if [[ -z "$VERSION" ]]; then
  VERSION="$(awk -F'"' '/^\[workspace\.package\]/{p=1} p&&/^version[[:space:]]*=/{print $2; exit}' "$ROOT/Cargo.toml")"
fi
if [[ -z "$VERSION" ]]; then
  echo "error: could not determine version (pass it as the first argument)" >&2
  exit 1
fi
TAG="v$VERSION"
BASE="https://github.com/$REPO/releases/download/$TAG"
echo "Generating packaging manifests for $REPO $TAG"

# --- target triples -> archive ext -----------------------------------------
MAC_ARM="aarch64-apple-darwin"
MAC_X86="x86_64-apple-darwin"
LINUX="x86_64-unknown-linux-gnu"
WIN="x86_64-pc-windows-msvc"

# Fetch a published .sha256 sidecar and echo just the 64-hex digest. Fails the
# whole script if the asset is missing (don't emit a manifest with a blank hash).
fetch_sha() { # $1=target  $2=ext
  local url="$BASE/lumux-$TAG-$1.$2.sha256" sha
  sha="$(curl -fsSL "$url" 2>/dev/null | tr -d '[:space:]' | cut -c1-64 || true)"
  if [[ ! "$sha" =~ ^[0-9a-fA-F]{64}$ ]]; then
    echo "error: could not fetch a valid SHA256 from $url" >&2
    echo "       (is the $TAG release published with that asset + .sha256 sidecar?)" >&2
    exit 1
  fi
  printf '%s' "$sha"
}

echo "Fetching published SHA256 sidecars for $TAG ..."
SHA_MAC_ARM="$(fetch_sha "$MAC_ARM" tar.gz)"
SHA_MAC_X86="$(fetch_sha "$MAC_X86" tar.gz)"
SHA_LINUX="$(fetch_sha "$LINUX" tar.gz)"
SHA_WIN="$(fetch_sha "$WIN" zip)"
SHA_WIN_UPPER="$(printf '%s' "$SHA_WIN" | tr 'a-f' 'A-F')"

DESC="Like tmux, with native Windows support — lightweight, tmux-config-compatible multiplexer"

# --- Homebrew --------------------------------------------------------------
# Layout mirrors the tap repo hgaol/homebrew-lumux: a Formula/ dir holding the
# single formula, overwritten each release.
mkdir -p "$OUT/homebrew/Formula"
cat > "$OUT/homebrew/Formula/lumux.rb" <<EOF
class Lumux < Formula
  desc "$DESC"
  homepage "https://github.com/$REPO"
  version "$VERSION"
  license "MIT"

  on_macos do
    on_arm do
      url "$BASE/lumux-$TAG-$MAC_ARM.tar.gz"
      sha256 "$SHA_MAC_ARM"
    end
    on_intel do
      url "$BASE/lumux-$TAG-$MAC_X86.tar.gz"
      sha256 "$SHA_MAC_X86"
    end
  end

  on_linux do
    on_intel do
      url "$BASE/lumux-$TAG-$LINUX.tar.gz"
      sha256 "$SHA_LINUX"
    end
  end

  def install
    bin.install "lumux"
  end

  test do
    assert_match "lumux #{version}", shell_output("#{bin}/lumux --version")
  end
end
EOF

# --- Scoop -----------------------------------------------------------------
# Layout mirrors the bucket repo hgaol/scoop-lumux: a bucket/ dir holding the
# single manifest, overwritten each release.
mkdir -p "$OUT/scoop/bucket"
cat > "$OUT/scoop/bucket/lumux.json" <<EOF
{
    "version": "$VERSION",
    "description": "$DESC",
    "homepage": "https://github.com/$REPO",
    "license": "MIT",
    "architecture": {
        "64bit": {
            "url": "$BASE/lumux-$TAG-$WIN.zip",
            "hash": "$SHA_WIN"
        }
    },
    "bin": "lumux.exe",
    "checkver": {
        "github": "https://github.com/$REPO"
    },
    "autoupdate": {
        "architecture": {
            "64bit": {
                "url": "https://github.com/$REPO/releases/download/v\$version/lumux-v\$version-$WIN.zip"
            }
        },
        "hash": {
            "url": "\$url.sha256"
        }
    }
}
EOF

# --- winget (version / installer / locale) ---------------------------------
WG="$OUT/winget/manifests/h/$OWNER/lumux/$VERSION"
mkdir -p "$WG"

cat > "$WG/$OWNER.lumux.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.version.1.6.0.schema.json
PackageIdentifier: $OWNER.lumux
PackageVersion: $VERSION
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.6.0
EOF

cat > "$WG/$OWNER.lumux.installer.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.installer.1.6.0.schema.json
PackageIdentifier: $OWNER.lumux
PackageVersion: $VERSION
InstallerType: zip
NestedInstallerType: portable
NestedInstallerFiles:
  - RelativeFilePath: lumux.exe
    PortableCommandAlias: lumux
Commands:
  - lumux
Installers:
  - Architecture: x64
    InstallerUrl: $BASE/lumux-$TAG-$WIN.zip
    InstallerSha256: $SHA_WIN_UPPER
ManifestType: installer
ManifestVersion: 1.6.0
EOF

cat > "$WG/$OWNER.lumux.locale.en-US.yaml" <<EOF
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.defaultLocale.1.6.0.schema.json
PackageIdentifier: $OWNER.lumux
PackageVersion: $VERSION
PackageLocale: en-US
Publisher: $OWNER
PublisherUrl: https://github.com/$OWNER
PublisherSupportUrl: https://github.com/$REPO/issues
PackageName: lumux
PackageUrl: https://github.com/$REPO
License: MIT
LicenseUrl: https://github.com/$REPO/blob/main/LICENSE
ShortDescription: $DESC
Description: |-
  lumux is a lightweight, open-source terminal multiplexer — like tmux, but with
  native Windows support. It offers sessions, windows, and panes with tmux-style
  keybindings, is tmux-config compatible (drop in your ~/.tmux.conf), and attaches
  as plain text over SSH or Microsoft tunnel. Cross-platform on Windows, Linux,
  and macOS.
Moniker: lumux
Tags:
  - tmux
  - terminal
  - multiplexer
  - console
  - conpty
  - cli
  - rust
ManifestType: defaultLocale
ManifestVersion: 1.6.0
EOF

echo "Wrote manifests under $OUT/:"
find "$OUT" -type f | sort | sed "s|$ROOT/|  |"
echo
echo "Copy each into its target repo manually (dirs mirror the repo layout):"
echo "  homebrew/Formula/lumux.rb   -> hgaol/homebrew-lumux  (overwrite Formula/lumux.rb)"
echo "  scoop/bucket/lumux.json     -> hgaol/scoop-lumux     (overwrite bucket/lumux.json)"
echo "  winget/manifests/...        -> fork of microsoft/winget-pkgs, then PR (new version dir)"
