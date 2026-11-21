#!/usr/bin/env python3
"""Prepare sanitized SFT datasets for Vitral local Qwen fine-tuning.

Generates ChatML JSONL records for four input-empowerment tasks:
1. Context-aware next-phrase / next-sentence prediction (Codex-style)
2. PY-GEC phonetic-constrained Chinese ASR error correction
3. Spoken transcript polishing into concise written text
4. Structured user-profile distillation from anonymized habit signals
"""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Iterable

SECRET_RE = re.compile(
    r"(?i)(?:sk-[a-z0-9_-]{12,}|gh[pousr]_[a-z0-9]{16,}|bearer\s+\S+|https?://\S+|\b\d{12,19}\b)"
)

SEED_PREDICTIONS = [
    ("微信", "技术背景，偏好简短直接", "刚到公司，先把昨晚的日志看一下", "，确认没问题我就在群里同步。"),
    ("终端", "Rust/Linux 开发者，常用 cargo 和 git", "刚把这一版改完，先跑一遍", " cargo test 和 clippy 确认全绿再推。"),
    ("邮件", "正式工作沟通，结论先行", "附件已更新为最新方案，烦请查阅", "，如有调整建议请随时告知。"),
    ("笔记", "产品与架构思考，条理清晰", "这一阶段先把核心底座收口", "，后续再按内存档位逐步放开新模型。"),
]

SEED_PY_GEC = [
    ("帮我把代码推到远端仓库", "帮我把代码推到远端仓苦", ["远端仓库", "Vitral"]),
    ("今天下午三点同步一下进度", "今天下午三点同布一下进度", ["同步"]),
    ("在输入法设置里打开模型中心", "在输入法设置里打开魔形中心", ["模型中心", "Vitral"]),
]

SEED_POLISH = [
    ("呃就是说咱们这个模型包太大了然后能不能拆出来让用户自己下", "建议把本地模型从安装包中拆分出来，让用户在设置里按需下载。"),
    ("那个我等会儿把最新代码推上去顺便把本地软件目录也更新一下", "稍后我会把最新代码推送到远端，并同步更新本地安装目录。"),
]


def sanitize_text(text: str) -> str:
    cleaned = SECRET_RE.sub("", text).strip()
    return re.sub(r"\s+", " ", cleaned)


def chatml_record(system: str, user: str, assistant: str, task: str) -> dict:
    return {
        "task": task,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
            {"role": "assistant", "content": f"<think>\n\n</think>\n{assistant}"},
        ],
    }


def load_local_habits(profile_path: Path | None) -> Iterable[dict]:
    if not profile_path or not profile_path.is_file():
        return []
    data = json.loads(profile_path.read_text(encoding="utf-8"))
    summary = sanitize_text(str(data.get("summary", "")))
    records = []
    for app_id, style in (data.get("app_styles") or {}).items():
        for phrase in (style.get("common_phrases") or [])[:12]:
            clean = sanitize_text(str(phrase))
            if len(clean) < 4:
                continue
            cut = max(2, len(clean) // 2)
            records.append(
                chatml_record(
                    "你是 Vitral 本地输入预测引擎。结合用户画像与当前应用，仅输出紧接前文的自然续写，不要解释。",
                    f"应用: {app_id}\n画像: {summary}\n前文: {clean[:cut]}",
                    clean[cut:],
                    "predict_from_profile",
                )
            )
    return records


def build_dataset(profile_path: Path | None, extra_jsonl: Path | None) -> list[dict]:
    rows: list[dict] = []
    for app, profile, prefix, cont in SEED_PREDICTIONS:
        rows.append(
            chatml_record(
                "你是 Vitral 本地输入预测引擎。结合用户画像与当前应用，仅输出紧接前文的自然续写，不要解释。",
                f"应用: {app}\n画像: {profile}\n前文: {prefix}",
                cont,
                "predict",
            )
        )
    for target, noisy, hotwords in SEED_PY_GEC:
        rows.append(
            chatml_record(
                "你是中文语音同音纠错器。仅在读音相近且符合热词/语境时修正错别字，禁止改写原意或增删内容。",
                f"热词: {', '.join(hotwords)}\n原始转写: {noisy}",
                target,
                "py_gec",
            )
        )
    for raw, polished in SEED_POLISH:
        rows.append(
            chatml_record(
                "将口语化转写整理为简洁自然的书面表达，保留原意，仅返回整理后的文本。",
                raw,
                polished,
                "polish",
            )
        )
    rows.extend(load_local_habits(profile_path))
    if extra_jsonl and extra_jsonl.is_file():
        for line in extra_jsonl.read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            item = json.loads(line)
            for msg in item.get("messages", []):
                msg["content"] = sanitize_text(str(msg.get("content", "")))
            rows.append(item)
    return rows


def main() -> None:
    parser = argparse.ArgumentParser(description="Build Vitral SFT dataset JSONL")
    parser.add_argument("--profile", type=Path, help="Optional local user-profile.json")
    parser.add_argument("--extra", type=Path, help="Optional extra JSONL dataset")
    parser.add_argument("--out", type=Path, required=True, help="Output JSONL path")
    args = parser.parse_args()

    rows = build_dataset(args.profile, args.extra)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with args.out.open("w", encoding="utf-8") as fh:
        for row in rows:
            fh.write(json.dumps(row, ensure_ascii=False) + "\n")
    print(f"Wrote {len(rows)} sanitized SFT records -> {args.out}")


if __name__ == "__main__":
    main()
