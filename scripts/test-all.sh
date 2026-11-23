#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
./scripts/locked-run.sh cargo test --workspace --locked
./scripts/fetch-assets.sh --verify
python_bin="${VITRAL_TEST_PYTHON:-.venv/bin/python}"
exec systemd-run --user --wait --pipe --collect --working-directory="$root/tests/e2e" \
  -p MemoryMax=4G -p MemorySwapMax=512M "$root/scripts/locked-run.sh" \
  "$python_bin" -m pytest -n 0 "$@"
