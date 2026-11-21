#!/usr/bin/env python3
"""Export assets/handwriting/hw_model.pt (DirectMapSENet EMA) to ONNX & INT8 ONNX.

Shims `cnn_chinese_hw` in `sys.modules` so the SHA-256-verified upstream files in
`assets/handwriting/scripts/{config,model}.py` are loaded untouched.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import math
import sys
import types
from pathlib import Path
from typing import Iterable

import numpy as np
import onnx
import onnxruntime as ort
import torch


REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_ASSETS_DIR = REPO_ROOT / "assets" / "handwriting"


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        while True:
            chunk = fh.read(1024 * 1024)
            if not chunk:
                break
            h.update(chunk)
    return h.hexdigest()


def load_upstream_hw_modules(hw_dir: Path):
    """Load config.py and model.py untouched by shimming cnn_chinese_hw in sys.modules."""
    scripts_dir = hw_dir / "scripts"
    config_path = scripts_dir / "config.py"
    model_path = scripts_dir / "model.py"
    if not config_path.is_file() or not model_path.is_file():
        raise FileNotFoundError(f"Missing upstream scripts in {scripts_dir}")

    pkg = types.ModuleType("cnn_chinese_hw")
    pkg.__path__ = [str(hw_dir.parent)]
    get_pkg_mod = types.ModuleType("cnn_chinese_hw.get_package_dir")
    get_pkg_mod.get_package_dir = lambda: str(hw_dir.parent)
    rec_pkg = types.ModuleType("cnn_chinese_hw.recognizer")
    rec_pkg.__path__ = [str(scripts_dir)]

    sys.modules["cnn_chinese_hw"] = pkg
    sys.modules["cnn_chinese_hw.get_package_dir"] = get_pkg_mod
    sys.modules["cnn_chinese_hw.recognizer"] = rec_pkg

    cfg_spec = importlib.util.spec_from_file_location(
        "cnn_chinese_hw.recognizer.config", config_path
    )
    assert cfg_spec and cfg_spec.loader
    cfg_mod = importlib.util.module_from_spec(cfg_spec)
    sys.modules["cnn_chinese_hw.recognizer.config"] = cfg_mod
    cfg_spec.loader.exec_module(cfg_mod)

    mdl_spec = importlib.util.spec_from_file_location(
        "cnn_chinese_hw.recognizer.model", model_path
    )
    assert mdl_spec and mdl_spec.loader
    mdl_mod = importlib.util.module_from_spec(mdl_spec)
    sys.modules["cnn_chinese_hw.recognizer.model"] = mdl_mod
    mdl_spec.loader.exec_module(mdl_mod)

    return cfg_mod, mdl_mod


# ---------------------------------------------------------------------------
# Exact DirectMapSENet rasterization (mirrors mcyph/cnn_chinese_hw preprocessing)
# ---------------------------------------------------------------------------
def _points_normalized(
    strokes: list[list[tuple[float, float]]], width: int, height: int
) -> list[list[tuple[int, int]]]:
    xs = [x for st in strokes for (x, _) in st]
    ys = [y for st in strokes for (_, y) in st]
    if not xs:
        return []
    min_x, max_x = min(xs), max(xs)
    min_y, max_y = min(ys), max(ys)
    dx = max_x - min_x
    dy = max_y - min_y
    sx = (width / dx) if dx > 1e-6 else 1.0
    sy = (height / dy) if dy > 1e-6 else 1.0
    out = []
    for st in strokes:
        pts = []
        for x, y in st:
            nx = max(0, min(width, round((x - min_x) * sx)))
            ny = max(0, min(height, round((y - min_y) * sy)))
            pts.append((nx, ny))
        out.append(pts)
    return out


def _get_vertex(stroke: list[tuple[int, int]], error_sq: float = 225.0) -> list[tuple[int, int]]:
    if len(stroke) <= 2:
        return list(stroke)

    def rdp(pts: list[tuple[int, int]]) -> list[tuple[int, int]]:
        if len(pts) <= 2:
            return list(pts)
        x0, y0 = pts[0]
        x1, y1 = pts[-1]
        vx, vy = x1 - x0, y1 - y0
        denom = vx * vx + vy * vy
        max_d2 = -1.0
        max_idx = 0
        for i in range(1, len(pts) - 1):
            px, py = pts[i]
            if denom == 0:
                d2 = float((px - x0) ** 2 + (py - y0) ** 2)
            else:
                cross = (px - x0) * vy - (py - y0) * vx
                d2 = float(cross * cross) / float(denom)
            if d2 > max_d2:
                max_d2 = d2
                max_idx = i
        if max_d2 > error_sq:
            left = rdp(pts[: max_idx + 1])
            right = rdp(pts[max_idx:])
            return left[:-1] + right
        return [pts[0], pts[-1]]

    if stroke[0] == stroke[-1] and len(stroke) >= 4:
        mid = len(stroke) // 2
        left = rdp(stroke[: mid + 1])
        right = rdp(stroke[mid:])
        return left[:-1] + right
    return rdp(stroke)


def _bresenham_line(x0: int, y0: int, x1: int, y1: int) -> list[tuple[int, int]]:
    points = []
    dx = abs(x1 - x0)
    dy = abs(y1 - y0)
    sx = 1 if x0 < x1 else -1
    sy = 1 if y0 < y1 else -1
    err = dx - dy
    cx, cy = x0, y0
    while True:
        points.append((cx, cy))
        if cx == x1 and cy == y1:
            break
        e2 = 2 * err
        if e2 > -dy:
            err -= dy
            cx += sx
        if e2 < dx:
            err += dx
            cy += sy
    return points


def render_directmap(strokes: list[list[tuple[float, float]]], size: int = 48) -> np.ndarray:
    norm1000 = _points_normalized(strokes, 1000, 1000)
    simplified = [_get_vertex(st) for st in norm1000 if st]
    norm47 = _points_normalized(
        [[(float(x), float(y)) for (x, y) in st] for st in simplified],
        size - 1,
        size - 1,
    )
    img = np.zeros((size, size, 3), dtype=np.uint8)
    m = len(norm47)
    for j, st in enumerate(norm47):
        strokes_pc = (j / (m - 1)) if m > 1 else 1.0
        c2 = int((1.0 - strokes_pc) * 255)
        if len(st) == 1:
            x, y = st[0]
            img[y, x, 0] = max(img[y, x, 0], 255)
            img[y, x, 1] = max(img[y, x, 1], 255)
            img[y, x, 2] = max(img[y, x, 2], c2)
            continue
        pix: list[tuple[int, int]] = []
        for idx in range(len(st) - 1):
            seg = _bresenham_line(st[idx][0], st[idx][1], st[idx + 1][0], st[idx + 1][1])
            if idx > 0 and pix and seg and seg[0] == pix[-1]:
                pix.extend(seg[1:])
            else:
                pix.extend(seg)
        l = len(pix)
        for i, (x, y) in enumerate(pix):
            stroke_pc = (i / (l - 1)) if l > 1 else 1.0
            c0 = int((0.2 + (1.0 - stroke_pc) * 0.8) * 255)
            c1 = int((0.2 + stroke_pc * 0.8) * 255)
            if c0 > img[y, x, 0]:
                img[y, x, 0] = c0
            if c1 > img[y, x, 1]:
                img[y, x, 1] = c1
            if c2 > img[y, x, 2]:
                img[y, x, 2] = c2
    chw = np.transpose(img, (2, 0, 1)).astype(np.float32) / 255.0
    return chw


def build_validation_samples() -> list[tuple[str, list[list[tuple[float, float]]]]]:
    """Synthetic geometric character trajectories for parity verification."""
    return [
        ("十", [[(0.15, 0.50), (0.85, 0.50)], [(0.50, 0.15), (0.50, 0.85)]]),
        (
            "口",
            [
                [(0.22, 0.25), (0.22, 0.78)],
                [(0.22, 0.25), (0.78, 0.25), (0.78, 0.78)],
                [(0.22, 0.78), (0.78, 0.78)],
            ],
        ),
        (
            "日",
            [
                [(0.24, 0.18), (0.24, 0.82)],
                [(0.24, 0.18), (0.76, 0.18), (0.76, 0.82)],
                [(0.24, 0.50), (0.76, 0.50)],
                [(0.24, 0.82), (0.76, 0.82)],
            ],
        ),
        (
            "由",
            [
                [(0.22, 0.35), (0.22, 0.84)],
                [(0.22, 0.35), (0.78, 0.35), (0.78, 0.84)],
                [(0.22, 0.59), (0.78, 0.59)],
                [(0.50, 0.12), (0.50, 0.84)],
                [(0.22, 0.84), (0.78, 0.84)],
            ],
        ),
        (
            "木",
            [
                [(0.16, 0.36), (0.84, 0.36)],
                [(0.50, 0.12), (0.50, 0.88)],
                [(0.50, 0.36), (0.18, 0.78)],
                [(0.50, 0.36), (0.84, 0.78)],
            ],
        ),
        (
            "火",
            [
                [(0.20, 0.32), (0.28, 0.48)],
                [(0.80, 0.30), (0.64, 0.46)],
                [(0.50, 0.14), (0.48, 0.52), (0.20, 0.86)],
                [(0.48, 0.52), (0.82, 0.86)],
            ],
        ),
        (
            "光",
            [
                [(0.50, 0.12), (0.50, 0.46)],
                [(0.24, 0.22), (0.34, 0.38)],
                [(0.76, 0.20), (0.64, 0.38)],
                [(0.14, 0.46), (0.86, 0.46)],
                [(0.40, 0.46), (0.34, 0.72), (0.16, 0.90)],
                [(0.58, 0.46), (0.58, 0.86), (0.86, 0.88), (0.88, 0.74)],
            ],
        ),
        (
            "人",
            [
                [(0.52, 0.16), (0.38, 0.56), (0.16, 0.86)],
                [(0.44, 0.44), (0.66, 0.68), (0.86, 0.86)],
            ],
        ),
    ]


def cosine_similarity(a: np.ndarray, b: np.ndarray) -> float:
    na = float(np.linalg.norm(a))
    nb = float(np.linalg.norm(b))
    if na == 0.0 or nb == 0.0:
        return 0.0
    return float(np.dot(a, b) / (na * nb))


def evaluate_parity(
    ref_logits: np.ndarray, test_logits: np.ndarray, label: str
) -> tuple[float, float, float]:
    cos_vals = [cosine_similarity(ref_logits[i], test_logits[i]) for i in range(len(ref_logits))]
    min_cos = min(cos_vals)
    top1_hits = sum(
        int(np.argmax(ref_logits[i]) == np.argmax(test_logits[i]))
        for i in range(len(ref_logits))
    )
    top8_overlaps = []
    for i in range(len(ref_logits)):
        r8 = set(np.argsort(ref_logits[i])[-8:].tolist())
        t8 = set(np.argsort(test_logits[i])[-8:].tolist())
        top8_overlaps.append(len(r8 & t8) / 8.0)
    top1_rate = top1_hits / len(ref_logits)
    min_top8 = min(top8_overlaps)
    print(
        f"[PARITY] {label:<20} min_cos={min_cos:.6f} top1_parity={top1_rate:.3f} min_top8_overlap={min_top8:.3f}"
    )
    return min_cos, top1_rate, min_top8


def load_pt_model(hw_dir: Path, use_ema: bool = True):
    cfg_mod, mdl_mod = load_upstream_hw_modules(hw_dir)
    ckpt_path = hw_dir / "hw_model.pt"
    ckpt = torch.load(ckpt_path, map_location="cpu", weights_only=False)
    data_cfg = cfg_mod.DataConfig(**ckpt["data_cfg"])
    model_cfg = cfg_mod.ModelConfig(**ckpt["model_cfg"])
    model = mdl_mod.build_model(model_cfg)
    state = (ckpt.get("ema_state") if use_ema else None) or ckpt["model_state"]
    model.load_state_dict(state)
    model.eval()
    return model, ckpt, data_cfg, model_cfg


def export_models(hw_dir: Path, use_ema: bool = True) -> None:
    ckpt_path = hw_dir / "hw_model.pt"
    onnx_path = hw_dir / "hw_model.onnx"
    int8_path = hw_dir / "hw_model.int8.onnx"
    classes_path = hw_dir / "hw_model.classes.json"
    stamp_path = hw_dir / ".hw_model_onnx.sha256"

    model, ckpt, data_cfg, _ = load_pt_model(hw_dir, use_ema=use_ema)
    temperature = float(ckpt.get("temperature", 1.0))

    classes = [int(o) for o in ckpt["classes"]]
    classes_path.write_text(json.dumps(classes, ensure_ascii=False, indent=0) + "\n", encoding="utf-8")

    dummy = torch.zeros(1, data_cfg.channels, data_cfg.image_size, data_cfg.image_size)
    tmp_onnx = hw_dir / ".hw_model.onnx.part"
    torch.onnx.export(
        model,
        (dummy,),
        str(tmp_onnx),
        input_names=["directmap"],
        output_names=["logits"],
        dynamic_axes={"directmap": {0: "batch"}, "logits": {0: "batch"}},
        opset_version=17,
        dynamo=False,
    )
    tmp_onnx.replace(onnx_path)
    onnx_model = onnx.load(str(onnx_path))
    onnx.checker.check_model(onnx_model)
    print(f"[EXPORT] FP32 ONNX -> {onnx_path} ({onnx_path.stat().st_size} bytes)")

    samples = build_validation_samples()
    batch = np.stack([render_directmap(strokes, data_cfg.image_size) for _, strokes in samples], axis=0)
    with torch.no_grad():
        pt_logits = model(torch.from_numpy(batch)).numpy()

    fp32_sess = ort.InferenceSession(str(onnx_path), providers=["CPUExecutionProvider"])
    fp32_logits = fp32_sess.run(["logits"], {"directmap": batch})[0]
    min_cos_fp32, top1_fp32, top8_fp32 = evaluate_parity(pt_logits, fp32_logits, "fp32_onnx")
    if min_cos_fp32 < 0.9999 or top1_fp32 < 1.0:
        raise SystemExit("FP32 ONNX parity check failed!")

    from onnxruntime.quantization import QuantType, quantize_dynamic

    candidates = [
        ("MatMul+Gemm", ["MatMul", "Gemm"], hw_dir / ".hw_model.matmul.int8.onnx"),
        ("Full", None, hw_dir / ".hw_model.full.int8.onnx"),
    ]

    chosen_mode = None
    chosen_tmp = None
    best_score = -1.0

    for mode_name, op_types, cand_path in candidates:
        kwargs = {"weight_type": QuantType.QInt8}
        if op_types is not None:
            kwargs["op_types_to_quantize"] = op_types
        quantize_dynamic(str(onnx_path), str(cand_path), **kwargs)
        try:
            sess = ort.InferenceSession(str(cand_path), providers=["CPUExecutionProvider"])
            q_logits = sess.run(["logits"], {"directmap": batch})[0]
            min_cos, top1, min_top8 = evaluate_parity(pt_logits, q_logits, f"int8_{mode_name}")
            if min_cos >= 0.995 and top1 == 1.0 and min_top8 >= 0.875:
                # Prefer smaller Full QInt8 only if parity is essentially identical, else MatMul+Gemm
                score = min_cos + (0.0001 if mode_name == "MatMul+Gemm" else 0.0)
                if score > best_score:
                    best_score = score
                    chosen_mode = mode_name
                    chosen_tmp = cand_path
        except Exception as exc:
            print(f"[WARN]   int8_{mode_name} failed ORT execution: {exc}")

    if chosen_tmp is None or chosen_mode is None:
        raise SystemExit("No INT8 quantization variant met cosine >= 0.995 & Top-1/Top-8 parity!")

    chosen_tmp.replace(int8_path)
    for _, _, cand_path in candidates:
        if cand_path.exists() and cand_path != int8_path:
            cand_path.unlink()

    print(f"[EXPORT] INT8 ({chosen_mode}) -> {int8_path} ({int8_path.stat().st_size} bytes)")

    stamp = {
        "ckpt_sha256": sha256_file(ckpt_path),
        "opset": 17,
        "temperature": temperature,
        "quantization_mode": chosen_mode,
        "files": {
            "hw_model.onnx": {
                "size": onnx_path.stat().st_size,
                "sha256": sha256_file(onnx_path),
            },
            "hw_model.int8.onnx": {
                "size": int8_path.stat().st_size,
                "sha256": sha256_file(int8_path),
            },
        },
    }
    tmp_stamp = stamp_path.with_name(stamp_path.name + ".part")
    tmp_stamp.write_text(json.dumps(stamp, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    tmp_stamp.replace(stamp_path)
    print(f"[STAMP]  Wrote {stamp_path}")


def verify_models(hw_dir: Path) -> None:
    ckpt_path = hw_dir / "hw_model.pt"
    onnx_path = hw_dir / "hw_model.onnx"
    int8_path = hw_dir / "hw_model.int8.onnx"
    stamp_path = hw_dir / ".hw_model_onnx.sha256"

    for p in (ckpt_path, onnx_path, int8_path, stamp_path):
        if not p.is_file() or p.stat().st_size == 0:
            raise SystemExit(f"[FAIL] Missing or empty required file: {p}")

    stamp = json.loads(stamp_path.read_text(encoding="utf-8"))
    actual_ckpt_sha = sha256_file(ckpt_path)
    if stamp.get("ckpt_sha256") != actual_ckpt_sha:
        raise SystemExit(
            f"[FAIL] ckpt SHA-256 mismatch: stamp={stamp.get('ckpt_sha256')} actual={actual_ckpt_sha}"
        )

    files_meta = stamp.get("files", {})
    for rel, p in (("hw_model.onnx", onnx_path), ("hw_model.int8.onnx", int8_path)):
        meta = files_meta.get(rel)
        if not isinstance(meta, dict):
            raise SystemExit(f"[FAIL] Stamp missing entry for {rel}")
        if p.stat().st_size != meta.get("size"):
            raise SystemExit(f"[FAIL] Size mismatch for {rel}")
        if sha256_file(p) != meta.get("sha256"):
            raise SystemExit(f"[FAIL] SHA-256 mismatch for {rel}")

    onnx.checker.check_model(onnx.load(str(onnx_path)))
    onnx.checker.check_model(onnx.load(str(int8_path)))

    model, _, data_cfg, _ = load_pt_model(hw_dir, use_ema=True)
    samples = build_validation_samples()
    batch = np.stack([render_directmap(strokes, data_cfg.image_size) for _, strokes in samples], axis=0)
    with torch.no_grad():
        pt_logits = model(torch.from_numpy(batch)).numpy()

    fp32_sess = ort.InferenceSession(str(onnx_path), providers=["CPUExecutionProvider"])
    fp32_logits = fp32_sess.run(["logits"], {"directmap": batch})[0]
    min_cos_fp32, top1_fp32, top8_fp32 = evaluate_parity(pt_logits, fp32_logits, "verify_fp32")

    int8_sess = ort.InferenceSession(str(int8_path), providers=["CPUExecutionProvider"])
    int8_logits = int8_sess.run(["logits"], {"directmap": batch})[0]
    min_cos_int8, top1_int8, top8_int8 = evaluate_parity(pt_logits, int8_logits, "verify_int8")

    if min_cos_fp32 < 0.999 or top1_fp32 < 1.0 or top8_fp32 < 1.0:
        raise SystemExit("[FAIL] FP32 ONNX verification parity threshold failed")
    if min_cos_int8 < 0.995 or top1_int8 < 1.0 or top8_int8 < 0.875:
        raise SystemExit("[FAIL] INT8 ONNX verification parity threshold failed")

    print("[OK]     hw_model.onnx & hw_model.int8.onnx verified cleanly against hw_model.pt EMA.")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--assets-dir",
        type=Path,
        default=DEFAULT_ASSETS_DIR,
        help="Path to assets/handwriting directory",
    )
    parser.add_argument(
        "--no-ema",
        action="store_true",
        help="Export raw model_state instead of ema_state",
    )
    parser.add_argument(
        "--verify",
        action="store_true",
        help="Verify existing ONNX exports, SHA-256 stamp, and PyTorch parity",
    )
    args = parser.parse_args()
    hw_dir = args.assets_dir.resolve()
    if args.verify:
        verify_models(hw_dir)
    else:
        export_models(hw_dir, use_ema=not args.no_ema)


if __name__ == "__main__":
    main()
