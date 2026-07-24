#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  ./scripts/build-standalone.sh <rust-target> <os> <native-architecture>
  ./scripts/build-standalone.sh all

Environment:
  OM_STANDALONE_BUILDER=auto|cargo|zigbuild|xwin
    Select the Cargo builder. The default uses cargo-zigbuild for GNU/Linux and
    cross-compiled Linux targets, cargo-xwin when cross-compiling an MSVC Windows
    target, and falls back to cargo otherwise.

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
  auto|cargo|zigbuild|xwin) ;;
  *) echo "OM_STANDALONE_BUILDER must be auto, cargo, zigbuild, or xwin" >&2; exit 2 ;;
esac

HOST_TARGET=$(rustc -vV | sed -n 's/^host: //p')
[ -n "$HOST_TARGET" ] || { echo "Unable to read the host target from rustc" >&2; exit 1; }

OUTPUT_DIR="$ROOT/dist/standalone"
MINIMUM_GLIBC_VERSION=2.17

required_glibc_version() {
  LC_ALL=C strings "$1" | awk '
    {
      line = $0
      while (match(line, /GLIBC_[0-9]+\.[0-9]+/)) {
        version = substr(line, RSTART + 6, RLENGTH - 6)
        split(version, parts, ".")
        major = parts[1] + 0
        minor = parts[2] + 0
        if (!found || major > max_major || (major == max_major && minor > max_minor)) {
          max_major = major
          max_minor = minor
        }
        found = 1
        line = substr(line, RSTART + RLENGTH)
      }
    }
    END {
      if (found) printf "%d.%d", max_major, max_minor
    }
  '
}

verify_glibc_baseline() {
  local target=$1
  local executable=$2
  local required required_major required_minor minimum_major minimum_minor

  [[ "$target" == *-linux-gnu* ]] || return 0
  required=$(required_glibc_version "$executable")
  [ -n "$required" ] || {
    echo "Unable to determine the required glibc version for $executable" >&2
    return 1
  }
  IFS=. read -r required_major required_minor <<< "$required"
  IFS=. read -r minimum_major minimum_minor <<< "$MINIMUM_GLIBC_VERSION"
  if (( required_major > minimum_major \
    || (required_major == minimum_major && required_minor > minimum_minor) )); then
    echo "$executable requires glibc $required, newer than the supported baseline $MINIMUM_GLIBC_VERSION" >&2
    return 1
  fi
  printf 'Verified glibc baseline: requires %s (maximum %s)\n' \
    "$required" "$MINIMUM_GLIBC_VERSION"
}

build_target() {
  local target=$1
  local os=$2
  local arch=$3
  local builder=$REQUESTED_BUILDER
  local source artifact extension xwin_tool_path cargo_target
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
    if [ "$target" != "$HOST_TARGET" ] && [[ "$target" == *-windows-msvc ]] \
      && command -v cargo-xwin >/dev/null 2>&1 \
      && command -v clang >/dev/null 2>&1 \
      && { command -v llvm-lib >/dev/null 2>&1 || command -v zig >/dev/null 2>&1; }; then
      builder=xwin
    elif [[ "$target" == *-linux-gnu* ]] \
      && command -v cargo-zigbuild >/dev/null 2>&1 \
      && command -v zig >/dev/null 2>&1; then
      builder=zigbuild
    elif [[ "$target" == *-linux-gnu* ]]; then
      echo "cargo-zigbuild and Zig are required to build $target against glibc $MINIMUM_GLIBC_VERSION" >&2
      return 1
    elif [ "$target" != "$HOST_TARGET" ] && [[ "$target" == *-linux-* ]] \
      && command -v cargo-zigbuild >/dev/null 2>&1 \
      && command -v zig >/dev/null 2>&1; then
      builder=zigbuild
    else
      builder=cargo
    fi
  fi
  if [[ "$target" == *-linux-gnu* ]] && [ "$builder" != zigbuild ]; then
    echo "$target must be built with cargo-zigbuild to enforce the glibc $MINIMUM_GLIBC_VERSION baseline" >&2
    return 1
  fi

  xwin_tool_path=
  if [ "$builder" = xwin ]; then
    command -v cargo-xwin >/dev/null 2>&1 || {
      echo "cargo-xwin is required; install it with: cargo install --locked cargo-xwin" >&2
      return 1
    }
    command -v clang >/dev/null 2>&1 || {
      echo "Clang is required by cargo-xwin; install LLVM and make clang available in PATH" >&2
      return 1
    }
    if ! command -v llvm-lib >/dev/null 2>&1; then
      command -v zig >/dev/null 2>&1 || {
        echo "llvm-lib or Zig is required by cargo-xwin" >&2
        return 1
      }
      xwin_tool_path="$ROOT/scripts/xwin-tools"
    fi
    build_command=(cargo xwin build)
  elif [ "$builder" = zigbuild ]; then
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

  cargo_target=$target
  if [ "$builder" = zigbuild ] && [[ "$target" == *-linux-gnu* ]]; then
    cargo_target="$target.$MINIMUM_GLIBC_VERSION"
  fi

  printf 'Building %s/%s (%s) with %s\n' "$os" "$arch" "$cargo_target" "$builder"
  if [ "$builder" = xwin ]; then
    (
      cd "$ROOT"
      PATH="${xwin_tool_path:+$xwin_tool_path:}$PATH" \
        XWIN_CROSS_COMPILER="${XWIN_CROSS_COMPILER:-clang}" \
        "${build_command[@]}" --locked --release --target "$cargo_target" --bin om-agent
    ) || return $?
  else
    (cd "$ROOT" && "${build_command[@]}" --locked --release --target "$cargo_target" --bin om-agent) || return $?
  fi
  [ -f "$source" ] || { echo "Built executable not found: $source" >&2; return 1; }
  verify_glibc_baseline "$target" "$source" || return $?

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
