#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
if [[ ${1:-} != --worker ]]; then
  pass_env=()
  for var in VITRAL_ANDROID_KEYSTORE VITRAL_ANDROID_KEY_ALIAS VITRAL_ANDROID_KS_PASS VITRAL_ANDROID_KEY_PASS; do
    if [[ -v $var ]]; then pass_env+=(--setenv="$var"); fi
  done
  exec systemd-run --user "${pass_env[@]}" --wait --pipe --collect --working-directory="$root" \
    -p MemoryMax=4G -p MemorySwapMax=512M "$root/scripts/locked-run.sh" "$root/scripts/build-android.sh" --worker
fi
source scripts/env.sh
df -h .
./scripts/fetch-assets.sh --verify
export CARGO_NDK_PLATFORM=29
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-z,max-page-size=16384"
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -o apps/android/app/src/main/jniLibs build --locked --release -p vitral-jni
cd apps/android
development_store="${ANDROID_USER_HOME:-$HOME/.android}/debug.keystore"
tasks=(testDebugUnitTest assembleRelease)
if [[ -z ${VITRAL_ANDROID_KEYSTORE:-} && ! -f $development_store ]]; then
    tasks=(:app:validateSigningDebug "${tasks[@]}")
fi
./gradlew --no-daemon "${tasks[@]}"
release_dir="$root/target/release"
mkdir -p "$release_dir"
stage=$(mktemp -d "$release_dir/vitral-apk.XXXXXX")
trap 'rm -rf -- "$stage"' EXIT
unsigned=app/build/outputs/apk/release/app-release-unsigned.apk
if [[ -n ${VITRAL_ANDROID_KEYSTORE:-} ]]; then
    : "${VITRAL_ANDROID_KEY_ALIAS:?Missing signing key alias}" "${VITRAL_ANDROID_KS_PASS:?Missing keystore password}"
    signing=(--ks "$VITRAL_ANDROID_KEYSTORE" --ks-key-alias "$VITRAL_ANDROID_KEY_ALIAS" --ks-pass env:VITRAL_ANDROID_KS_PASS)
    if [[ -n ${VITRAL_ANDROID_KEY_PASS:-} ]]; then signing+=(--key-pass env:VITRAL_ANDROID_KEY_PASS); fi
else
    signing=(--ks "$development_store" --ks-key-alias androiddebugkey --ks-pass pass:android)
    echo "Signing local release with this Android SDK user's development certificate."
fi
apksigner sign "${signing[@]}" --v4-signing-enabled false --out "$stage/Vitral-android.apk" "$unsigned"
apksigner verify --verbose "$stage/Vitral-android.apk"
zipalign -c -P 16 4 "$stage/Vitral-android.apk"
mv "$stage/Vitral-android.apk" "$release_dir/"
