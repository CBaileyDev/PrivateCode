#!/usr/bin/env bash
set -euo pipefail
# Private Code CLI installer (Phase 5, Step 5.12).
# Downloads the release binary for this host's target triple — the names MUST
# match the artifacts produced by .github/workflows/release.yml
# (private-code-<target-triple>).
REPO="CBaileyDev/PrivateCode"
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Darwin)
    case "$ARCH" in
      arm64 | aarch64) TARGET="aarch64-apple-darwin" ;;
      x86_64) TARGET="x86_64-apple-darwin" ;;
      *) echo "Unsupported macOS arch: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  Linux)
    case "$ARCH" in
      aarch64 | arm64) TARGET="aarch64-unknown-linux-gnu" ;;
      x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
      *) echo "Unsupported Linux arch: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  *)
    echo "Unsupported OS: $OS (Windows users: download the .exe from the releases page)" >&2
    exit 1
    ;;
esac

URL="https://github.com/${REPO}/releases/latest/download/private-code-${TARGET}"
echo "Downloading Private Code CLI (${TARGET}) from ${URL}..."
TMP="$(mktemp)"
curl -fSL "$URL" -o "$TMP"
chmod +x "$TMP"
sudo mv "$TMP" /usr/local/bin/private-code
echo "Installed private-code to /usr/local/bin/private-code"
