#!/usr/bin/env python3
"""Export a fine-tuned model to ONNX INT8 / catalog entry and run Vitral eval checks."""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
from pathlib import Path


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        while chunk := fh.read(1024 * 1024):
            h.update(chunk)
    return h.hexdigest()


def edit_distance(a: str, b: str) -> int:
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        curr = [i]
        for j, cb in enumerate(b, 1):
            curr.append(min(prev[j] + 1, curr[j - 1] + 1, prev[j - 1] + (ca != cb)))
        prev = curr
    return prev[-1]


def evaluate_predictions(eval_jsonl: Path) -> dict:
    """Evaluate offline prediction & PY-GEC outputs stored as {reference, hypothesis, task}."""
    pred_hits = 0
    pred_total = 0
    gec_cer_num = 0
    gec_cer_den = 0
    for line in eval_jsonl.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        row = json.loads(line)
        ref = str(row.get("reference", "")).strip()
        hyp = str(row.get("hypothesis", "")).strip()
        if row.get("task") == "py_gec":
            gec_cer_num += edit_distance(ref, hyp)
            gec_cer_den += max(1, len(ref))
        else:
            pred_total += 1
            if hyp and (ref.startswith(hyp[:2]) or hyp.startswith(ref[:2])):
                pred_hits += 1
    return {
        "prediction_prefix_hit_rate": round(pred_hits / max(1, pred_total), 4),
        "prediction_samples": pred_total,
        "py_gec_cer": round(gec_cer_num / max(1, gec_cer_den), 4),
    }


def export_onnx(merged_dir: Path, out_dir: Path, quantize_int8: bool) -> list[Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        [
            "optimum-cli",
            "export",
            "onnx",
            "--model",
            str(merged_dir),
            "--task",
            "text-generation-with-past",
            str(out_dir),
        ],
        check=True,
    )
    produced = []
    model_fp = out_dir / "model.onnx"
    if quantize_int8 and model_fp.is_file():
        from onnxruntime.quantization import QuantType, quantize_dynamic

        model_int8 = out_dir / "model_int8.onnx"
        quantize_dynamic(str(model_fp), str(model_int8), weight_type=QuantType.QInt8)
        produced.append(model_int8)
    elif model_fp.is_file():
        produced.append(model_fp)
    for name in ("tokenizer.json", "config.json"):
        p = out_dir / name
        if p.is_file():
            produced.append(p)
    return produced


def build_catalog_entry(
    model_id: str,
    name: str,
    version: str,
    min_ram_mb: int,
    recommended_ram_mb: int,
    base_url: str,
    files: list[Path],
) -> dict:
    base = base_url.rstrip("/")
    return {
        "id": model_id,
        "capability": "local_llm",
        "name": name,
        "description": "Vitral 特调本地模型，支持语境预测输入、个人画像蒸馏与离线 AI 文本处理",
        "version": version,
        "engine": "onnx",
        "license": "Apache-2.0",
        "min_ram_mb": min_ram_mb,
        "recommended_ram_mb": recommended_ram_mb,
        "files": [
            {
                "rel_path": f"llm/{model_id}/{p.name}",
                "size": p.stat().st_size,
                "sha256": sha256_file(p),
                "urls": [f"{base}/{p.name}"],
            }
            for p in files
        ],
    }


def main() -> None:
    p = argparse.ArgumentParser(description="Export and evaluate Vitral local models")
    sub = p.add_subparsers(dest="cmd", required=True)

    exp = sub.add_parser("export", help="Export merged HF model to ONNX and emit catalog JSON")
    exp.add_argument("--merged-dir", type=Path, required=True)
    exp.add_argument("--out-dir", type=Path, required=True)
    exp.add_argument("--model-id", required=True)
    exp.add_argument("--name", required=True)
    exp.add_argument("--version", default="v1")
    exp.add_argument("--min-ram-mb", type=int, default=3072)
    exp.add_argument("--recommended-ram-mb", type=int, default=6144)
    exp.add_argument("--base-url", required=True)
    exp.add_argument("--int8", action="store_true")

    ev = sub.add_parser("eval", help="Evaluate prediction/PY-GEC JSONL outputs")
    ev.add_argument("--input", type=Path, required=True)

    args = p.parse_args()
    if args.cmd == "export":
        files = export_onnx(args.merged_dir, args.out_dir, args.int8)
        entry = build_catalog_entry(
            args.model_id,
            args.name,
            args.version,
            args.min_ram_mb,
            args.recommended_ram_mb,
            args.base_url,
            files,
        )
        out_json = args.out_dir / "catalog-entry.json"
        out_json.write_text(json.dumps(entry, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(f"Exported {len(files)} files and catalog entry -> {out_json}")
    else:
        print(json.dumps(evaluate_predictions(args.input), ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
