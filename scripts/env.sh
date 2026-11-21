# Source this file in bash/zsh: source scripts/env.sh
# Safe to source multiple times (idempotent PATH updates).

_vitral_prepend_path() {
  local dir="$1"
  [ -d "$dir" ] || return 0
  case ":${PATH}:" in
    *":${dir}:"*) ;;
    *) export PATH="${dir}:${PATH}" ;;
  esac
}

export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"

# Portable Temurin JDK 17 (user-space primary) with fallbacks
if [ -z "${JAVA_HOME:-}" ]; then
  if [ -d "$HOME/.local/jdks/jdk-17" ]; then
    export JAVA_HOME="$HOME/.local/jdks/jdk-17"
  elif [ -d "$HOME/.local/opt/jdk-17" ]; then
    export JAVA_HOME="$HOME/.local/opt/jdk-17"
  elif [ -d "/usr/lib/jvm/java-17-openjdk" ]; then
    export JAVA_HOME="/usr/lib/jvm/java-17-openjdk"
  fi
fi

export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
export ANDROID_SDK_ROOT="${ANDROID_SDK_ROOT:-$ANDROID_HOME}"

# Prefer pinned NDK r27 LTS, else newest installed NDK under $ANDROID_HOME/ndk
if [ -z "${ANDROID_NDK_HOME:-}" ] && [ -d "$ANDROID_HOME/ndk" ]; then
  if [ -d "$ANDROID_HOME/ndk/27.3.13750724" ]; then
    export ANDROID_NDK_HOME="$ANDROID_HOME/ndk/27.3.13750724"
  else
    _latest_ndk="$(ls -1d "$ANDROID_HOME/ndk/"* 2>/dev/null | sort -V | tail -n 1 || true)"
    [ -n "$_latest_ndk" ] && export ANDROID_NDK_HOME="$_latest_ndk"
    unset _latest_ndk
  fi
fi

# cargo-xwin Windows MSVC CRT & SDK cache
export XWIN_CACHE_DIR="${XWIN_CACHE_DIR:-$HOME/.cache/cargo-xwin}"
export XWIN_ARCH="${XWIN_ARCH:-x86_64}"
export XWIN_CROSS_COMPILER="${XWIN_CROSS_COMPILER:-clang-cl}"

# Prevent Corepack interactive download prompts in scripts/CI
export COREPACK_ENABLE_DOWNLOAD_PROMPT=0

# Host OOM-prevention caps (15 GiB RAM host shared with desktop & IDE)
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}"
export RUST_TEST_THREADS="${RUST_TEST_THREADS:-1}"
export OMP_NUM_THREADS="${OMP_NUM_THREADS:-1}"
export ORT_INTRA_OP_NUM_THREADS="${ORT_INTRA_OP_NUM_THREADS:-1}"
export ORT_INTER_OP_NUM_THREADS="${ORT_INTER_OP_NUM_THREADS:-1}"
export RAYON_NUM_THREADS="${RAYON_NUM_THREADS:-2}"
export NODE_OPTIONS="${NODE_OPTIONS:---max-old-space-size=1536}"
export GRADLE_OPTS="${GRADLE_OPTS:--Dorg.gradle.jvmargs=-Xmx1280m -Dorg.gradle.daemon=false -Dorg.gradle.workers.max=2}"

_vitral_prepend_path "$HOME/.local/bin"
_vitral_prepend_path "$CARGO_HOME/bin"
[ -n "${JAVA_HOME:-}" ] && _vitral_prepend_path "$JAVA_HOME/bin"
_vitral_prepend_path "$ANDROID_HOME/cmdline-tools/latest/bin"
_vitral_prepend_path "$ANDROID_HOME/platform-tools"
if [ -n "${ANDROID_HOME:-}" ] && [ -d "$ANDROID_HOME/build-tools" ]; then
  _latest_bt="$(ls -1d "$ANDROID_HOME/build-tools/"* 2>/dev/null | sort -V | tail -n 1 || true)"
  [ -n "$_latest_bt" ] && _vitral_prepend_path "$_latest_bt"
  unset _latest_bt
fi

unset -f _vitral_prepend_path
