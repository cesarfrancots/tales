#!/usr/bin/env python3
"""LoRA fine-tune a small open base model into Tales' own conductor.

Input: the chat-JSONL produced by `tales coordinator export-dataset`
(each line: {"messages": [system, user(task), assistant(decision)]}).

Output: a LoRA adapter (and, with --merge, merged fp16 weights ready for GGUF
conversion). The result is *your* model — it runs locally, no external API.

Run on a GPU box:
    pip install -r requirements.txt
    python train_conductor.py --data ../conductor-dataset.jsonl --merge

See README.md for the full export -> train -> GGUF -> serve recipe.
"""
import argparse

from datasets import load_dataset
from peft import LoraConfig
from transformers import AutoModelForCausalLM, AutoTokenizer
from trl import SFTConfig, SFTTrainer


def main() -> None:
    ap = argparse.ArgumentParser(description="LoRA fine-tune the Tales conductor.")
    ap.add_argument("--data", default="../conductor-dataset.jsonl",
                    help="chat-JSONL from `tales coordinator export-dataset`")
    ap.add_argument("--base", default="Qwen/Qwen2.5-0.5B-Instruct",
                    help="open-weights base model to specialize (small = fast to serve)")
    ap.add_argument("--out", default="conductor-lora", help="adapter output dir")
    ap.add_argument("--epochs", type=float, default=3.0)
    ap.add_argument("--batch", type=int, default=8)
    ap.add_argument("--lr", type=float, default=2e-4)
    ap.add_argument("--merge", action="store_true",
                    help="also write merged fp16 weights (<out>-merged) for GGUF export")
    ap.add_argument("--max-seq", type=int, default=512,
                    help="max sequence length (bump for the longer orchestration-plan target)")
    args = ap.parse_args()

    tok = AutoTokenizer.from_pretrained(args.base)
    if tok.pad_token is None:
        tok.pad_token = tok.eos_token

    # Render each chat example to a single training string via the base model's
    # own chat template — what it expects at inference, so training matches serving.
    ds = load_dataset("json", data_files=args.data, split="train")

    def to_text(example):
        return {"text": tok.apply_chat_template(example["messages"], tokenize=False)}

    ds = ds.map(to_text, remove_columns=ds.column_names)
    print(f"training examples: {len(ds)}")

    model = AutoModelForCausalLM.from_pretrained(
        args.base, torch_dtype="auto", device_map="auto"
    )

    lora = LoraConfig(
        r=16,
        lora_alpha=32,
        lora_dropout=0.05,
        bias="none",
        target_modules="all-linear",
        task_type="CAUSAL_LM",
    )

    cfg = SFTConfig(
        output_dir=args.out,
        num_train_epochs=args.epochs,
        per_device_train_batch_size=args.batch,
        gradient_accumulation_steps=2,
        learning_rate=args.lr,
        warmup_ratio=0.03,
        logging_steps=10,
        save_strategy="no",
        max_seq_length=args.max_seq,
        packing=False,
        dataset_text_field="text",
        bf16=True,
        report_to="none",
    )

    trainer = SFTTrainer(
        model=model,
        args=cfg,
        train_dataset=ds,
        peft_config=lora,
        tokenizer=tok,
    )
    trainer.train()
    trainer.save_model(args.out)
    tok.save_pretrained(args.out)
    print(f"adapter saved: {args.out}")

    if args.merge:
        merged_dir = f"{args.out}-merged"
        merged = trainer.model.merge_and_unload()
        merged.save_pretrained(merged_dir)
        tok.save_pretrained(merged_dir)
        print(f"merged fp16 weights saved: {merged_dir} (convert to GGUF next — see README)")


if __name__ == "__main__":
    main()
