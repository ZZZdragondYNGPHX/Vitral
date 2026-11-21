#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

VERIFY_ONLY=0
SKIP_XWIN_CACHE=0
for arg in "$@"; do
  case "$arg" in
    --verify-only) VERIFY_ONLY=1 ;;
    --skip-xwin-cache) SKIP_XWIN_CACHE=1 ;;
    -h|--help)
      echo "Usage: $0 [--verify-only] [--skip-xwin-cache]"
      exit 0
      ;;
    *)
      echo "Unknown option: $arg" >&2
      exit 2
      ;;
  esac
done

# Pin tested versions/IDs
JDK17_URL="https://api.adoptium.net/v3/binary/latest/17/ga/linux/x64/jdk/hotspot/normal/eclipse"
JDK17_DIR="$HOME/.local/jdks/jdk-17"
CMDLINE_TOOLS_URL="https://dl.google.com/android/repository/commandlinetools-linux-15859902_latest.zip"
ANDROID_PLATFORM_PKG="platforms;android-35"
ANDROID_BUILD_TOOLS_PKG="build-tools;35.0.0"
ANDROID_NDK_PKG="ndk;27.3.13750724"
PNPM_VERSION="11.9.0"

RUST_TARGETS=(
  "x86_64-unknown-linux-gnu"
  "aarch64-linux-android"
  "armv7-linux-androideabi"
  "x86_64-linux-android"
  "x86_64-pc-windows-msvc"
)

# Load current paths
# shellcheck source=scripts/env.sh
source "$SCRIPT_DIR/env.sh"

verify_all() {
  local fail=0
  check_cmd() {
    local label="$1" cmd="$2"
    if command -v "$cmd" >/dev/null 2>&1; then
      printf '[OK]   %-24s %s\n' "$label" "$(command -v "$cmd")"
    else
      printf '[MISS] %-24s (%s not in PATH)\n' "$label" "$cmd"
      fail=1
    fi
  }

  check_cmd "rustup" rustup
  check_cmd "rustc" rustc
  check_cmd "cargo" cargo
  check_cmd "cargo-binstall" cargo-binstall
  check_cmd "cargo-ndk" cargo-ndk
  check_cmd "cargo-xwin" cargo-xwin
  check_cmd "cargo-tauri" cargo-tauri
  check_cmd "cbindgen" cbindgen
  check_cmd "node" node
  check_cmd "pnpm" pnpm
  check_cmd "clang" clang
  check_cmd "clang++" clang++
  check_cmd "lld-link" lld-link
  check_cmd "llvm-rc" llvm-rc
  check_cmd "llvm-lib" llvm-lib
  check_cmd "java" java
  check_cmd "javac" javac
  check_cmd "sdkmanager" sdkmanager
  check_cmd "adb" adb

  if [ -x "$JDK17_DIR/bin/javac" ]; then
    printf '[OK]   %-24s %s\n' "temurin-jdk17" "$JDK17_DIR"
  else
    printf '[MISS] %-24s (%s/bin/javac missing)\n' "temurin-jdk17" "$JDK17_DIR"
    fail=1
  fi

  if command -v rustup >/dev/null 2>&1; then
    local installed_targets
    installed_targets="$(rustup target list --installed 2>/dev/null || true)"
    for t in "${RUST_TARGETS[@]}"; do
      if grep -qx "$t" <<<"$installed_targets"; then
        printf '[OK]   rust-target:%-12s installed\n' "$t"
      else
        printf '[MISS] rust-target:%-12s missing\n' "$t"
        fail=1
      fi
    done
  fi

  for sdk_dir in \
    "$ANDROID_HOME/cmdline-tools/latest" \
    "$ANDROID_HOME/platform-tools" \
    "$ANDROID_HOME/platforms/android-35" \
    "$ANDROID_HOME/build-tools/35.0.0" \
    "$ANDROID_HOME/ndk/27.3.13750724"; do
    if [ -d "$sdk_dir" ]; then
      printf '[OK]   android-sdk             %s\n' "$sdk_dir"
    else
      printf '[MISS] android-sdk             %s\n' "$sdk_dir"
      fail=1
    fi
  done

  if [ -d "$XWIN_CACHE_DIR/xwin/crt" ] && [ -d "$XWIN_CACHE_DIR/xwin/sdk" ]; then
    printf '[OK]   xwin-cache              %s/xwin\n' "$XWIN_CACHE_DIR"
  else
    printf '[MISS] xwin-cache              %s/xwin\n' "$XWIN_CACHE_DIR"
    fail=1
  fi

  return "$fail"
}

if [ "$VERIFY_ONLY" -eq 1 ]; then
  verify_all
  exit $?
fi

# 1. Rustup + stable + targets (clean any orphaned 0-byte .partial files)
if [ -d "$RUSTUP_HOME/downloads" ]; then
  find "$RUSTUP_HOME/downloads" -maxdepth 1 -name '*.partial' -size 0 -delete
