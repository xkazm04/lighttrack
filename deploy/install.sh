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

# Check the asset exists before piping into tar. Without this, a missing build makes curl 404 while
# `tar` reports "does not look like a tar archive" — the pipeline's status is tar's, so the real
# cause is invisible and people go hunting the wrong bug. This bit us for real: no release before
# v0.0.7 shipped an x86_64-apple-darwin build (its CI leg used a retired runner), so every Intel Mac
# install died on that confusing tar error.
code="$(curl -sSL -o /dev/null -w '%{http_code}' -I "$url" || echo 000)"
if [ "$code" != "200" ]; then
  echo "no published build for ${target} (HTTP ${code})" >&2
  echo "  tried: ${url}" >&2
  echo "  LightTrack publishes: x86_64-unknown-linux-gnu, x86_64-apple-darwin," >&2
  echo "                        aarch64-apple-darwin, x86_64-pc-windows-msvc (deploy/install.ps1)." >&2
  echo "  If your platform is on that list, the latest release is incomplete — please report it at" >&2
  echo "  https://github.com/${REPO}/issues. Otherwise build from source: cargo build --release --bins" >&2
  exit 1
fi

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
