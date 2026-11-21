#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

MANIFEST_PATH="$SCRIPT_DIR/assets-manifest.json"
CACHE_DIR="${VITRAL_ASSET_CACHE:-$HOME/.cache/vitral/assets}"
ASSETS_DIR="$REPO_ROOT/assets"
VERIFY_ONLY=0
DRY_RUN=0
GROUP_FILTER=""

usage() {
  cat <<EOF
Usage: $0 [OPTIONS]

Options:
  --manifest <path>    Path to assets-manifest.json (default: scripts/assets-manifest.json)
  --cache-dir <dir>    Cache directory for downloaded archives (default: \$VITRAL_ASSET_CACHE or ~/.cache/vitral/assets)
  --assets-dir <dir>   Staging directory for extracted/ready assets (default: <repo>/assets)
  --group <name>       Only process entries in group (rime|grammar|opencc|handwriting|voice|reranker)
  --verify             Pure offline local verification of cached archives and staged files
  --dry-run            Print planned downloads/extractions without modifying files
  -h, --help           Show this help message
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --manifest)
      MANIFEST_PATH="$2"
      shift 2
      ;;
    --cache-dir)
      CACHE_DIR="$2"
      shift 2
      ;;
    --assets-dir)
      ASSETS_DIR="$2"
      shift 2
      ;;
    --group)
      GROUP_FILTER="$2"
      shift 2
      ;;
    --verify)
      VERIFY_ONLY=1
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [ ! -f "$MANIFEST_PATH" ]; then
  echo "ERROR: Manifest not found: $MANIFEST_PATH" >&2
  exit 1
fi

export VITRAL_MANIFEST_PATH="$MANIFEST_PATH"
export VITRAL_CACHE_DIR="$CACHE_DIR"
export VITRAL_ASSETS_DIR="$ASSETS_DIR"
export VITRAL_VERIFY_ONLY="$VERIFY_ONLY"
export VITRAL_DRY_RUN="$DRY_RUN"
export VITRAL_GROUP_FILTER="$GROUP_FILTER"
export VITRAL_SCRIPT_DIR="$SCRIPT_DIR"

python3 - <<'PY'
import hashlib
import io
import json
import os
import pickle
import shutil
import subprocess
import sys
import tarfile
import tempfile
import zipfile
from pathlib import Path

manifest_path = Path(os.environ["VITRAL_MANIFEST_PATH"]).resolve()
cache_dir = Path(os.environ["VITRAL_CACHE_DIR"]).expanduser().resolve()
assets_dir = Path(os.environ["VITRAL_ASSETS_DIR"]).expanduser().resolve()
verify_only = os.environ["VITRAL_VERIFY_ONLY"] == "1"
dry_run = os.environ["VITRAL_DRY_RUN"] == "1"
group_filter = os.environ["VITRAL_GROUP_FILTER"].strip()

with manifest_path.open("r", encoding="utf-8") as f:
    manifest = json.load(f)

entries = manifest.get("assets", [])
if group_filter:
    entries = [e for e in entries if e.get("group") == group_filter]
    if not entries:
        print(f"ERROR: No assets matched group '{group_filter}'", file=sys.stderr)
        sys.exit(2)


def resolve_stage_dir(raw_stage_dir: str) -> Path:
    p = Path(raw_stage_dir)
    if p.parts and p.parts[0] == "assets":
        return assets_dir.joinpath(*p.parts[1:])
    return assets_dir / p


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        while True:
            chunk = fh.read(1024 * 1024)
            if not chunk:
                break
            h.update(chunk)
    return h.hexdigest()


def extract_hw_classes(pt_path: Path, out_json: Path) -> bool:
    """Deterministically extract classes list from hw_model.pt without requiring PyTorch."""
    class _DummyObj:
        def __init__(self, *args, **kwargs):
            pass
        def __setstate__(self, state):
            self.state = state

    class _SafeCkptUnpickler(pickle.Unpickler):
        def find_class(self, module, name):
            if module == "collections" and name == "OrderedDict":
                from collections import OrderedDict
                return OrderedDict
            if module.startswith("torch"):
                return lambda *args, **kwargs: _DummyObj(*args, **kwargs)
            return super().find_class(module, name)

        def persistent_load(self, saved_id):
            return None

    with zipfile.ZipFile(pt_path, "r") as zf:
        pkl_candidates = [n for n in zf.namelist() if n.endswith("data.pkl")]
        if not pkl_candidates:
            return False
        raw = zf.read(pkl_candidates[0])
    ckpt = _SafeCkptUnpickler(io.BytesIO(raw)).load()
    if not isinstance(ckpt, dict) or "classes" not in ckpt:
        return False
    classes = list(ckpt["classes"])
    tmp_out = out_json.with_suffix(".json.part")
    tmp_out.write_text(json.dumps(classes, ensure_ascii=False, indent=0) + "\n", encoding="utf-8")
    tmp_out.replace(out_json)
    return len(classes) > 0


