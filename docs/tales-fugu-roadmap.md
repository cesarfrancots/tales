# Tales → Fugu-Class: Roadmap & Model-Build Feasibility

Generated: 2026-06-22

## Status — shipped on `feat/coordinator-model` (PR #2)

- ✅ **Coordinator model** (Lever 4 / Tier-2 router; Phase D/E essence) —
  `tales-core::coordinator`: a pure-Rust MLP routing each task to solo / debate /
  tiered plus a difficulty/tier estimate. Wired into `tales run` as advisory
  routing; `tales coordinator {train,predict,show}`.
- ✅ **Run-trace flywheel** (Phase A substrate) — `tales-core::trace`: local,
  telemetry-free; the coordinator retrains on successful runs.
- ✅ **Verify-and-iterate loop** (Phase B / Lever 1) — `tales-core::verify` +
  orchestrator `Phase::Verifying`: `tales run --verify "<cmd>"` iterates the
  executor to green, up to `--verify-max`.
- ✅ **Verify-failure escalation** (Phase C) — `tales run --escalate <tool>` hands
  the back half of the fix attempts to a stronger, distinct executor when the
  cheap one stalls (cheap-first, strong-to-finish).
- Reviewed across three adversarial passes; `fmt` + `clippy -D warnings` + the
  full workspace test suite green.

Remaining (scoped follow-ups): **Phase F** best-of-N parallel execution with
verifier selection, **Phase D** LLM-as-conductor variant, **Phase G** distilled/RL
conductor.

## 0. The strategic insight (read this first)

Fugu looks like "a smart model that routes." It isn't, really. Its moat is **four
layers, and the model is the *last* one**, ranked here by how hard each is to build:

1. **Eval / reward infrastructure** — a large, auto-scored task suite. *Hardest, most valuable.*
2. **Data flywheel** — every run emits structured traces that become training data.
3. **A trained coordination policy** — the "model" (their papers: TRINITY = *evolved*
   coordinator, Conductor = *RL-trained* natural-language coordination).
4. **A frontier model pool** — Gemini 3.x Pro / Opus / GPT-5.x to route across.

The non-obvious part: **layer 3 (the model) is downstream of layers 1–2.** You cannot
train or evolve a coordinator without an eval suite to score it and a data flywheel to
feed it. Sakana did not build a foundation model — they trained a *coordinator policy on
top of existing models*.

Two consequences for Tales:

- **We already have layer 4.** Tales' connected CLIs (Claude Code, Codex, Gemini) are
  thin wrappers over exactly the frontier models Fugu routes across. The pool is *not* the gap.
- **The path to "our own model" runs *through* the work that makes Tales better today.**
  Verification, real evals, and trace logging are both (a) the biggest near-term quality
  wins and (b) the prerequisite substrate for ever training a coordinator. We do not choose
  between "improve Tales" and "build a model" — the first three phases are the same work.

**Goal, restated honestly:** output-quality parity with **Claude Fable 5** at lower
blended cost, *plus* Fugu-like adaptivity (verify, escalate, route per task). **Not**:
become a cloud black box. Non-negotiables that survive every phase below — **local,
zero-telemetry, human-on-trigger by default, ~1.5 MB binary, MIT.**

---

## 1. The gap, concretely

| Capability | Fugu | Tales today (code) | Target phase |
|---|---|---|---|
| Coordination logic | Trained / evolved policy | `RuleConductor` round-robin (`conductor.rs:57`) | D, G |
| Role assignment | Dynamic Thinker/Worker/Verifier | Fixed Drafter/Critic/Executor (`conductor.rs:14`) | B, D |
| **Verification** | First-class Verifier, multi-turn, iterate-to-green | **none** — `collect_turn → Phase::Done` (`orchestrator.rs:1283`) | **B** |
| Execution | Coordinates many models, selects | exactly **one** executor (`run_execution`, `:1232`) | F |
| Difficulty handling | "Ultra" reaches a deeper pool | one fixed shape regardless of task | C |
| Cross-run learning | trained on outcomes | **stateless** — `aggregate()` is votes-only (`recommend.rs:219`) | A, E |
| Evals | real outcome scoring | **mock** — `run_mock_eval` forecasts cost, runs 0 model calls (`eval_harness.rs:99`) | A |
| Routing | learned per task | confidence-weighted live vote, no prior (`recommend.rs:219`) | E |

