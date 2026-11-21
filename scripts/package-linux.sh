#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
./scripts/fetch-assets.sh --verify
./scripts/locked-run.sh cargo build --release --locked -p vitral-cli -p vitral-ffi --features vitral-ffi/fcitx-control
./scripts/locked-run.sh cargo build --release --locked --manifest-path apps/desktop-tauri/src-tauri/Cargo.toml
build=linux/fcitx5-vitral/build
./scripts/locked-run.sh cmake -S linux/fcitx5-vitral -B "$build" -DCMAKE_INSTALL_PREFIX=/usr -DCMAKE_BUILD_TYPE=Release -DVITRAL_FFI="$root/target/release/libvitral_ffi.so"
./scripts/locked-run.sh cmake --build "$build" --parallel 2
stage=$(mktemp -d "$root/target/vitral-linux.XXXXXX")
trap 'rm -rf -- "$stage"' EXIT
DESTDIR="$stage" ./scripts/locked-run.sh cmake --install "$build"
install -Dm755 apps/desktop-tauri/src-tauri/target/release/vitral-desktop "$stage/usr/bin/vitral-desktop"
install -Dm755 target/release/vitral "$stage/usr/bin/vitral"
install -Dm644 linux/fcitx5-vitral/vitral-desktop.desktop "$stage/usr/share/applications/vitral-desktop.desktop"
mkdir -p "$stage/usr/share/vitral/assets"
install -d "$stage/usr/share/doc/vitral"
cp -a LICENSE-MIT LICENSE-APACHE licenses "$stage/usr/share/doc/vitral/"
assets="$stage/usr/share/vitral/assets"
cp -a assets/{rime,opencc} "$assets/"
find "$assets" -name '.*.sha256' -delete
install -Dm644 scripts/assets-manifest.json "$assets/assets-manifest.json"
for entry in handwriting/{hw_model.int8.onnx,hw_model.classes.json,cjkvi_ids.txt} reranker/{model_int8.onnx,vocab.txt} voice/sensevoice-small/{model.int8.onnx,tokens.txt,LICENSE}; do
    install -Dm644 "assets/$entry" "$assets/$entry"
done
mkdir -p target/release
tar --zstd -cf "$stage/Vitral-linux-x86_64.tar.zst" -C "$stage" usr
mv "$stage/Vitral-linux-x86_64.tar.zst" target/release/