EXPECTED_HW_CLASS_COUNT = 15146
REQUIRED_HW_CHARS = ("耄", "曝", "爆")


def verify_hw_classes_json(classes_json: Path) -> list[str]:
    if not classes_json.is_file() or classes_json.stat().st_size == 0:
        return [f"[MISS] hw_model.classes.json missing or empty: {classes_json}"]
    try:
        data = json.loads(classes_json.read_text(encoding="utf-8"))
    except Exception as exc:
        return [f"[FAIL] hw_model.classes.json invalid JSON ({classes_json}): {exc}"]
    if not isinstance(data, list) or len(data) != EXPECTED_HW_CLASS_COUNT:
        got = len(data) if isinstance(data, list) else type(data).__name__
        return [
            f"[FAIL] hw_model.classes.json must be a JSON array of {EXPECTED_HW_CLASS_COUNT} ints, got {got}"
        ]
    if not all(isinstance(x, int) and not isinstance(x, bool) for x in data):
        return [f"[FAIL] hw_model.classes.json contains non-integer elements: {classes_json}"]
    codepoints = set(data)
    missing_chars = [ch for ch in REQUIRED_HW_CHARS if ord(ch) not in codepoints]
    if missing_chars:
        return [
            f"[FAIL] hw_model.classes.json missing required characters {missing_chars}: {classes_json}"
        ]
    return []


def verify_hw_onnx_stamp(hw_dir: Path) -> list[str]:
    stamp_path = hw_dir / ".hw_model_onnx.sha256"
    pt_path = hw_dir / "hw_model.pt"
    onnx_fp32 = hw_dir / "hw_model.onnx"
    onnx_int8 = hw_dir / "hw_model.int8.onnx"
    if not stamp_path.is_file():
        if onnx_fp32.exists() or onnx_int8.exists():
            return [f"[MISS] ONNX stamp missing despite ONNX files present: {stamp_path}"]
        return [f"[MISS] .hw_model_onnx.sha256 missing: {stamp_path}"]
    try:
        stamp = json.loads(stamp_path.read_text(encoding="utf-8"))
    except Exception as exc:
        return [f"[FAIL] .hw_model_onnx.sha256 invalid JSON ({stamp_path}): {exc}"]
    if not isinstance(stamp, dict):
        return [f"[FAIL] .hw_model_onnx.sha256 must be a JSON object: {stamp_path}"]
    if pt_path.is_file():
        actual_ckpt_sha = sha256_file(pt_path)
        if str(stamp.get("ckpt_sha256", "")).lower() != actual_ckpt_sha:
            return [f"[FAIL] .hw_model_onnx.sha256 ckpt_sha256 mismatch against hw_model.pt"]
    files_map = stamp.get("files")
    if not isinstance(files_map, dict):
        return [f"[FAIL] .hw_model_onnx.sha256 missing 'files' map: {stamp_path}"]
    errs: list[str] = []
    for rel in ("hw_model.onnx", "hw_model.int8.onnx"):
        target = hw_dir / rel
        if not target.is_file() or target.stat().st_size == 0:
            errs.append(f"[MISS] ONNX file missing or empty: {target}")
            continue
        meta = files_map.get(rel)
        if not isinstance(meta, dict):
            errs.append(f"[FAIL] .hw_model_onnx.sha256 missing entry for '{rel}'")
            continue
        if target.stat().st_size != meta.get("size"):
            errs.append(f"[FAIL] ONNX size mismatch for {target}: expected {meta.get('size')}, got {target.stat().st_size}")
            continue
        actual_sha = sha256_file(target)
        if actual_sha != str(meta.get("sha256", "")).lower():
            errs.append(f"[FAIL] ONNX SHA-256 mismatch for {target}: expected {meta.get('sha256')}, got {actual_sha}")
    return errs


