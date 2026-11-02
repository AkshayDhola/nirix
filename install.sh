#!/usr/bin/env bash
set -euo pipefail

OWNER="AkshayDhola"
REPO="nirix"
BINARY="nirix"
ASSET="${BINARY}-linux-x86_64.tar.gz"

DOWNLOAD_URL="https://github.com/${OWNER}/${REPO}/releases/latest/download/${ASSET}"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

echo "Downloading latest ${BINARY} release..."

curl -fL "$DOWNLOAD_URL" -o "$TMP_DIR/app.tar.gz"

tar -xzf "$TMP_DIR/app.tar.gz" -C "$TMP_DIR"

if [[ ! -f "$TMP_DIR/$BINARY" ]]; then
    echo "Error: binary not found in release archive"
    exit 1
fi

chmod +x "$TMP_DIR/$BINARY"

# Install globally if possible; otherwise use user's local bin.
if [[ -w /usr/local/bin ]]; then
    INSTALL_DIR="/usr/local/bin"
elif command -v sudo >/dev/null 2>&1; then
    INSTALL_DIR="/usr/local/bin"
    sudo install -m 755 "$TMP_DIR/$BINARY" "$INSTALL_DIR/$BINARY"
    echo "Installed ${BINARY} to ${INSTALL_DIR}"
    exit 0
else
    INSTALL_DIR="${HOME}/.local/bin"
    mkdir -p "$INSTALL_DIR"
fi

install -m 755 "$TMP_DIR/$BINARY" "$INSTALL_DIR/$BINARY"

echo "Successfully installed ${BINARY}"
echo "Location: ${INSTALL_DIR}/${BINARY}"
