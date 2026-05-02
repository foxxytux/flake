#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
BIN_DIR="${HOME}/.local/bin"
BIN_PATH="${BIN_DIR}/flake"

mkdir -p "$BIN_DIR"
cd "$ROOT"
cargo build --release
install -m 755 "$ROOT/target/release/flake" "$BIN_PATH"

printf 'Installed flake to %s\n' "$BIN_PATH"
