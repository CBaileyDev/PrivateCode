#!/usr/bin/env bash
set -euo pipefail
# Private Code CLI installer (Phase 5, Step 5.12)
REPO="CBaileyDev/PrivateCode"
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64) ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *) echo "Unsupported arch: $ARCH"; exit 1 ;;
esac
URL="https://github.com/${REPO}/releases/latest/download/private-code-${ARCH}-unknown-${OS}-gnu"
echo "Downloading Private Code CLI from ${URL}..."
curl -fsSL "$URL" -o /tmp/private-code
chmod +x /tmp/private-code
sudo mv /tmp/private-code /usr/local/bin/private-code
echo "Installed private-code to /usr/local/bin/private-code"
