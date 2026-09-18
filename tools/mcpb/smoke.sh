#!/bin/sh
# Build and exercise the packed native binary; optionally test an existing bundle.
set -eu
cd "$(dirname "$0")/../.."
sh tools/mcpb/portability.sh
if [ "$#" -gt 1 ]; then
  echo 'usage: sh tools/mcpb/smoke.sh [native-bundle.mcpb]' >&2
  exit 1
fi
if [ "$#" -eq 0 ]; then
  sh tools/mcpb/build.sh
  MCPB_TARGET=$(rustc -vV | sed -n 's/^host: //p')
  MCPB_VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
  set -- "tools/mcpb/build/devondb-$MCPB_VERSION-$MCPB_TARGET.mcpb"
fi
python3 tools/mcpb/smoke.py "$1"
