#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
df -h .
./scripts/fetch-assets.sh --verify
target=x86_64-pc-windows-msvc
./scripts/locked-run.sh cargo xwin build --release --locked --target "$target" -p vitral-tsf
./scripts/locked-run.sh cargo xwin build --release --locked --target "$target" --manifest-path apps/desktop-tauri/src-tauri/Cargo.toml
cache=${VITRAL_ASSET_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/vitral/assets}
mkdir -p "$cache" target/release
wheel="$cache/onnxruntime-win-1.23.2.whl"
digest=25de5214923ce941a3523739d34a520aac30f21e631de53bba9174dc9c004435
stage=$(mktemp -d "$root/target/vitral-windows.XXXXXX")
part=
trap 'rm -rf -- "$stage"; if [[ -n $part ]]; then rm -f -- "$part"; fi' EXIT
verify_wheel() { echo "$digest  $1" | sha256sum --check --status; }
if [[ ! -f $wheel ]] || ! verify_wheel "$wheel"; then
    part=$(mktemp "$cache/onnxruntime-win.XXXXXX")
    curl --fail --location --retry 3 -o "$part" https://files.pythonhosted.org/packages/c0/b4/569d298f9fc4d286c11c45e85d9ffa9e877af12ace98af8cab52396e8f46/onnxruntime-1.23.2-cp312-cp312-win_amd64.whl
    verify_wheel "$part"
    mv "$part" "$wheel"
    part=
fi
python - "$stage/Vitral-windows-x86_64.zip" "$wheel" <<'PY'
import sys, zipfile
from pathlib import Path
target = Path("target/x86_64-pc-windows-msvc/release")
with zipfile.ZipFile(sys.argv[1], "w", zipfile.ZIP_DEFLATED, compresslevel=5) as out, zipfile.ZipFile(sys.argv[2]) as runtime:
    for filename in ("onnxruntime.dll", "onnxruntime_providers_shared.dll"):
        out.writestr(filename, runtime.read("onnxruntime/capi/" + filename))
    for source in [Path("LICENSE-MIT"), Path("LICENSE-APACHE"), *sorted(Path("licenses").glob("*"))]:
        out.write(source)
    out.write(target / "vitral_tsf.dll", "vitral_tsf.dll")
    out.write(Path("apps/desktop-tauri/src-tauri") / target / "vitral-desktop.exe", "vitral-desktop.exe")
    out.write("docs/windows.md", "README.md")
    out.write("scripts/assets-manifest.json", "assets/assets-manifest.json")
    out.write("scripts/models-catalog.json", "assets/models-catalog.json")
    assets = Path("assets")
    for name in ("rime/cn_dicts/8105.dict.yaml", "rime/cn_dicts/41448.dict.yaml",
                 "rime/cn_dicts/base.dict.yaml", "rime/cn_dicts/ext.dict.yaml", "rime/cn_dicts/others.dict.yaml",
                 "rime/LICENSE"):
        out.write(assets / name)
    for source in sorted((assets / "opencc").rglob("*")):
        if source.is_file() and not source.name.startswith("."):
            out.write(source)
PY
mv "$stage/Vitral-windows-x86_64.zip" target/release/