def build_stage_stamp(entry: dict, stage_dir: Path, expected_sha: str, expected_size: int) -> dict:
    files_meta = {}
    for rel in entry.get("verify_files", []):
        target = stage_dir / rel
        files_meta[rel] = {
            "size": target.stat().st_size,
            "sha256": sha256_file(target),
        }
    return {
        "id": entry["id"],
        "archive_type": entry["archive_type"],
        "sha256": expected_sha,
        "size": expected_size,
        "files": files_meta,
    }


def write_stage_stamp(stamp_file: Path, stamp_data: dict) -> None:
    tmp_stamp = stamp_file.with_name(stamp_file.name + ".part")
    tmp_stamp.write_text(json.dumps(stamp_data, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    tmp_stamp.replace(stamp_file)


def check_staged_entry(entry: dict, stage_dir: Path, stamp_file: Path) -> list[str]:
    """Verify staged files and metadata stamp for an entry. Returns list of error lines."""
    errors: list[str] = []
    archive_type = entry["archive_type"]
    expected_sha = entry["sha256"].lower()
    expected_size = int(entry["size"])
    verify_files = entry.get("verify_files", [])

    if archive_type == "raw":
        for rel in verify_files:
            target = stage_dir / rel
            if not target.is_file() or target.stat().st_size == 0:
                errors.append(f"[MISS] staged file missing or empty: {target}")
                continue
            actual_size = target.stat().st_size
            if actual_size != expected_size:
                errors.append(
                    f"[FAIL] staged size mismatch for {target}: expected {expected_size}, got {actual_size}"
                )
                continue
            actual_sha = sha256_file(target)
            if actual_sha != expected_sha:
                errors.append(
                    f"[FAIL] staged SHA-256 mismatch for {target}: expected {expected_sha}, got {actual_sha}"
                )
        return errors

    # zip / tar.bz2: require valid JSON stamp matching archive sha256 and every verify_files entry
    if not stamp_file.is_file():
        errors.append(f"[MISS] stage stamp missing: {stamp_file}")
        return errors
    try:
        stamp = json.loads(stamp_file.read_text(encoding="utf-8"))
    except Exception as exc:
        errors.append(f"[FAIL] stage stamp invalid JSON ({stamp_file}): {exc}")
        return errors
    if not isinstance(stamp, dict) or str(stamp.get("sha256", "")).lower() != expected_sha:
        errors.append(f"[FAIL] stage stamp archive SHA-256 mismatch: {stamp_file}")
        return errors
    files_map = stamp.get("files")
    if not isinstance(files_map, dict):
        errors.append(f"[FAIL] stage stamp missing 'files' map: {stamp_file}")
        return errors

    for rel in verify_files:
        target = stage_dir / rel
        if not target.is_file() or target.stat().st_size == 0:
            errors.append(f"[MISS] staged file missing or empty: {target}")
            continue
        file_meta = files_map.get(rel)
        if not isinstance(file_meta, dict):
            errors.append(f"[FAIL] stage stamp missing entry for '{rel}': {stamp_file}")
            continue
        exp_fsize = file_meta.get("size")
        exp_fsha = str(file_meta.get("sha256", "")).lower()
        act_fsize = target.stat().st_size
        if not isinstance(exp_fsize, int) or act_fsize != exp_fsize:
            errors.append(
                f"[FAIL] staged size mismatch for {target}: expected {exp_fsize}, got {act_fsize}"
            )
            continue
        act_fsha = sha256_file(target)
        if act_fsha != exp_fsha:
            errors.append(
                f"[FAIL] staged SHA-256 mismatch for {target}: expected {exp_fsha}, got {act_fsha}"
            )
    return errors


if dry_run:
    print(f"[DRY-RUN] cache_dir={cache_dir}")
    print(f"[DRY-RUN] assets_dir={assets_dir}")
    for e in entries:
        sd = resolve_stage_dir(e["stage_dir"])
        print(f"[DRY-RUN] {e['id']} ({e['group']}, {e['size']} B) -> {sd}")
    sys.exit(0)

if not verify_only:
    cache_dir.mkdir(parents=True, exist_ok=True)
    assets_dir.mkdir(parents=True, exist_ok=True)

failures = 0
for entry in entries:
    aid = entry["id"]
    expected_sha = entry["sha256"].lower()
    expected_size = int(entry["size"])
    cached_path = cache_dir / entry["filename"]
    stage_dir = resolve_stage_dir(entry["stage_dir"])
    stamp_file = stage_dir / f".{aid}.sha256"

    if verify_only:
        entry_errors = 0
        if cached_path.is_file():
            actual_size = cached_path.stat().st_size
            if actual_size != expected_size:
                print(
                    f"[FAIL] size mismatch for {cached_path}: expected {expected_size}, got {actual_size}",
                    file=sys.stderr,
                )
                entry_errors += 1
            else:
                actual_sha = sha256_file(cached_path)
                if actual_sha != expected_sha:
                    print(
                        f"[FAIL] SHA-256 mismatch for {cached_path}: expected {expected_sha}, got {actual_sha}",
                        file=sys.stderr,
                    )
                    entry_errors += 1

        staged_errs = check_staged_entry(entry, stage_dir, stamp_file)
        for err_msg in staged_errs:
            print(err_msg, file=sys.stderr)
            entry_errors += 1

        if entry_errors > 0:
            failures += entry_errors
        else:
            print(f"[OK]   {aid:<28} sha256={expected_sha[:12]}... staged={stage_dir}")
        continue

    # Download / verify cache
    need_download = True
    if cached_path.is_file():
        actual_sha = sha256_file(cached_path)
        if actual_sha == expected_sha and cached_path.stat().st_size == expected_size:
            print(f"[CACHE] {aid}: verified {cached_path.name}")
            need_download = False
        else:
            print(f"[WARN]  {aid}: corrupt cache {cached_path.name} (sha={actual_sha}), deleting", file=sys.stderr)
            cached_path.unlink()

    if need_download:
        part_path = cached_path.with_name(cached_path.name + ".part")
        print(f"[FETCH] {aid}: {entry['url']} -> {cached_path.name}")
        cmd = [
            "curl",
            "-fSL",
            "--retry",
            "3",
            "--retry-delay",
            "2",
            "-o",
            str(part_path),
            entry["url"],
        ]
        res = subprocess.run(cmd)
        if res.returncode != 0:
            if part_path.exists():
                part_path.unlink()
            print(f"[FAIL]  {aid}: download failed with exit {res.returncode}", file=sys.stderr)
            sys.exit(1)
        actual_size = part_path.stat().st_size
        actual_sha = sha256_file(part_path)
        if actual_sha != expected_sha or actual_size != expected_size:
            part_path.unlink(missing_ok=True)
            print(
                f"[FAIL]  {aid}: verification failed! "
                f"expected sha={expected_sha} size={expected_size}, "
                f"got sha={actual_sha} size={actual_size}",
                file=sys.stderr,
            )
            sys.exit(1)
        part_path.replace(cached_path)
        print(f"[OK]    {aid}: downloaded and verified ({actual_size} bytes)")

    # Stage into assets_dir
    stage_dir.mkdir(parents=True, exist_ok=True)
    staged_errs = check_staged_entry(entry, stage_dir, stamp_file)
    stamp_is_json = False
    if stamp_file.is_file():
        try:
            parsed = json.loads(stamp_file.read_text(encoding="utf-8"))
            stamp_is_json = isinstance(parsed, dict) and str(parsed.get("sha256", "")).lower() == expected_sha
        except Exception:
            stamp_is_json = False
    all_present = (len(staged_errs) == 0) and stamp_is_json
    if not all_present:
        archive_type = entry["archive_type"]
        print(f"[STAGE] {aid} ({archive_type}) -> {stage_dir}")
        if archive_type == "raw":
            dst_name = entry.get("stage_filename") or cached_path.name
            dst_path = stage_dir / dst_name
            tmp_dst = dst_path.with_name(dst_path.name + ".part")
            shutil.copy2(cached_path, tmp_dst)
            tmp_dst.replace(dst_path)
        elif archive_type == "zip":
            subpath = entry.get("extract_subpath", "").strip("/")
            with tempfile.TemporaryDirectory() as tmpdir:
                tmp_root = Path(tmpdir)
                with zipfile.ZipFile(cached_path, "r") as zf:
                    zf.extractall(tmp_root)
                src_root = tmp_root / subpath if subpath else tmp_root
                if not src_root.is_dir():
                    print(f"[FAIL]  {aid}: extract_subpath '{subpath}' missing in zip", file=sys.stderr)
                    sys.exit(1)
                shutil.copytree(src_root, stage_dir, dirs_exist_ok=True)
        elif archive_type == "tar.bz2":
            strip_n = int(entry.get("strip_components", 0))
            with tarfile.open(cached_path, "r:bz2") as tf:
                for member in tf.getmembers():
                    parts = Path(member.name).parts
                    if len(parts) <= strip_n:
                        continue
                    rel_parts = parts[strip_n:]
                    rel_path = Path(*rel_parts)
                    if rel_path.is_absolute() or ".." in rel_path.parts:
                        continue
                    out_path = stage_dir / rel_path
                    if member.isdir():
                        out_path.mkdir(parents=True, exist_ok=True)
                    elif member.isfile():
                        out_path.parent.mkdir(parents=True, exist_ok=True)
                        with tf.extractfile(member) as src_f, out_path.open("wb") as dst_f:
                            if src_f is not None:
                                shutil.copyfileobj(src_f, dst_f)
        else:
            print(f"[FAIL]  {aid}: unknown archive_type '{archive_type}'", file=sys.stderr)
            sys.exit(1)

        for rel in entry.get("verify_files", []):
            target = stage_dir / rel
            if not target.is_file() or target.stat().st_size == 0:
                print(f"[FAIL]  {aid}: expected staged file missing: {target}", file=sys.stderr)
                sys.exit(1)
        if entry["archive_type"] == "raw":
            for rel in entry.get("verify_files", []):
                target = stage_dir / rel
                if target.stat().st_size != expected_size or sha256_file(target) != expected_sha:
                    print(f"[FAIL]  {aid}: staged raw file hash/size mismatch after copy: {target}", file=sys.stderr)
                    sys.exit(1)
        write_stage_stamp(stamp_file, build_stage_stamp(entry, stage_dir, expected_sha, expected_size))

    # Deterministic label extraction & ONNX export for handwriting checkpoint
    if aid == "cnn-chinese-hw-ckpt":
        hw_pt = stage_dir / "hw_model.pt"
        classes_json = stage_dir / "hw_model.classes.json"
        if hw_pt.is_file() and len(verify_hw_classes_json(classes_json)) > 0:
            if extract_hw_classes(hw_pt, classes_json):
                print(f"[META]  {aid}: extracted label table -> {classes_json}")
        export_py = Path(os.environ.get("VITRAL_SCRIPT_DIR", "")) / "export_hw_onnx.py"
        export_venv_py = Path(os.environ.get("VITRAL_PY_EXPORT", str(Path.home() / ".cache/vitral/py-export-venv/bin/python")))
        if hw_pt.is_file() and len(verify_hw_onnx_stamp(stage_dir)) > 0:
            if export_py.is_file() and export_venv_py.is_file():
                print(f"[ONNX]  {aid}: running {export_py.name} -> {stage_dir}")
                res = subprocess.run([str(export_venv_py), str(export_py), "--assets-dir", str(stage_dir)])
                if res.returncode != 0:
                    print(f"[FAIL]  {aid}: ONNX export failed with exit {res.returncode}", file=sys.stderr)
                    sys.exit(1)

if verify_only:
    hw_ckpt_entries = [e for e in entries if e.get("id") == "cnn-chinese-hw-ckpt"]
    if hw_ckpt_entries:
        hw_dir = resolve_stage_dir(hw_ckpt_entries[0]["stage_dir"])
        hw_classes = hw_dir / "hw_model.classes.json"
        hw_errs = verify_hw_classes_json(hw_classes)
        if hw_errs:
            for err_msg in hw_errs:
                print(err_msg, file=sys.stderr)
                failures += 1
        else:
            print(f"[OK]   hw-classes                   {hw_classes}")
        onnx_errs = verify_hw_onnx_stamp(hw_dir)
        if onnx_errs:
            for err_msg in onnx_errs:
                print(err_msg, file=sys.stderr)
                failures += 1
        else:
            print(f"[OK]   hw-onnx                      {hw_dir / '.hw_model_onnx.sha256'}")
    if failures:
        print(f"Verification failed with {failures} error(s).", file=sys.stderr)
        sys.exit(1)
    print(f"[OK] All {len(entries)} asset entries verified cleanly.")
else:
    print(f"[OK] All {len(entries)} asset entries fetched, SHA-256 verified, and staged.")
PY