The throughline: **Tales is a one-shot, open-loop pipeline. Fugu is a closed loop**
(verify → score → learn → route). Phases A–E close the loop.

---

## 2. The phased roadmap

Each phase ships independently and is reversible. Effort assumes one strong dev with AI
assist. "Ships as" maps to the existing lockstep SemVer cadence in `CHANGELOG.md`.

### Phase A — Real evals + run traces (the substrate)
- **Goal:** turn `tales eval` from a cost *simulator* into an outcome *scorer*, and make
  every run emit a typed training trace.
- **Why:** nothing downstream (learning, routing, a trained model) can exist without a
  reward signal and logged data. This is layers 1–2 of Fugu's moat.
- **Code touchpoints:**
  - `eval_harness.rs`: add an `EvalMode`-driven *real* run path beside `run_mock_eval`;
    add a scored task fixture format (start by generalizing the regex/oracle harness from
    the README benchmark — `task + hidden cases + oracle`).
  - New `RunTrace` type persisted to `.tales/runs/<id>/trace.jsonl`: task features
    (len, file count, language, ambiguity flags), shape (solo/debate/tiered), roster,
    models, roles, turns, **verifier results**, final pass/fail, cost, latency. You already
    emit most of this loosely in `events.jsonl` — formalize it into a typed schema.
- **Deliverable:** `tales eval run <suite>` executes shapes against scored tasks and writes
  per-run traces + an aggregate scorecard.
- **Definition of done:** running the suite twice produces comparable, real quality scores
  (not forecasts); traces are machine-parseable and stable (`schema_version`).
- **Effort:** 1–2 weeks. **Ships as:** 0.5.0.

### Phase B — Verify-and-iterate loop (highest ROI)
- **Goal:** add a `Phase::Verifying` + `Role::Verifier`; after the executor's diff, verify;
  on failure feed the failure back and let the executor iterate, capped.
- **Why:** your *own* benchmark proves this is the lever — the cheap executor went 43.8%
  (blind) → 100% only by iterating against tests. Fugu makes this a built-in role; Tales
  leaves it to chance. It also *produces the reward signal* Phase A needs. Two birds.
- **Code touchpoints:**
  - `conductor.rs:14` — add `Role::Verifier` (and `is_planner` stays false for it).
  - `orchestrator.rs:91` — add `Phase::Verifying` between `Executing` and `Done`.
  - `orchestrator.rs:67` `PromptPhase` — add `Verification`.
  - `orchestrator.rs:1283` — **the exact insertion point.** Today:
    `let output = self.collect_turn(...).await?; self.set_phase(Phase::Done);`
    Becomes: `collect_turn → run_verifier → if fail and under cap: feed failure via
    compose_execution_prompt (reuse the lean/delta handoff) and loop; else Done`.
  - Extend `RunOutcome::Executed` (`:42`) with `verified: bool` + `iterations: u8`.
  - Verifier strategy: run the task's tests/oracle when present; else a critic-on-diff turn.
- **Deliverable:** executor output is checked and iterated to green (or to the cap) before Done.
- **Definition of done:** a fixture that fails on first attempt and passes after feedback;
  one runnable test asserting the loop (fail → feedback → pass) and the iteration cap.
- **Effort:** 1–2 weeks. **Ships as:** 0.6.0. **Do this first.**

### Phase C — Difficulty-aware routing + model escalation
- **Goal:** estimate task difficulty; start cheap; escalate the executor to a stronger
  model/tool on verifier failure or low confidence. This is Fugu's "everyday vs Ultra"
  as control flow — no training.
- **Code touchpoints:** a `difficulty.rs` heuristic first (task length, file count,
  algorithmic-keyword/ambiguity signals); wire an escalation step into the Phase-B loop
  (fail at cheap tier → re-run at a stronger seat before giving up); surface the tier in
  `RunTrace` so Phase E can learn the right starting tier.