fi
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --default-toolchain stable --profile default
fi
# shellcheck source=scripts/env.sh
source "$SCRIPT_DIR/env.sh"

rustup toolchain install stable --profile default
rustup default stable
rustup target add "${RUST_TARGETS[@]}"

# 2. Cargo tools via cargo-binstall (fallback to cargo install --locked)
if ! command -v cargo-binstall >/dev/null 2>&1; then
  curl -fsSL https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash
fi

ensure_cargo_tool() {
  local bin_name="$1" crate_name="$2"
  if command -v "$bin_name" >/dev/null 2>&1; then
    echo "[skip] $bin_name already installed"
    return 0
  fi
  cargo binstall -y "$crate_name" || cargo install --locked "$crate_name"
}

ensure_cargo_tool cargo-ndk cargo-ndk
ensure_cargo_tool cargo-xwin cargo-xwin
ensure_cargo_tool cargo-tauri tauri-cli
ensure_cargo_tool cbindgen cbindgen

# 3. pnpm via user-space corepack into ~/.local/bin (fallback: npm -g with ~/.local prefix)
mkdir -p "$HOME/.local/bin"
if ! command -v pnpm >/dev/null 2>&1; then
  if [ -x "$HOME/.local/bin/corepack" ]; then
    "$HOME/.local/bin/corepack" enable pnpm --install-directory "$HOME/.local/bin"
    COREPACK_ENABLE_DOWNLOAD_PROMPT=0 "$HOME/.local/bin/corepack" prepare "pnpm@${PNPM_VERSION}" --activate
  elif command -v corepack >/dev/null 2>&1; then
    corepack enable pnpm --install-directory "$HOME/.local/bin"
    COREPACK_ENABLE_DOWNLOAD_PROMPT=0 corepack prepare "pnpm@${PNPM_VERSION}" --activate
  else
    npm install -g "pnpm@${PNPM_VERSION}"
  fi
fi

# 4. Portable Temurin JDK 17 in ~/.local/jdks/jdk-17
if [ ! -x "$JDK17_DIR/bin/javac" ]; then
  tmp_jdk="$(mktemp -d)"
  trap 'rm -rf "$tmp_jdk"' EXIT
  curl -fsSL "$JDK17_URL" -o "$tmp_jdk/jdk17.tar.gz"
  mkdir -p "$JDK17_DIR"
  tar -xzf "$tmp_jdk/jdk17.tar.gz" --strip-components=1 -C "$JDK17_DIR"
  rm -rf "$tmp_jdk"
  trap - EXIT
fi
export JAVA_HOME="$JDK17_DIR"

# 5. Android Command-Line Tools + SDK + NDK
if [ ! -x "$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager" ]; then
  tmp_sdk="$(mktemp -d)"
  trap 'rm -rf "$tmp_sdk"' EXIT
  curl -fsSL "$CMDLINE_TOOLS_URL" -o "$tmp_sdk/cmdline-tools.zip"
  unzip -q "$tmp_sdk/cmdline-tools.zip" -d "$tmp_sdk/unpacked"
  mkdir -p "$ANDROID_HOME/cmdline-tools/latest"
  cp -a "$tmp_sdk/unpacked/cmdline-tools/." "$ANDROID_HOME/cmdline-tools/latest/"
  rm -rf "$tmp_sdk"
  trap - EXIT
fi

# shellcheck source=scripts/env.sh
source "$SCRIPT_DIR/env.sh"

SDKMANAGER="$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager"
{
  set +o pipefail
  yes 2>/dev/null | "$SDKMANAGER" --sdk_root="$ANDROID_HOME" --licenses >/dev/null || true
  set -o pipefail
}
"$SDKMANAGER" --sdk_root="$ANDROID_HOME" \
  "platform-tools" \
  "$ANDROID_PLATFORM_PKG" \
  "$ANDROID_BUILD_TOOLS_PKG" \
  "$ANDROID_NDK_PKG"

# Re-source env.sh so ANDROID_NDK_HOME resolves newly installed NDK
# shellcheck source=scripts/env.sh
source "$SCRIPT_DIR/env.sh"

# 6. Pre-populate cargo-xwin MSVC CRT & Windows SDK cache
if [ "$SKIP_XWIN_CACHE" -eq 0 ]; then
  if [ ! -d "$XWIN_CACHE_DIR/xwin/crt" ] || [ ! -d "$XWIN_CACHE_DIR/xwin/sdk" ]; then
    mkdir -p "$XWIN_CACHE_DIR"
    XWIN_CACHE_DIR="$XWIN_CACHE_DIR" XWIN_ARCH="x86_64" cargo xwin cache xwin
  fi
fi

verify_all
