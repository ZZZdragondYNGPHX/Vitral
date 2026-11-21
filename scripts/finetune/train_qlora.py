#!/usr/bin/env python3
"""QLoRA / LoRA fine-tuning entry point for Vitral Qwen small models.

Designed to stay model-agnostic across Qwen3 (0.6B/1.7B), Qwen3.5 (0.8B/2B),
and future open-weight CausalLM backbones while fitting consumer GPUs (>=6 GB VRAM).
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Fine-tune a small Qwen model with QLoRA for Vitral")
    p.add_argument("--base-model", default="Qwen/Qwen3-0.6B", help="HF model id or local path")
    p.add_argument("--dataset", type=Path, required=True, help="JSONL from prepare_dataset.py")
    p.add_argument("--output-dir", type=Path, required=True, help="Directory for adapter and merged weights")
    p.add_argument("--epochs", type=int, default=3)
    p.add_argument("--lr", type=float, default=2e-4)
    p.add_argument("--max-len", type=int, default=512)
    p.add_argument("--lora-r", type=int, default=16)
    p.add_argument("--lora-alpha", type=int, default=32)
    p.add_argument("--use-4bit", action="store_true", help="Enable 4-bit NF4 QLoRA via bitsandbytes")
    p.add_argument("--merge", action="store_true", help="Also save merged full-precision weights for ONNX/GGUF export")
    return p.parse_args()


def main() -> None:
    args = parse_args()
    import torch
    from datasets import Dataset
    from peft import LoraConfig, get_peft_model, prepare_model_for_kbit_training
    from transformers import (
        AutoModelForCausalLM,
        AutoTokenizer,
        BitsAndBytesConfig,
        DataCollatorForLanguageModeling,
        Trainer,
        TrainingArguments,
    )

    records = [
        json.loads(line)
        for line in args.dataset.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    if not records:
        raise SystemExit("Dataset is empty")

    tokenizer = AutoTokenizer.from_pretrained(args.base_model, trust_remote_code=True)
    if tokenizer.pad_token is None:
        tokenizer.pad_token = tokenizer.eos_token

    def encode(row: dict) -> dict:
        text = tokenizer.apply_chat_template(
            row["messages"], tokenize=False, add_generation_prompt=False
        )
        toks = tokenizer(text, truncation=True, max_length=args.max_len)
        toks["labels"] = list(toks["input_ids"])
        return toks

    ds = Dataset.from_list([encode(r) for r in records])

    quant_cfg = None
    if args.use_4bit:
        quant_cfg = BitsAndBytesConfig(
            load_in_4bit=True,
            bnb_4bit_quant_type="nf4",
            bnb_4bit_compute_dtype=torch.bfloat16 if torch.cuda.is_bf16_supported() else torch.float16,
            bnb_4bit_use_double_quant=True,
        )

    model = AutoModelForCausalLM.from_pretrained(
        args.base_model,
        quantization_config=quant_cfg,
        torch_dtype=torch.bfloat16 if torch.cuda.is_bf16_supported() else torch.float16,
        device_map="auto",
        trust_remote_code=True,
    )
    if args.use_4bit:
        model = prepare_model_for_kbit_training(model)
    model.config.use_cache = False

    lora_cfg = LoraConfig(
        r=args.lora_r,
        lora_alpha=args.lora_alpha,
        lora_dropout=0.05,
        bias="none",
        task_type="CAUSAL_LM",
        target_modules=["q_proj", "k_proj", "v_proj", "o_proj", "gate_proj", "up_proj", "down_proj"],
    )
    model = get_peft_model(model, lora_cfg)
    model.print_trainable_parameters()

    adapter_dir = args.output_dir / "lora_adapter"
    train_args = TrainingArguments(
        output_dir=str(args.output_dir / "checkpoints"),
        num_train_epochs=args.epochs,
        per_device_train_batch_size=1,
        gradient_accumulation_steps=8,
        learning_rate=args.lr,
        bf16=torch.cuda.is_available() and torch.cuda.is_bf16_supported(),
        fp16=torch.cuda.is_available() and not torch.cuda.is_bf16_supported(),
        gradient_checkpointing=True,
        logging_steps=5,
        save_strategy="no",
        report_to=[],
    )
    trainer = Trainer(
        model=model,
        args=train_args,
        train_dataset=ds,
        data_collator=DataCollatorForLanguageModeling(tokenizer=tokenizer, mlm=False),
    )
    trainer.train()

    adapter_dir.mkdir(parents=True, exist_ok=True)
    model.save_pretrained(adapter_dir)
    tokenizer.save_pretrained(adapter_dir)
    print(f"Saved LoRA adapter -> {adapter_dir}")

    if args.merge:
        merged_dir = args.output_dir / "merged"
        merged = model.merge_and_unload()
        merged.save_pretrained(merged_dir, safe_serialization=True)
        tokenizer.save_pretrained(merged_dir)
        print(f"Saved merged model -> {merged_dir}")


if __name__ == "__main__":
    main()
