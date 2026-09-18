#!/bin/sh
# Map a supported Rust target to its MCPB platform; no cross compilation.
set -eu
if [ "$#" -ne 1 ]; then
  echo 'usage: sh tools/mcpb/platform.sh RUST_TARGET' >&2
  exit 1
fi
case "$1" in
  aarch64-apple-darwin | x86_64-apple-darwin) echo darwin ;;
  aarch64-unknown-linux-gnu | x86_64-unknown-linux-gnu | \
  aarch64-unknown-linux-musl | x86_64-unknown-linux-musl) echo linux ;;
  *) echo "platform.sh: unsupported native target: $1" >&2; exit 1 ;;
esac
