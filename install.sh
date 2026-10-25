#!/bin/sh
# Build nirix and install it to ~/.local/bin (override with BIN_DIR=...).
set -eu

cd "$(dirname "$0")"

if ! command -v cargo >/dev/null 2>&1; then
    echo "nirix: cargo not found. Install Rust first: https://rustup.rs" >&2
    exit 1
fi

BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/nirix"

cargo build --release --locked

mkdir -p "$BIN_DIR"
cp target/release/nirix "$BIN_DIR/nirix"
chmod 755 "$BIN_DIR/nirix"
echo "installed $BIN_DIR/nirix"

# never overwrite a config the user already has
if [ ! -f "$CONFIG_DIR/config.toml" ]; then
    mkdir -p "$CONFIG_DIR"
    cp config.example.toml "$CONFIG_DIR/config.toml"
    echo "created $CONFIG_DIR/config.toml"
fi

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "note: $BIN_DIR is not in PATH; add it to your shell profile" ;;
esac
