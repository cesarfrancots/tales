# CONDUCTOR_PLAN_SYSTEM candidate — rationale

Candidate: `training/CONDUCTOR_PLAN_SYSTEM.candidate.txt` (1878 chars vs 1153 original).

The schema, field names, enums, and output shape are **unchanged** — this only retightens
instructional framing so the model's output still matches the deterministic
`dataset::orchestration_plan` training target.

## Changes mapped to the three observed failure modes

### 1. Invented enum tokens (`"shape":"codemod"`, bad role/agent/model)
Seen in the eval failures as clustered `roles_valid` + `agents_valid` + `models_valid`
failures on `debate` tasks (`plan-orch3-easy/hard.json`).

- Moved a **closed-set enumeration to the top**, before the schema, listing every legal
  token for `shape`, `role`, `agent`, `model`, and `verify.method` verbatim, with the
  explicit rule "Use ONLY these tokens; never invent others." Small models anchor on
  early, list-formatted constraints far better than on enums buried inside a long JSON
  example string.

### 2. Corrupted numbers (`"difficulty":0.2,-0.79,`)
- Added a dedicated line: difficulty is **a single decimal 0–1, at most two decimals**,
  written **once**, "no extra digits, no second number, no trailing tokens." Also added a
  global "Output every field exactly once and stop after the closing brace" instruction so
  the model doesn't continue generating after a valid object (the source of the trailing
  `,-0.79,` garbage and many of the unparseable rows).

### 3. JSON discipline (unparseable rows — heaviest on `tiered`/volume tasks)
- First sentence now demands **ONE valid JSON object and NOTHING else: no prose, no
  markdown, no code fences, no comments**, plus the explicit stop-after-brace rule. This
  directly targets the unparseable cluster (5/18 easy, 5/13 game).

## Preserved (must not drift)
- Same 9 top-level keys; the example is shown in the **exact alphabetical serialization
  order** the trained target uses (`coordination, difficulty, escalate, rationale, risks,
  roster, schema_version, shape, verify`), so the in-context example matches what
  `serde_json` emits and reinforces the real target rather than fighting it.
- Same agent pool semantics (claude opus/sonnet/haiku, codex gpt-5.x), same shape
  definitions, same escalate-to-opus guidance, same `coordination.order = roster roles`
  rule.

## Adoption note
The system prompt is **baked into every SFT example** (`dataset::to_plan_jsonl` writes it
as the `system` message). Adopting this candidate therefore requires **regenerating
`plan-dataset.jsonl` and retraining** — the deployed model must be served with the same
prompt it was trained on.

## How to A/B it first (no retrain)
`eval_llm_conductor.py` already supports `--system-file`, so the candidate can be tried at
inference against the current model:

    python eval_llm_conductor.py --model talesml-orch3 --system-file CONDUCTOR_PLAN_SYSTEM.candidate.txt

Note: `eval_plan.py` currently loads the prompt from the Rust source via `--dataset-rs`
and has no `--system-file` flag, so a true plan-grade A/B needs either (a) a one-line
`--system-file` addition to `eval_plan.py` (do **not** edit it while the training job is
live), or (b) compare the new prompt's effect only after it is hand-translated into the
Rust `&str` literal and a fresh dataset is regenerated + retrained. Expect a pre-retrain
A/B to mainly de-risk the JSON/enum/number discipline; the roster-match gains land only
after retraining on the regenerated dataset.
