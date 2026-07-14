#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  ./scripts/build-standalone.sh <rust-target> <os> <native-architecture>
  ./scripts/build-standalone.sh all

Environment:
  OM_STANDALONE_BUILDER=auto|cargo|zigbuild
    Select the Cargo builder. The default uses cargo-zigbuild when cross-compiling
    a Linux target and falls back to cargo otherwise.

Examples:
  ./scripts/build-standalone.sh all
  ./scripts/build-standalone.sh x86_64-unknown-linux-gnu linux x86_64
  ./scripts/build-standalone.sh aarch64-unknown-linux-musl linux aarch64
  ./scripts/build-standalone.sh aarch64-apple-darwin macos arm64
  ./scripts/build-standalone.sh x86_64-pc-windows-msvc windows x64
USAGE
}

SUPPORTED_TARGETS=(
  'x86_64-unknown-linux-gnu|linux|x86_64'
  'x86_64-unknown-linux-musl|linux|x86_64-musl'
  'aarch64-unknown-linux-musl|linux|aarch64'
  'armv7-unknown-linux-gnueabihf|linux|arm'
  'i686-unknown-linux-gnu|linux|x86'
  'x86_64-pc-windows-msvc|windows|x64'
  'aarch64-pc-windows-msvc|windows|arm64'
  'i686-pc-windows-msvc|windows|x86'
  'aarch64-apple-darwin|macos|arm64'
  'x86_64-apple-darwin|macos|x86_64'
)

if [ "$#" -eq 1 ] && [ "$1" = all ]; then
  BUILD_ALL=1
elif [ "$#" -eq 3 ]; then
  BUILD_ALL=0
  TARGET=$1
  OS=$2
  ARCH=$3
else
  usage >&2
  exit 2
fi

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
VERSION=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)
[ -n "$VERSION" ] || { echo "Unable to read version from Cargo.toml" >&2; exit 1; }

REQUESTED_BUILDER=${OM_STANDALONE_BUILDER:-auto}
case "$REQUESTED_BUILDER" in
  auto|cargo|zigbuild) ;;
  *) echo "OM_STANDALONE_BUILDER must be auto, cargo, or zigbuild" >&2; exit 2 ;;
esac

HOST_TARGET=$(rustc -vV | sed -n 's/^host: //p')
[ -n "$HOST_TARGET" ] || { echo "Unable to read the host target from rustc" >&2; exit 1; }

OUTPUT_DIR="$ROOT/dist/standalone"

build_target() {
  local target=$1
  local os=$2
  local arch=$3
  local builder=$REQUESTED_BUILDER
  local source artifact extension
  local -a build_command

  case "$os" in
    linux|macos)
      source="$ROOT/target/$target/release/om-agent"
      extension=bin
      ;;
    windows)
      source="$ROOT/target/$target/release/om-agent.exe"
      extension=exe
      ;;
    *)
      echo "Unsupported operating system: $os" >&2
      return 2
      ;;
  esac

  if [ "$builder" = auto ]; then
    if [ "$target" != "$HOST_TARGET" ] && [[ "$target" == *-linux-* ]] \
      && command -v cargo-zigbuild >/dev/null 2>&1 \
      && command -v zig >/dev/null 2>&1; then
      builder=zigbuild
    else
      builder=cargo
    fi
  fi

  if [ "$builder" = zigbuild ]; then
    command -v cargo-zigbuild >/dev/null 2>&1 || {
      echo "cargo-zigbuild is required; install it with: cargo install cargo-zigbuild" >&2
      return 1
    }
    command -v zig >/dev/null 2>&1 || {
      echo "Zig is required by cargo-zigbuild; install Zig and make it available in PATH" >&2
      return 1
    }
    build_command=(cargo zigbuild)
  else
    build_command=(cargo build)
  fi

  printf 'Building %s/%s (%s) with %s\n' "$os" "$arch" "$target" "$builder"
  (cd "$ROOT" && "${build_command[@]}" --locked --release --target "$target" --bin om-agent) || return $?
  [ -f "$source" ] || { echo "Built executable not found: $source" >&2; return 1; }

  artifact="$OUTPUT_DIR/om-agent_${VERSION}_${os}_${arch}.${extension}"
  mkdir -p "$OUTPUT_DIR" || return $?
  cp "$source" "$artifact" || return $?
  chmod 755 "$artifact" || return $?
  if command -v shasum >/dev/null 2>&1; then
    (cd "$OUTPUT_DIR" && shasum -a 256 "$(basename "$artifact")" > "$(basename "$artifact").sha256") || return $?
  else
    (cd "$OUTPUT_DIR" && sha256sum "$(basename "$artifact")" > "$(basename "$artifact").sha256") || return $?
  fi
  printf 'Created %s\nCreated %s.sha256\n' "$artifact" "$artifact"
}

failure_reason() {
  local log=$1
  local status=$2
  local reason

  reason=$(awk '
    /^[[:space:]]*error(\[[^]]+\])?:/ && first == "" { first = $0 }
    /[^[:space:]]/ && $0 !~ /^Building [^ ]+\/[^ ]+ \([^)]*\) with [^ ]+$/ { last = $0 }
    END {
      if (first != "") print first
      else if (last != "") print last
    }
  ' "$log")
  if [ -z "$reason" ]; then
    reason="build command exited with status $status"
  else
    reason="$reason (exit status $status)"
  fi
  printf '%s' "$reason"
}

build_all_targets() {
  local log_dir entry target os arch log status reason index
  local -a failure_platforms=()
  local -a failure_reasons=()

  log_dir=$(mktemp -d "${TMPDIR:-/tmp}/om-agent-build.XXXXXX")
  for entry in "${SUPPORTED_TARGETS[@]}"; do
    IFS='|' read -r target os arch <<< "$entry"
    log="$log_dir/${os}_${arch}.log"
    printf '\n=== %s/%s (%s) ===\n' "$os" "$arch" "$target"
    if build_target "$target" "$os" "$arch" 2>&1 | tee "$log"; then
      printf 'Succeeded: %s/%s (%s)\n' "$os" "$arch" "$target"
    else
      status=$?
      reason=$(failure_reason "$log" "$status")
      failure_platforms+=("$os/$arch ($target)")
      failure_reasons+=("$reason")
      printf 'Failed: %s/%s (%s): %s\n' "$os" "$arch" "$target" "$reason" >&2
    fi
  done
  rm -rf "$log_dir"

  if [ "${#failure_platforms[@]}" -ne 0 ]; then
    printf '\nBuild failures (%s):\n' "${#failure_platforms[@]}" >&2
    for ((index = 0; index < ${#failure_platforms[@]}; index++)); do
      printf '  - %s: %s\n' "${failure_platforms[$index]}" "${failure_reasons[$index]}" >&2
    done
    return 1
  fi

  printf '\nAll %s supported platform builds succeeded.\n' "${#SUPPORTED_TARGETS[@]}"
}

if [ "$BUILD_ALL" -eq 1 ]; then
  build_all_targets
else
  build_target "$TARGET" "$OS" "$ARCH"
fi
