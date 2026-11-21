# Vitral 本地模型中心与滚动迭代流水线

Vitral 将所有重型本地模型剥离出 Release 安装包，只内置约 30–42 MB 的核心词库与简繁转换表；用户可在设置页「模型中心」按本机 RAM 档位按需下载并热启用各项本地模型能力。

## 1. 按本机内存自动分档

`vitral-core` 启动时读取本机物理内存（Linux/Android 解析 `/proc/meminfo`，Windows 调用 `GlobalMemoryStatusEx`），自动划分三档：

| 内存档位 | 物理内存范围 | 推荐启用组合 |
| :--- | :--- | :--- |
| 轻量档 (`low`) | `< 6 GB` | 万象语法、离线手写、rbt3 重排、SenseVoice-Small、Qwen3-0.6B（空闲即卸载） |
| 标准档 (`standard`) | `6 – 12 GB` | 全部基础增强 + **Qwen3-ASR-0.6B**（热词偏置语音）+ **Qwen3-0.6B**（预测/画像/离线 AI） |
| 高性能档 (`high`) | `≥ 12 GB` | 全部能力 + **Qwen3-1.7B**（高精度预测与离线文本处理） |

## 2. 能力槽位与模型替换

所有模型由 [models-catalog.json](../scripts/models-catalog.json) 统一描述，支持从远端或自定义镜像地址动态更新，无需重新编译客户端即可跟随 Qwen（如 Qwen3 / Qwen3.5 / 后续版本）或更优竞品滚动迭代：

- `grammar`：万象中文语法模型（`.gram`）
- `handwriting`：离线手写识别模型 + 部件拆字索引
- `reranker`：连续手写双向重排模型（`rbt3`）
- `voice`：同能力互斥切换——轻量 `SenseVoice-Small` 或高精 `Qwen3-ASR-0.6B`（支持注入个人高频词热词偏置与拼音约束纠错 PY-GEC）
- `local_llm`：同能力互斥切换——`Qwen3-0.6B`（INT8 ONNX）或 `Qwen3-1.7B`（Q4F16 ONNX），原生通过 `ort` 跨 Linux / Windows / Android 运行，驱动：
  - Codex 风格本地预测输入（`live` / `catch_up` 双触发、非组词门控、LRU 缓存、立即取消、`Tab` 接受草稿）
  - 本地用户画像蒸馏（结构化压缩个人高频实体、领域、按应用风格，可查看/编辑/一键清空）
  - 纯离线选区翻译、口语润色、录音摘要与连续手写 Tier 3 本地消歧

## 3. 专属特调与发布流水线（阶段 4）

`scripts/finetune/` 提供完整的模型无关微调与量化发布脚手架：

```bash
# 1. 从脱敏后的个人画像/语料构建 SFT 数据集（自动剔除密钥/URL/卡号）
python3 scripts/finetune/prepare_dataset.py \
  --profile ~/.local/share/vitral/user-profile.json \
  --out target/finetune/sft.jsonl

# 2. 基于最新 Qwen3 / Qwen3.5 或竞品底模执行 QLoRA 微调（适配 >= 6GB 显存）
python3 scripts/finetune/train_qlora.py \
  --base-model Qwen/Qwen3-0.6B \
  --dataset target/finetune/sft.jsonl \
  --output-dir target/finetune/run1 \
  --use-4bit --merge

# 3. 导出 ONNX INT8 并自动生成带 SHA-256 的 models-catalog.json 条目
python3 scripts/finetune/export_and_eval.py export \
  --merged-dir target/finetune/run1/merged \
  --out-dir target/finetune/export \
  --model-id llm-qwen3-0.6b-vitral \
  --name "Qwen3-0.6B Vitral 特调版" \
  --base-url https://github.com/ZZZdragondYNGPHX/Vitral/releases/download/models-v2 \
  --int8

# 4. 评测预测命中率与 PY-GEC 纠错字符错误率
python3 scripts/finetune/export_and_eval.py eval --input target/finetune/eval_outputs.jsonl
```

发布独立模型包到 GitHub Releases 时运行 `./scripts/package-models.sh`，将 `target/release/models/` 下的文件与 `models-catalog.json` 上传至对应 Release 标签即可。
