#!/usr/bin/env bash
# Install the latest ccc release binary for this OS/arch.
#   curl -fsSL https://raw.githubusercontent.com/shivamhwp/ccc/main/scripts/install.sh | bash
set -euo pipefail

REPO="shivamhwp/ccc"
BINDIR="${CCC_INSTALL_DIR:-$HOME/.local/bin}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Darwin) case "$arch" in
            arm64) target="aarch64-apple-darwin" ;;
            x86_64) target="x86_64-apple-darwin" ;;
            *) echo "unsupported macOS arch: $arch" >&2; exit 1 ;;
          esac ;;
  Linux)  case "$arch" in
            x86_64) target="x86_64-unknown-linux-musl" ;;
            *) echo "unsupported Linux arch: $arch (build from source)" >&2; exit 1 ;;
          esac ;;
  *) echo "unsupported OS: $os (on Windows use the .zip from Releases)" >&2; exit 1 ;;
esac

tag="${CCC_VERSION:-$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
  | grep -m1 '"tag_name"' | cut -d'"' -f4)}"
[ -n "$tag" ] || { echo "could not determine latest release tag" >&2; exit 1; }

asset="ccc-$target.tar.gz"
url="https://github.com/$REPO/releases/download/$tag/$asset"
echo "Downloading $asset ($tag)…"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fsSL "$url" -o "$tmp/$asset"

# Verify checksum if published alongside.
if curl -fsSL "$url.sha256" -o "$tmp/$asset.sha256" 2>/dev/null; then
  ( cd "$tmp" && (shasum -a 256 -c "$asset.sha256" >/dev/null 2>&1 \
      || sha256sum -c "$asset.sha256" >/dev/null 2>&1) ) \
    && echo "checksum ok" || { echo "checksum verification failed" >&2; exit 1; }
fi

tar xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$BINDIR"
install -m 0755 "$tmp/ccc-$target/ccc" "$BINDIR/ccc"

echo "Installed ccc to $BINDIR/ccc"
case ":$PATH:" in
  *":$BINDIR:"*) ;;
  *) echo "note: add $BINDIR to your PATH" ;;
esac
echo "Next: ccc setup"
