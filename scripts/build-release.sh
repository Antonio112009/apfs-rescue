#!/bin/sh
# Release build without local paths (home folder, user name) inside the binary.
set -e
cd "$(dirname "$0")/.."
CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
RUSTFLAGS="--remap-path-prefix=$CARGO_HOME_DIR=cargo --remap-path-prefix=$PWD=apfs-rescue $RUSTFLAGS" \
  cargo build --release "$@"
