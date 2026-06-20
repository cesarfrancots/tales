<div align="center">

# ❯ tales

### two AIs, one terminal, you on the trigger

**Tales** runs AI coding CLIs — **Claude Code**, **Codex**, **Open Code**, and generic CLI rows like Gemini — side by side in your terminal.
They argue out a plan, recommend who should execute — and **nothing runs until you say so.**

[![MIT](https://img.shields.io/badge/license-MIT-2dd4bf?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/built%20with-Rust-c08cff?style=flat-square)](https://www.rust-lang.org)
[![size](https://img.shields.io/badge/binary-1.4%20MB-5cb0ff?style=flat-square)](#weighs-almost-nothing)
[![telemetry](https://img.shields.io/badge/telemetry-none-7ee0a3?style=flat-square)](#weighs-almost-nothing)

[**Documentation**](https://cesarfrancots.github.io/tales/docs.html) · [**Website**](https://cesarfrancots.github.io/tales/) · [**Quickstart**](#quickstart)

</div>

```console
❯ tales "add OAuth login"

  ▌ Claude Code  DRAFTER
    middleware layer — passport.js + the Google strategy, wired into the
    existing session store. small, isolated change.

  ▌ Codex  CRITIC
    approach is right. Claude has the better read on the current auth flow —
    let it execute. I'll review the diff after.

  ★ recommend  Claude Code
  ▸ pick executor   [1] Claude Code   [2] Codex      ← you decide

  You ▸ 1
  ✓ Claude Code executing in an isolated git worktree…
  ✓ done — clean diff ready for your review.
```

---

## Why two?

> One model has one opinion. **Two models have an argument.**

A single agent commits to its first framing and runs with it. Tales puts two strong
models with different priors in the same room: **one drafts, the other critiques**, and
the disagreement surfaces the assumptions a solo run would have buried — *before* the diff,
not after.

This isn't a hunch; the field keeps re-discovering it:

- **[OpenRouter model fusion](https://openrouter.ai)** blends several models for a stronger single answer.
- **[AWS Kiro](https://kiro.dev)** splits spec-then-build across agents.

Tales applies the same bet — *combine more than one model instead of trusting one* — where you
already work: **your terminal.** It does it for **planning** (the live drafter/critic discussion)
and gives you the tools to do it for **execution** (git-worktree isolation), with a human on
the gate the whole time.

| | |
|---|---|
| **drafter** | proposes the approach, writes the plan |
| **critic** | pokes holes, asks the questions, names the trade-offs |
| **you** | read the debate and pick the executor — a hard gate, nothing auto-runs |

## Mix the room, tier the cost

Tales isn't locked to two models or one vendor. **Any AI coding CLI can join** behind one
small adapter — Claude Code, Codex, Open Code, and generic CLI rows today — and the room doesn't have to be
uniform.

The payoff is **tiered execution**: run your *smart, expensive* models on the part that
matters — the plan and the argument — then hand the agreed plan to a *cheaper, faster* model
to implement. You pay top dollar for judgment, not for typing.

```sh
# two strong models plan + argue, a cheap/fast one implements
tales run "add OAuth login" \
  --drafter claude --drafter-model opus \
  --critic  codex  --critic-model  gpt-5  --critic-effort high \
  --execute gemini --execute-model gemini-2.5-flash
```

**Pick the model and the effort per seat.** Every participant takes its own `--<role>-model`
and `--<role>-effort` (e.g. Codex `low`/`medium`/`high`), and in the interactive terminal the
connect screen cycles both inline — `m` for model, `e` for effort.

**Hooking a new tool is a row, not an adapter.** Tools with bespoke wiring (Claude Code, Codex,
Open Code) name their adapter; any other turn-based CLI that prints its reply to stdout rides one
shared *generic* adapter, configured from a registry row (`run_args` / `model_flag` /
`prompt_flag`). Gemini, GLM, and Kimi ship as rows today; the connect screen auto-detects which
CLIs are actually installed on every run. Adding Kiro, Aider, or your own is a one-line change.

## Weighs almost nothing

No Electron, no cloud, no account. The whole thing is small, native Rust.

| | |
|--:|:--|
| **1.5 MB** | the `tales-tui` binary (size-optimized release build; `tales` 1.9 MB, `tales-web` 1.8 MB) |
| **~18.9k** | lines of Rust across a UI-agnostic core + three frontends |
| **0** | telemetry, cloud calls, or background daemons — runs on your machine, your keys |
| **∞** | models, eventually — add an `AgentAdapter`, add a row, done |

## The bet

A single frontier model — the best one money can buy — is the bar. **The bet behind Tales is that
an orchestrated team of *cheaper* models can reach for that bar**: two strong-but-not-top models
argue out the plan, a cheap fast model does the typing, and a human holds the trigger. The aim is
**frontier-class results at a lower blended cost** — *approaching* a model like Claude Fable 5,
not a claim to beat it (we haven't benchmarked against Fable 5 — it wasn't available). What
follows is the honest evidence so far: where this already pays off, and where one strong model is
still the better tool.

## Benchmark — where tiering helps, and where it doesn't

A **fair, end-to-end** test: implement a **regex engine** (`fullmatch(pattern, text)` over
literals, `.`, `* + ?`, `|`, groups, character classes, escapes), scored against **400 hidden
cases** with Python's `re` as the oracle (no `import re`). Two conditions, each producing a real
`solution.py` that gets scored:

| Condition | Wall-clock | Cost | Quality |
|---|--:|--:|:--:|
| **Opus 4.8 — solo** (plans + executes) | **355s** | **$1.53** | 100% |
| **Tales** — Opus+Codex plan → Haiku executes | 481s | $1.74 | 100% |
| └ planning (two smart models, in parallel) | 176s | $1.00 | |
| └ Haiku execution (iterates against `re` until green) | 304s | $0.74 | |

On *this* task the single model won on every axis — **36% faster, 13% cheaper, same quality.**
The cheap-executor thesis lost here, and the reason is the point.

**Why it lost.** A regex engine's difficulty lives *in the implementation* (greedy backtracking,
empty-match termination, edge cases), not in separable boilerplate. So the cheap executor had two
options, both bad: implement **blind** and ship a systematic bug (43.8%, 224 crashes), or
**iterate against tests** until green — which it did (100%), but that took 47 tool-rounds and
~4.6M cached tokens, $0.74 and 304s. The iteration-to-correctness ate the per-token cost
advantage. Meanwhile Opus-solo wrote a correct ~350-line engine in one pass — and hit 100%
*blind*, because the sandbox denied it a Python interpreter too.

### Where Tales beats a single model — and where it doesn't

The deciding factor is the **shape** of the work, not its size:

- ✅ **Mechanically voluminous execution** — boilerplate, repetitive endpoints, broad migrations.
  The two-planner cost is roughly fixed, so it amortizes; the cheap executor applies its cheap
  rate to a lot of volume it doesn't need to be *smart* about. Here, bigger helps Tales.
- ✅ **Ambiguous, architecture-defining work** with no known-correct answer — where two strong
  models arguing, plus a human on the gate, catch a bad design before it becomes a costly diff.
  A quality/risk play more than a cost/speed one.
- ❌ **Algorithmically hard, self-contained tasks** where correctness *is* the implementation. The
  cheap executor's capability gap and the lossy plan handoff both compound — bigger makes it
  *worse*, and a single frontier model wins (our regex engine, scaled up).

> Honesty notes: single trial per arm (the *direction* is structural; exact percentages wobble).
> Costs are each tool's own meter — Opus/Haiku via Tales' cost printer at Anthropic list prices
> (Opus 4.8 **$5/$25**, Haiku 4.5 **$1/$5** per M in/out), Codex from its `~/.codex/sessions`
> token ledger × gpt-5.5 rates ($5 / $0.50 cached / $30). The Haiku executor ran through an agent
> harness that re-sends context aggressively; a leaner executor iterates cheaper, but the dynamic
> holds. Both models were denied a Python interpreter, so Opus-solo's one-pass 100% is *despite*
> not being able to test.

### The planning phase is now parallel

That first run exposed two costs in planning: the two planners ran **sequentially** (latency = the
*sum* of their turns) and each turn **re-pasted the whole transcript**. Both are fixed:

- **Parallel rounds** — planners draft concurrently (round 1 independent, then one synthesizes a
  merged plan while the other cross-reviews), so a round costs `max`, not `sum`.
- **Delta-only context** — resumable adapters get only the *unseen* tail each turn, not the whole
  transcript re-pasted.

Live A/B on the same planning task (Opus drafter + Codex critic): **250.8s sequential → 176.5s
parallel — a 30% speedup.** (The synthesis step adds some Opus output, so parallel planning runs
~20% pricier — a speed-for-cost trade, tunable by moving the merge onto the cheaper model.)
Default for `tales run`/`discuss`; `--sequential` opts out.

## Quickstart

```sh
# 1. build (needs Rust + the claude / codex CLIs installed & authenticated)
git clone https://github.com/cesarfrancots/tales && cd tales
cargo build --release          # → target/release: tales, tales-tui, tales-web

# 2. check local tools and cached project context
tales doctor --all
tales context
tales profile refresh

# 3. open the terminal — connect your tools, then plan
tales

# 4. or go straight in, scriptable:
tales run "add OAuth login" --drafter claude --critic codex --execute claude

# 5. compare collaboration shapes without model calls
tales eval compare "add OAuth login"
```

Try the whole flow with **no API calls**: `tales-tui --demo` or `tales-web --demo`.

## The flow

1. **Connect** — pick which CLIs join: Claude Code, Codex, Open Code, Gemini, GLM, Kimi, or another registry row. Bring two; bring three.
2. **Plan** *(default)* — they discuss in a live chat. Type to interject — you're a participant.
3. **Pick** — they recommend an executor; you confirm, override (`/confirm <n>`), or reject. **This gate can't be skipped.**
4. **Execute** — the chosen tool builds the plan in an isolated git worktree. You get a clean, reviewable diff.

`/tales` from inside Claude Code or Codex opens the same terminal with that tool pre-connected.

## Architecture

A Cargo workspace with a strictly UI-agnostic core — frontends talk to it only through the bus.

- **`tales-core`** — the orchestrator. Every tool is one `AgentAdapter` emitting normalized
  `AgentEvent`s; the wildly different CLIs (Claude's bidirectional `stream-json` vs Codex's
  turn-based `exec`/`resume`) differ only through `AgentCaps` flags — never an `if claude {…}`.
- **`tales-cli`** — the `tales` binary: bare `tales` opens the interactive terminal; `run` / `discuss` / `solo` are the scriptable counterparts; `doctor`, `context`, `profile`, and `eval` preflight tools/cache/memory/evaluation state with no model calls.
- **`tales-tui`** — the interactive terminal: connect → plan → pick → execute.
- **`tales-web`** — a local browser view (axum + WebSocket) of the same session, with a pre-session workspace/task picker so you do not need to `cd` first.

Tales caches a compact repo map/manifest context outside the project and injects it into first planning prompts. It also keeps an opt-in local workspace profile under the Tales cache tree with metadata only: commands, preferred tools, warnings, report paths, and run summaries. Reports include prompt telemetry, provider token/cost data when adapters expose it, local-change handoff summaries, deterministic optimization hints, and recommendation inputs.

The shared `SessionConfig` contract now describes task, cwd, seats, prompt budget, report paths, and the approval policy used by CLI/web preflight. `--dry-run --json` includes that config, workspace profile status, and deterministic smarter/faster/cheaper tool recommendation chips.

Adding a tool (Gemini, Aider, …) is one adapter impl or one generic row in `KNOWN_TOOLS`; the picker,
CLI, and orchestrator all read that registry. Full details in the
[**documentation**](https://cesarfrancots.github.io/tales/docs.html).

## Built by Tales

This repo dogfoods itself. The [`landing/`](landing/) website *and* its
[documentation](https://cesarfrancots.github.io/tales/docs.html) were produced **with Tales** —
Claude Code and Codex drafting and reviewing each other through `tales discuss`, finalized
against the source. Eat your own cooking.

## Status

Live today: multi-agent discussion, parallel planning, cached project context, local workspace profiles, prompt forecasts, deterministic eval comparisons,
recommendation + a hard confirmation gate, git-worktree execution & merge, the interactive
terminal workspace, browser supervision UI with workspace picker and command palette, report writers, and bespoke/generic tool adapters.
Hardened against deadlocks and zombie processes; the test suite and strict clippy pass before push.

---

<div align="center">
<sub>two models, one terminal, you on the trigger · <a href="LICENSE">MIT</a></sub>
</div>
