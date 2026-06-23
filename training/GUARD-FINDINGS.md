# TalesSML Plan-Output Guard — Findings

Failure taxonomy and hardening recommendations built from the three baseline eval
runs (`plan-orch3-easy.json`, `plan-orch3-hard.json`, `plan-orch3-game.json`).
Total tasks graded: 42 (easy 18, hard 11, game 13).

## Failure taxonomy

| Mode | easy | hard | game | total | What it is |
|------|-----:|-----:|-----:|------:|------------|
| **1. Unparseable** (corrupt-float / truncation) | 5 | 2 | 5 | **12** | `parse_plan` returned `None`. The dominant defect. |
| **2. Empty shape `""`** (malformed before `shape`) | 1 | 0 | 1 | **2** | Parsed to a dict but `shape` empty / top keys missing — output garbled mid-stream. |
| **3. Roster mismatch + hallucinated enums** | 1 | 3 | 0 | **4** | Valid JSON, shape often correct (debate), but `roles_valid`/`agents_valid`/`models_valid` and `order_matches_roster` all fail → the model invented roster role/agent/model tokens. |

Aggregate: shape accuracy ~54–82%, plan-valid rate ~54–61%, unparseable 11–38% per run.

### Mode 1 — Unparseable (12 / 42, the priority)
Concentrated on **tiered** and a few **solo** tasks — the shapes whose rationale
text is longest, giving the float-token bug more surface. Examples:
- `migrate all the test files from mocha to jest` (tiered)
- `convert all callbacks to async await across the codebase` (tiered)
- `add the new traceId field to every API response across all services` (tiered)
- `port all the canvas draw calls to the new WebGL renderer across the codebase` (tiered)
- `optimize rendering by pre-rendering the static arena to an offscreen canvas` (solo)

Root cause (per `dataset.rs:457-461`): a raw f32 difficulty serialized as
`0.20000000298023224`; the SLM learned to hallucinate extra numeric tokens around
that long high-entropy string, e.g. `"difficulty":0.2,-0.79,-0.13,` — a stray
number run where a key is expected, which breaks JSON. The training target is now
rounded to 2 decimals, but the **served model may still drift**, so a runtime
repair is still wanted. `plan_guard._repair_corrupt_floats` removes the stray run
and keeps the first number, only when it makes invalid JSON parse.
(Truncated replies — cut off mid-object — also land here; those have no safe
repair and must fall back.)

### Mode 2 — Empty / malformed shape (2 / 42)
- `figure out the best approach and tradeoffs for offline-first sync` (debate → `""`)
- `balance all eight troop cards' hp and damage for fair matchups` (debate → `""`)

Output corrupted before/around the `shape` field. The guard flags these via
"missing top-level key" + "hallucinated shape ''" and reports `valid=False`.

### Mode 3 — Roster mismatch / hallucinated roster enums (4 / 42)
- `decide whether to shard the database and how` (debate)
- `decide whether to migrate all the cron jobs to the message queue ...` (debate)
- `weigh whether to rename the entire public API ...` (debate)
- `figure out how offline edits should reconcile when two devices conflict ...` (debate)

All on "decide/weigh/figure out" debate tasks with mechanical-sounding clauses.
The model picks the right *shape* but then emits a roster whose roles/agents/
models don't match the policy (and often aren't even valid enum tokens). The
guard catches each as a per-row issue and `order_matches_roster` mismatch.

## Recommendations

### (a) Inference-time guard — how the served conductor uses `plan_guard.py`
1. Call `validate_and_repair(raw_reply, expected_shape=None)` on every model
   reply. (`expected_shape` is for eval; leave `None` in production.)
2. Decision policy:
   - `valid == True` → use `result["plan"]` directly. (If `repaired == True`,
     log it for drift monitoring — the served model is emitting corrupt floats.)
   - `valid == False` but `plan` is not `None` with **only** a roster/order
     mismatch (Mode 3) → the *shape* is trustworthy; reconstruct the roster
     deterministically from the policy for that shape (port `orchestration_plan`'s
     per-shape roster table) instead of trusting the hallucinated one.
   - `valid == False` due to a **hallucinated/empty shape** (Mode 2) or
     `plan is None` (Mode 1 truncation) → **fall back to the keyword router**
     (`coordinator::advise`). Never ship a hallucinated shape.
3. Never let the guard *rewrite* a shape — flagging only. The fallback owns the
   recovery decision.

### (b) Rust `llm_conductor.rs` hardening (cite line numbers)
The Rust runtime currently parses only the v1 `{shape,difficulty}` reply and is
**more fragile than the Python guard**:
- `parse_decision` (lines 147-160) does a greedy first-`{`..last-`}` slice and a
  single `serde_json::from_str`. It has **no corrupt-float repair**, so a
  `"difficulty":0.2,-0.79,` reply fails to parse → silent keyword fallback
  (line 122-124). Port `_repair_corrupt_floats`: on the first parse failure,
  regex-strip the stray numeric run and retry once before giving up.
- The `#[serde(default)] difficulty` (lines 141-143) defaults a missing/garbled
  difficulty to `0.0` → wrongly routes to `Tier::Cheap`. Prefer deriving the
  default from the chosen shape (the ponytail "Upgrade" note at line 142 already
  flags this) so a parse-salvaged plan still tiers sanely.
- `shape_from_str` (lines 162-169) correctly returns `None` for a hallucinated
  shape like `codemod`, and `try_route` (lines 125-130) turns that into an error
  → keyword fallback. Good — keep this; it is the Mode 2 safety net. Just add a
  `tracing::warn!` naming the bad shape so drift is visible.
- When TalesSML v2 (full plan) is served through Rust, add a roster-validation
  pass mirroring `_validate`: if the shape is valid but the roster enums/order
  don't match the policy (Mode 3), rebuild the roster from `orchestration_plan`
  rather than trusting the model's roster.

### (c) Training-data additions for the next iteration (per mode)
- **Mode 1 (corrupt float):** the f32→2-decimal fix at `dataset.rs:461` removes
  the root cause for *new* training; keep it. Add a handful of
  format-reinforcing examples where difficulty is a clean 1–2 digit decimal, and
  keep the `response_format: json_object` constraint at inference. No stray-number
  examples are needed — don't teach the corruption.
- **Mode 2 (truncation / empty shape):** these cluster on longer debate
  rationales; cap `max_tokens` generously (plans are < 400 tokens) and ensure the
  training targets never approach the cap. Add more debate examples so the model
  is fluent enough not to stall mid-object.
- **Mode 3 (roster hallucination on debate):** add adversarial debate examples
  where the task contains mechanical verbs ("migrate", "rename", "convert") but
  the *decision* is the work — e.g. `decide whether to migrate all the cron jobs
  to the message queue` — each paired with the **exact** policy roster
  (`drafter/critic/executor/reviewer`, `claude/codex/claude/codex`, `review`).
  The dataset already has `ADV_DEBATE_TEMPLATES`; increase their weight and add
  the specific failing phrasings above so the model anchors the roster to the
  shape, not to surface keywords.
