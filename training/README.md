# Train your own Tales conductor

This produces **your own** LLM conductor: a small open base model, fine-tuned on
Tales-generated coordination data, served locally — no cloud API, no external
agent CLI, no per-token cost. The base weights are open-source (you can't
indie-train a foundation model from scratch, and you don't need to — this is how
Fugu's Conductor was built too: a coordinator policy *on top of* an existing
model).

The keyword [`coordinator`](../crates/tales-core/src/coordinator.rs) stays as the
zero-cost default and the fallback when the LLM isn't loaded.

## The pipeline

```
tales coordinator export-dataset   →  conductor-dataset.jsonl   (labeled tasks)
python train_conductor.py --merge  →  conductor-lora-merged/    (your fp16 weights)
llama.cpp convert + quantize       →  conductor.Q4_K_M.gguf     (servable, ~hundreds of MB)
llama-server / ollama              →  http://localhost:8080     (your local conductor)
tales run --conductor llm          →  Tales asks YOUR model how to coordinate
```

## 1. Generate the dataset (no GPU)

```sh
tales coordinator export-dataset --out conductor-dataset.jsonl
```

676+ chat-format examples, labels ground-truth by construction. Expand the
vocab/templates in `crates/tales-core/src/dataset.rs` to scale it up — more
diversity = a sharper model.

## 2. Fine-tune (GPU: yours or rented)

```sh
python -m venv .venv && . .venv/bin/activate      # Windows: .venv\Scripts\activate
pip install -r requirements.txt
python train_conductor.py --data conductor-dataset.jsonl --merge
```

Defaults to `Qwen/Qwen2.5-0.5B-Instruct` — small on purpose: a 0.5–1.5B model is
plenty for a 3-way routing decision and serves fast. LoRA fits on a single
consumer GPU (or rent an hour of an A10/A100). Output: `conductor-lora-merged/`.

> No GPU at all? `--base Qwen/Qwen2.5-0.5B-Instruct` with 4-bit (`bitsandbytes`)
> trains on modest hardware; or rent compute for ~an hour. This step is the only
> one that needs a GPU — everything else is CPU/local.

## 3. Convert to GGUF + quantize (for llama.cpp)

```sh
git clone https://github.com/ggerganov/llama.cpp && cd llama.cpp
python convert_hf_to_gguf.py ../conductor-lora-merged --outfile conductor.f16.gguf
./llama-quantize conductor.f16.gguf conductor.Q4_K_M.gguf Q4_K_M
```

## 4. Serve locally (no external service)

```sh
./llama-server -m conductor.Q4_K_M.gguf --port 8080 --ctx-size 1024
# or:  ollama create tales-conductor -f Modelfile && ollama run tales-conductor
```

The server speaks an OpenAI-compatible `/v1/chat/completions` on localhost — it
never leaves your machine.

## 5. Point Tales at it

Build Tales with the conductor client enabled (it's an opt-in feature so the lean
default build pulls no HTTP stack), then route with `--conductor llm`:

```sh
cargo build --release --features llm-conductor

tales run "design the caching strategy for the API" \
  --conductor llm --conductor-url http://localhost:8080/v1
```

Tales' `LlmConductor` sends the task with the **same system prompt the model was
trained on** (`dataset::CONDUCTOR_SYSTEM`), parses the `{"shape","difficulty"}`
reply, and routes accordingly — falling back to the keyword coordinator if the
server is unreachable or the reply won't parse. Without the `llm-conductor`
feature, `--conductor llm` notes the missing build option and uses the keyword
coordinator, so a stock build still works.

## Why this beats the keyword model

The keyword coordinator counts keywords; it can't tell that "decide whether to
migrate all the cron jobs to the queue" is a *design decision* (debate), not bulk
mechanical work (tiered) — it sees "migrate all" and routes tiered at 99%
confidence. A model that reasons over the task instead of counting tokens
generalizes to these mixed-signal phrasings, while still running fully local and free.

`eval_llm_conductor.py` scores a served model exactly as `LlmConductor` calls it
(no torch, stdlib only):

```sh
# baseline: the keyword coordinator on the held-out corpus
python eval_llm_conductor.py --keyword                 # 18/18 = 100%
# a prompted local model (ollama) on the same corpus
python eval_llm_conductor.py --model qwen3.5:9b        # ties at 100% after prompt tuning
# the mixed-signal corpus where keyword-counting misfires
python eval_llm_conductor.py --keyword --corpus-file hard_corpus.json   # 7/11  = 63.6%
python eval_llm_conductor.py --model qwen3.5:9b --corpus-file hard_corpus.json  # 10/11 = 90.9%
```

Measured result (prompted `qwen3.5:9b`, `CONDUCTOR_SYSTEM` as shipped): it **ties**
the keyword model on the keyword-separable held-out set (100%) and **beats** it on
the Codex-validated mixed-signal `hard_corpus.json` (90.9% vs 63.6%) — every one
of the keyword model's misroutes there is a *decision* task it mistook for bulk
work. Fine-tuning (above) sharpens this further and removes the prompt-length cost
at inference.
