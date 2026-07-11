#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: ./scripts/build-standalone.sh <rust-target> <os> <native-architecture>

Examples:
  ./scripts/build-standalone.sh x86_64-unknown-linux-gnu linux x86_64
  ./scripts/build-standalone.sh aarch64-unknown-linux-musl linux aarch64
  ./scripts/build-standalone.sh aarch64-apple-darwin macos arm64
USAGE
}

[ "$#" -eq 3 ] || { usage >&2; exit 2; }
TARGET=$1
OS=$2
ARCH=$3
case "$OS" in linux|macos) ;; *) echo "Unix builder supports linux or macos" >&2; exit 2 ;; esac

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
VERSION=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)
[ -n "$VERSION" ] || { echo "Unable to read version from Cargo.toml" >&2; exit 1; }

(cd "$ROOT" && cargo build --locked --release --target "$TARGET" --bin om-agent)
SOURCE="$ROOT/target/$TARGET/release/om-agent"
[ -f "$SOURCE" ] || { echo "Built executable not found: $SOURCE" >&2; exit 1; }
OUTPUT_DIR="$ROOT/dist/standalone"
ARTIFACT="$OUTPUT_DIR/om-agent_${VERSION}_${OS}_${ARCH}.bin"
mkdir -p "$OUTPUT_DIR"
cp "$SOURCE" "$ARTIFACT"
chmod 755 "$ARTIFACT"
if command -v shasum >/dev/null 2>&1; then
  (cd "$OUTPUT_DIR" && shasum -a 256 "$(basename "$ARTIFACT")" > "$(basename "$ARTIFACT").sha256")
else
  (cd "$OUTPUT_DIR" && sha256sum "$(basename "$ARTIFACT")" > "$(basename "$ARTIFACT").sha256")
fi
printf 'Created %s\nCreated %s.sha256\n' "$ARTIFACT" "$ARTIFACT"