- **Deliverable:** `--escalate` policy; cheap-first execution that promotes on failure.
- **Definition of done:** a hard fixture that fails cheap, escalates, and passes; trace
  records the escalation.
- **Effort:** ~1 week. **Ships as:** 0.7.0.

### Phase D — LLM-as-Conductor (the first "orchestrator brain")
- **Goal:** implement `LlmConductor: Conductor` — reads the `Blackboard`, decides next
  speaker / when to stop / when to demand a plan / when to spawn the verifier / when to
  escalate. Replaces round-robin with *adaptive* coordination. The single most "Fugu-like"
  behavioral change, and it needs **no training** — the trait was literally designed for
  this (`conductor.rs:6`: "An LLM-driven conductor can later implement the same trait
  without touching the orchestrator").
- **Code touchpoints:** new `LlmConductor` implementing `Conductor::next_turn`; it calls a
  connected model (or local Qwen) with the blackboard + a structured decision schema; the
  orchestrator already accepts any `Box<dyn Conductor>` — parameterize the `RuleConductor::new`
  site to inject it.
- **Deliverable:** `--conductor llm` mode.
- **Definition of done:** on an A/B suite (Phase A), the LLM conductor matches-or-beats
  round-robin on quality at equal-or-lower turn count.
- **Effort:** 1–2 weeks. **Ships as:** 0.8.0.

### Phase E — Empirical router (your first *trained* model)
- **Goal:** once Phase A's flywheel has a few hundred scored runs, train a **small
  contextual-bandit / gradient-boosted router** that predicts the best shape+executor+tier
  from task features. Blend its prior with the live votes in `aggregate()`.
- **Why:** this is the honest, achievable "mini orchestrator model" — small, CPU-trainable,
  interpretable, **local**, and it directly closes the "stateless, no learning" gap.
- **Code touchpoints:** offline trainer (separate Python sidecar, see §4); export a tiny
  artifact (ONNX or plain coefficients) loaded at runtime; `recommend.rs:219` `aggregate`
  gains a `prior: Option<RouterPrior>` blended with vote confidence.
- **Deliverable:** `recommend` consults an empirical prior; cold-start falls back to votes.
- **Definition of done:** on held-out tasks, prior-blended routing beats votes-only on
  realized quality/cost.
- **Effort:** 2–4 weeks (incl. waiting for data). **Ships as:** 0.9.0.

### Phase F — Best-of-N parallel execution + selection
- **Goal:** for hard tasks (Phase C), run N executors in parallel git worktrees; the
  verifier (Phase B) selects the diff that passes. Reuses the existing parallel-round demux
  and worktree isolation.
- **Why:** a known quality multiplier — but a cost multiplier too, so gate it to the
  algorithmically-hard tasks where a single executor struggles. Do it *last*.
- **Effort:** 1–2 weeks. **Ships as:** 0.10.0.

### Phase G — (optional, research) distilled / RL'd conductor
- **Goal:** distill the Phase-D LLM-conductor's decisions + successful traces into the
  **local Qwen3.5-9B** via SFT, producing a free, fast, offline conductor. Later (only if
  warranted): RL/evolutionary training on real outcome rewards — Fugu's actual approach.
- **Honest status:** months of work, GPU/compute, ML expertise, regression-guarding evals.
  This is the moat. Don't start until the flywheel is mature and there's a concrete reason.
- **Ships as:** 1.x, behind a feature flag.

---

## 3. Can we build our own mini orchestrator model? — feasibility

Short answer: **yes for the useful versions, no for true Fugu parity (without a research
budget).** "Orchestrator model" is not one thing — it's a ladder:

| Tier | What it is | Training? | Compute | Data needed | Time | Verdict |
|---|---|---|---|---|---|---|
| **0** | Heuristic router | none | none | none | — | *Have it (`RuleConductor`)* |
| **1** | **Prompted LLM-as-conductor** | none | none / 1 GPU for local Qwen | none | **days** | **Do it (Phase D)** |
| **2** | **Classical ML router** (bandit / GBDT on logged features) | offline, CPU | laptop CPU | ~hundreds of scored runs | **1–3 wks** | **Best first *trained* model (Phase E)** |
| **3** | **Fine-tuned small LLM conductor** (SFT/distill into Qwen3.5-9B) | SFT | 1 GPU (rent/own) | ~1–10k decision traces | **1–2 mo** | **Achievable real project (Phase G)** |
| **4** | **RL/evolutionary policy** on outcome rewards (Fugu's TRINITY/Conductor) | RL/evolution | multi-GPU | large auto-scored suite | **many mo** | **Research; needs funding/compute** |

**What's actually hard** (and it isn't the model):
- **The eval suite.** You can't train/evolve/score without a big, automatically-graded
  task set. This is Phase A and it's the real bottleneck — same as it was for Sakana.
- **Reward design.** "Good orchestration" = quality *and* cost *and* latency. Multi-objective
  reward shaping is where RL projects (Tier 4) live or die.
- **Data volume.** Tiers 3–4 need thousands of high-quality decision/outcome traces. The
  flywheel (Phase A) has to run for a while first.
- **The stack boundary.** Tales is Rust; training is Python (PyTorch/HF/sklearn). Keep them
  separate (see §4) — don't drag a training runtime into the 1.5 MB binary.
- **Serving locally.** A Tier-2 artifact is trivial (ONNX/coeffs). A Tier-3 LLM means
  llama.cpp/Qwen at runtime — fine, you already run Qwen locally, but it's a dependency.

**What you have going for you** (more than most indie attempts):
- **A frontier pool already** — your CLIs wrap Opus/GPT-5.x/Gemini. Fugu's expensive layer 4
  is free to you.
- **Local Qwen3.5-9B + TurboQuant** — a ready base model to distill into for Tier 3, no
  per-token cost, fully offline.
- **Structured traces already emitted** (`events.jsonl`) — the data substrate is half-built.
- **A clean `Conductor` trait + phase machine** — the seams for a learned policy are already
  cut.

**Recommendation:** target **Tier 1 now (Phase D), Tier 2 next (Phase E)** — that combination
*is* a "mini orchestrator model" in every sense that matters to a user, at indie cost. Treat
**Tier 3** as a fun stretch once the flywheel is mature, and **Tier 4 as out of scope** unless
this becomes a funded effort. Fugu's parity isn't gated by model architecture — it's gated by
eval scale and compute, which is exactly the part money buys.

---

## 4. Non-negotiables & risks

- **Keep the human gate as the default.** Autonomy (Fugu ran 123 experiments unattended) is
  an *opt-in* mode where the **verifier replaces the human** as the correctness check — never
  delete the gate to chase autonomy. The gate is the product's identity.
- **Zero telemetry, ever.** All traces/training data stay local and opt-in. No phone-home.
- **Don't bloat the binary.** The trainer is a *separate Python sidecar repo*, not linked into
  `tales-core`. Runtime only ever *loads* a small exported artifact (Tier 2) or talks to a
  local model server (Tier 3). The 1.5 MB promise holds.
- **Every phase reversible & independently shippable.** Map to the existing SemVer cadence;
  feature-flag anything experimental (Phase G).
- **Verifier honesty.** When there's no oracle/tests, a critic-on-diff verifier can rubber-stamp.
  Track verifier precision in traces; don't let "verified" become theater.

---

## 5. Sequence at a glance

```
A (real evals + traces)  →  B (verify loop)  →  C (escalation)
        │                        │
        │                        └─ B is highest ROI; start here even before A is complete.
        ▼
D (LLM conductor)  →  E (empirical router = first trained model)  →  F (best-of-N)  →  G (distilled/RL, optional)
```

First concrete move: **Phase B**, because it is the largest single quality jump, it is the
gap your own benchmark documents, it slots into one insertion point (`orchestrator.rs:1283`),
and it is the prerequisite that makes A, E, F, and G meaningful — you cannot score, learn, or
select without a verifier defining "correct."
