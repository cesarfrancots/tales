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

```sh
tales run "design the caching strategy for the API" \
  --conductor llm --conductor-url http://localhost:8080/v1
```

Tales' `LlmConductor` sends the task with the **same system prompt the model was
trained on** (`dataset::CONDUCTOR_SYSTEM`), parses the `{"shape","difficulty"}`
reply, and routes accordingly — falling back to the keyword coordinator if the
server is unreachable. (The `--conductor llm` wiring is the next build step; the
data + training half is what lives here.)

## Why this beats the keyword model

The keyword coordinator counts keywords; it can't tell that "refactor the sorting
algorithm" is conceptually a close call. The fine-tuned model learns from
*labels*, not keywords, so it generalizes to phrasings and mixed-signal tasks the
keyword model misreads — while still running fully local and free. Measure the
gap with `tales coordinator eval` (keyword baseline) and, once wired, the same
held-out corpus against the served model.
