#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
./scripts/fetch-assets.sh --verify
python3 - <<'PY'
import hashlib
import json
import shutil
from pathlib import Path

catalog = Path('scripts/models-catalog.json')
files = [f for m in json.loads(catalog.read_text())['models'] for f in m['files'] if (Path('assets') / f['rel_path']).is_file()]
# Verify every payload before replacing a previous export.
for item in files:
    path = Path('assets') / item['rel_path']
    if path.stat().st_size != item['size'] or hashlib.file_digest(path.open('rb'), 'sha256').hexdigest() != item['sha256']:
        raise SystemExit(f'Model verification failed: {path}')
out = Path('target/release/models')
if out.exists():
    shutil.rmtree(out)
out.mkdir(parents=True)
sums = []
for item in files:
    source = Path('assets') / item['rel_path']
    # Release assets are individual files, matching catalog download URLs.
    shutil.copy2(source, out / source.name)
    sums.append(f"{item['sha256']}  {source.name}")
shutil.copy2(catalog, out / catalog.name)
sums.append(f"{hashlib.sha256(catalog.read_bytes()).hexdigest()}  {catalog.name}")
(out / 'SHA256SUMS').write_text('\n'.join(sums) + '\n')
print(out)
PY
