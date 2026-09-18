#!/bin/sh
# Build a native, size-gated MCP bundle. See tools/mcpb/README.md.
set -eu
cd "$(dirname "$0")/../.."
MCPB_ROOT=$(pwd)
MCPB_DIR="$MCPB_ROOT/tools/mcpb"
MCPB_BUILD="$MCPB_DIR/build"
MCPB_TARGET=$(rustc -vV | sed -n 's/^host: //p')
MCPB_PLATFORM=$(sh "$MCPB_DIR/platform.sh" "$MCPB_TARGET")
MCPB_VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
case "$MCPB_VERSION" in
  *[!0-9.]* | "") echo 'build.sh: invalid workspace version' >&2; exit 1 ;;
esac

# Explicit target and output directory prevent environment/Cargo configuration
# from silently packaging a foreign target or an older target/dist binary.
cargo build --locked --profile dist -p devondb-cli \
  --target "$MCPB_TARGET" --target-dir "$MCPB_ROOT/target"
MCPB_BIN="$MCPB_ROOT/target/$MCPB_TARGET/dist/devondb"
MCPB_BYTES=$(wc -c < "$MCPB_BIN" | tr -d '[:space:]')
if [ "$MCPB_BYTES" -gt 10485760 ]; then
  echo "build.sh: binary exceeds 10 MiB edge budget: $MCPB_BYTES bytes" >&2
  exit 1
fi
if [ "$("$MCPB_BIN" --version)" != "devondb $MCPB_VERSION" ]; then
  echo 'build.sh: native binary version does not match workspace' >&2
  exit 1
fi

mkdir -p "$MCPB_BUILD"
MCPB_STAGE=$(mktemp -d "$MCPB_BUILD/stage.XXXXXX")
trap 'rm -rf "$MCPB_STAGE"' 0
trap 'exit 1' HUP INT TERM
mkdir -p "$MCPB_STAGE/server/$MCPB_TARGET"
sed -e "s/__VERSION__/$MCPB_VERSION/g" \
  -e "s/__TARGET__/$MCPB_TARGET/g" -e "s/__PLATFORM__/$MCPB_PLATFORM/g" \
  "$MCPB_DIR/manifest.json" > "$MCPB_STAGE/manifest.json"
cp "$MCPB_BIN" "$MCPB_STAGE/server/$MCPB_TARGET/devondb"
chmod 755 "$MCPB_STAGE/server/$MCPB_TARGET/devondb"
cp LICENSE "$MCPB_STAGE/LICENSE"
cp crates/devondb-geo/THIRD_PARTY_LICENSES.md "$MCPB_STAGE/THIRD_PARTY_LICENSES.md"
MCPB_BUNDLE="$MCPB_BUILD/devondb-$MCPB_VERSION-$MCPB_TARGET.mcpb"
(cd "$MCPB_STAGE" && zip -q -r -X bundle.zip manifest.json server LICENSE THIRD_PARTY_LICENSES.md)
mv "$MCPB_STAGE/bundle.zip" "$MCPB_BUNDLE"
echo "built: $MCPB_BUNDLE"
echo "stripped binary bytes: $MCPB_BYTES (limit: 10485760)"
echo "bundle bytes: $(wc -c < "$MCPB_BUNDLE" | tr -d '[:space:]')"
