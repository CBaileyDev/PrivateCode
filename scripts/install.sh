#!/usr/bin/env bash
set -euo pipefail
# Private Code CLI installer.
# Downloads a release binary for this host's target triple — artifact names MUST
# match .github/workflows/release.yml (private-code-<target-triple>[.exe]).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/CBaileyDev/PrivateCode/main/scripts/install.sh | bash
#   PRIVATE_CODE_VERSION=v0.1.0 ./scripts/install.sh   # pin a tag (recommended)
#   PRIVATE_CODE_INSTALL_DIR=~/.local/bin ./scripts/install.sh  # no sudo
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
    echo "Unsupported OS: $OS (Windows: download private-code-x86_64-pc-windows-msvc.exe from GitHub Releases)" >&2
    exit 1
    ;;
esac

BIN_NAME="private-code-${TARGET}"
INSTALL_DIR="${PRIVATE_CODE_INSTALL_DIR:-/usr/local/bin}"
VERSION="${PRIVATE_CODE_VERSION:-}"

if [[ -n "$VERSION" ]]; then
  BASE="https://github.com/${REPO}/releases/download/${VERSION}"
else
  BASE="https://github.com/${REPO}/releases/latest/download"
fi

URL="${BASE}/${BIN_NAME}"
SHA_URL="${URL}.sha256"

echo "Downloading Private Code CLI (${TARGET}) from ${URL}..."
TMP="$(mktemp)"
trap 'rm -f "$TMP" "${TMP}.sha256"' EXIT

curl -fSL "$URL" -o "$TMP"

if curl -fsSL "$SHA_URL" -o "${TMP}.sha256" 2>/dev/null; then
  EXPECTED="$(awk '{print $1}' "${TMP}.sha256")"
  ACTUAL="$(sha256sum "$TMP" | awk '{print $1}')"
  if [[ "$EXPECTED" != "$ACTUAL" ]]; then
    echo "SHA256 mismatch: expected $EXPECTED, got $ACTUAL" >&2
    exit 1
  fi
  echo "SHA256 verified."
else
  echo "Warning: no ${SHA_URL} found — install continues without checksum verification." >&2
fi

chmod +x "$TMP"
mkdir -p "$INSTALL_DIR"
if [[ -w "$INSTALL_DIR" ]]; then
  mv "$TMP" "${INSTALL_DIR}/private-code"
else
  sudo mv "$TMP" "${INSTALL_DIR}/private-code"
fi
trap - EXIT
echo "Installed private-code to ${INSTALL_DIR}/private-code"
echo "Run: private-code selftest   # offline engine smoke (no API key)"
