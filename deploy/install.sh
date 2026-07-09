#!/usr/bin/env sh
# LightTrack installer (Linux/macOS). Usage:
#   curl -fsSL https://raw.githubusercontent.com/xkazm04/lighttrack/main/deploy/install.sh | sh
# Override the install dir with LIGHTTRACK_BIN_DIR=/usr/local/bin.
set -eu

REPO="xkazm04/lighttrack"
BINDIR="${LIGHTTRACK_BIN_DIR:-$HOME/.local/bin}"

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)  o="unknown-linux-gnu" ;;
  Darwin) o="apple-darwin" ;;
  *) echo "unsupported OS: $os (use deploy/install.ps1 on Windows)" >&2; exit 1 ;;
esac
case "$arch" in
  x86_64|amd64)  a="x86_64" ;;
  arm64|aarch64) a="aarch64" ;;
  *) echo "unsupported arch: $arch" >&2; exit 1 ;;
esac
if [ "$o" = "unknown-linux-gnu" ] && [ "$a" != "x86_64" ]; then
  echo "only x86_64 is published for Linux" >&2; exit 1
fi

target="${a}-${o}"
url="https://github.com/${REPO}/releases/latest/download/lighttrack-${target}.tar.gz"
echo "downloading ${url}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fsSL "$url" | tar -xz -C "$tmp"

mkdir -p "$BINDIR"
for b in lt lt-runner lt-mcp lighttrack-api; do
  mv "$tmp/$b" "$BINDIR/$b"
  chmod +x "$BINDIR/$b"
done

echo "installed lt, lt-runner, lt-mcp, lighttrack-api to ${BINDIR}"
case ":$PATH:" in
  *":$BINDIR:"*) ;;
  *) echo "add it to your PATH:  export PATH=\"$BINDIR:\$PATH\"" ;;
esac
