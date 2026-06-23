#!/bin/sh
# lumux installer — Linux & macOS.
#
#   curl -fsSL https://hgaol.github.io/lumux/scripts/install.sh | sh
#
# Downloads the latest lumux release binary for your OS/arch from GitHub,
# verifies its SHA256 against the published .sha256 sidecar, and installs it
# to ~/.local/bin (override with LUMUX_INSTALL_DIR). No build, no root needed.
set -eu

REPO="hgaol/lumux"
INSTALL_DIR="${LUMUX_INSTALL_DIR:-$HOME/.local/bin}"

say()  { printf '\033[1;32m%s\033[0m\n' "$*"; }
info() { printf '  %s\n' "$*"; }
err()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# --- detect platform -------------------------------------------------------
os="$(uname -s)"; arch="$(uname -m)"
case "$os" in
  Linux)
    case "$arch" in
      x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
      *) err "no prebuilt Linux binary for '$arch' yet. Install from source: cargo install lumux" ;;
    esac ;;
  Darwin)
    case "$arch" in
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      x86_64)        target="x86_64-apple-darwin" ;;
      *) err "unknown macOS arch '$arch'" ;;
    esac ;;
  *) err "unsupported OS '$os'. On Windows use install.ps1; otherwise: cargo install lumux" ;;
esac
ext="tar.gz"

# --- resolve latest version tag --------------------------------------------
have() { command -v "$1" >/dev/null 2>&1; }
have curl || err "curl is required"
have tar  || err "tar is required"

say "Resolving the latest lumux release…"
tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep -m1 '"tag_name"' | cut -d'"' -f4)"
[ -n "${tag:-}" ] || err "could not determine the latest release tag"
version="${tag#v}"
info "latest is $tag for $target"

asset="lumux-$tag-$target.$ext"
base="https://github.com/$REPO/releases/download/$tag"

# --- download + verify in a temp dir ---------------------------------------
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
say "Downloading $asset…"
curl -fsSL "$base/$asset"        -o "$tmp/$asset"        || err "download failed"
curl -fsSL "$base/$asset.sha256" -o "$tmp/$asset.sha256" || err "checksum download failed"

say "Verifying checksum…"
expected="$(cut -c1-64 < "$tmp/$asset.sha256")"
if have sha256sum;   then actual="$(sha256sum   "$tmp/$asset" | cut -c1-64)"
elif have shasum;    then actual="$(shasum -a 256 "$tmp/$asset" | cut -c1-64)"
else err "need sha256sum or shasum to verify the download"; fi
[ "$expected" = "$actual" ] || err "checksum mismatch — expected $expected, got $actual"
info "ok ($actual)"

# --- unpack + install ------------------------------------------------------
tar -xzf "$tmp/$asset" -C "$tmp"
bin="$(find "$tmp" -type f -name lumux -perm -u+x | head -n1)"
[ -n "$bin" ] || bin="$tmp/lumux-$tag-$target/lumux"
[ -f "$bin" ] || err "could not find the lumux binary inside the archive"

mkdir -p "$INSTALL_DIR"
install -m755 "$bin" "$INSTALL_DIR/lumux" 2>/dev/null || { cp "$bin" "$INSTALL_DIR/lumux"; chmod 755 "$INSTALL_DIR/lumux"; }

say "Installed lumux $version -> $INSTALL_DIR/lumux"

# --- persist PATH + the 'lm' alias to your shell rc ------------------------
# So a NEW terminal finds lumux on PATH and gets the short `lm` alias. Write to
# the rc of the active shell (fall back to ~/.profile); a marker block keeps
# re-runs idempotent. The alias line carries a comment so the user can drop it.
case "${SHELL:-}" in
  */zsh)  rc="$HOME/.zshrc" ;;
  */bash) rc="$HOME/.bashrc" ;;
  *)      rc="$HOME/.profile" ;;
esac
# Show $HOME as a literal $HOME in the written PATH line for readability.
case "$INSTALL_DIR" in
  "$HOME"/*) path_entry="\$HOME/${INSTALL_DIR#"$HOME"/}" ;;
  *)         path_entry="$INSTALL_DIR" ;;
esac
if ! grep -qsF "# >>> lumux >>>" "$rc"; then
  {
    printf '\n# >>> lumux >>>\n'
    printf 'export PATH="%s:$PATH"\n' "$path_entry"
    printf "alias lm=lumux   # comment out this line (prepend #) to use the full 'lumux' command\n"
    printf '# <<< lumux <<<\n'
  } >> "$rc" && info "added PATH + the 'lm' alias to $rc"
else
  info "$rc already has the lumux block (left unchanged)"
fi

printf '\nOpen a new terminal — or run \033[1;32m. %s\033[0m — to pick up PATH and the\n' "$rc"
printf '\033[1;32mlm\033[0m alias, then start a session:  \033[1;32mlm new -s work\033[0m\n'
