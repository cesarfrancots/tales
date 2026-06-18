<div align="center">

# ❯ tales

### two AIs, one terminal, you on the trigger

**Tales** runs **Claude Code** and **Codex** (and **Open Code**) side by side in your terminal.
They argue out a plan, recommend who should execute — and **nothing runs until you say so.**

[![MIT](https://img.shields.io/badge/license-MIT-2dd4bf?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/built%20with-Rust-c08cff?style=flat-square)](https://www.rust-lang.org)
[![size](https://img.shields.io/badge/binary-1.2%20MB-5cb0ff?style=flat-square)](#weighs-almost-nothing)
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
small adapter — Claude Code, Codex, and Open Code today — and the room doesn't have to be
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
| **1.4 MB** | the `tales-tui` binary (size-optimized, statically built; `tales` 1.6 MB, `tales-web` 1.4 MB) |
| **~9.7k** | lines of Rust across a UI-agnostic core + three frontends |
| **0** | telemetry, cloud calls, or background daemons — runs on your machine, your keys |
| **∞** | models, eventually — add an `AgentAdapter`, add a row, done |

## Benchmark — does tiering actually hold up?

A **fair** test on a hard, execution-heavy task: implement a **regex engine**
(`fullmatch(pattern, text)` over literals, `.`, `* + ?`, `|`, groups, character classes,
escapes), scored against **400 hidden cases** with Python's `re` as the oracle (solutions
may not `import re`). Every condition **plans first, then executes** — apples-to-apples:

| Condition (plan → execute) | Quality | Cost |
|---|:--:|---|
| Codex — solo (gpt-5.5, high reasoning) | **100%** (400/400) | **$0.98** — measured from its token ledger (498K total: 85.5K uncached + 401K cached input, 11.7K output) |
| Claude Opus 4.8 — solo (ultrathink) | **100%** (400/400) | **~$1–3** *(est.)* — whole task on Opus, ~60k blended tokens, no in/out split |
| **Tales** — Opus + Codex plan → **Haiku** exec | **100%** (400/400) | **~$1.0** — Opus plan **$0.47** (billed) + Codex plan **$0.40** (measured) + Haiku build **~$0.12** *(est.)* |

- **Quality is a flat tie.** Handed the Opus+Codex plan, the *cheap* Haiku executor wrote a
  flawless ~350-line engine — matching both strong solo models, zero failures on 400 cases.
- **The cost win is narrower than it looks — and only against the priciest model.** Tales runs
  **two** frontier planners (Opus $0.47 + Codex $0.40 = $0.87) before the cheap Haiku executor
  (~$0.12), landing at **~$1.0** — about the same as **Codex-solo's $0.98**, and below Opus-solo's
  ~$1–3. On a task this size the double-planning overhead roughly cancels the cheap-executor
  savings. Tiering pulls clearly ahead on cost only against an expensive solo model, or once the
  implementation is big enough that running it on Haiku instead of a frontier model saves more
  than two planners cost.
- **Tales is *not* faster.** The planning discussion adds latency; tiering buys a second opinion
  and (in the right regime) cost, not speed.

> Honesty notes: **Codex is now priced precisely** — from its own per-call token ledger
> (`~/.codex/sessions` rollout: 498,522 total tokens for the solo run, 52,202 for the Tales
> critic) × **gpt-5.5 metered API rates** ($5 input / $0.50 cached input / $30 output per 1M,
> cross-checked across OpenAI's developer docs, OpenRouter, and pricing aggregators). Tales' Opus
> planning ($0.47) is from Tales' own cost printer. The Opus-solo (~60k blended tokens) and Haiku
> (~41k tokens) figures lack an input/output split, so they stay estimates. Quality (100% / 100% /
> 100%) and every token count are measured. These are metered API prices; usage run through a
> ChatGPT/Claude subscription is bundled into the flat fee, not billed per-token.

The honest takeaway: on a well-specified task a single strong model already aces — and once you
price all the planners, tiering doesn't undercut a *cheap* strong solo run. Where it pays off is
**execution-heavy** work (where the cheap executor writes most of the tokens) and against
**expensive** solo models — *same quality, lower bill in that regime, just not faster.*

### Closing the speed gap (parallel planning)

That benchmark exposed two costs in the planning phase: the two planners ran **sequentially**
(latency = the *sum* of their turns) and each turn **re-pasted the whole transcript** (redundant
input tokens). Both are now fixed:

- **Parallel rounds** — planners draft **concurrently** (round 1 independent, then one
  synthesizes a merged plan while the other cross-reviews), so a round costs `max`, not `sum`.
  Default for `tales run`/`discuss` (`--sequential` opts out); the live TUI stays sequential.
- **Delta-only context** — adapters that resume their session (Codex/Claude/Open Code keep their
  own history server-side) are sent only the *unseen* tail each turn, not the whole transcript —
  a direct input-token cut.

Synthetic proof (two stub planners, 4 turns @ 1s each): **2.88s parallel vs 4.28s sequential**.
With real frontier models (30–60s/turn) the round-level `sum→max` saving scales up. A full live
re-benchmark to put new end-to-end numbers here is the next step.

## Quickstart

```sh
# 1. build (needs Rust + the claude / codex CLIs installed & authenticated)
git clone https://github.com/cesarfrancots/tales && cd tales
cargo build --release          # → target/release: tales, tales-tui, tales-web

# 2. open the terminal — connect your tools, then plan
tales

# 3. or go straight in, scriptable:
tales run "add OAuth login" --drafter claude --critic codex --execute claude
```

Try the whole flow with **no API calls**: `tales-tui --demo`.

## The flow

1. **Connect** — pick which CLIs join: Claude Code, Codex, Open Code. Bring two; bring three.
2. **Plan** *(default)* — they discuss in a live chat. Type to interject — you're a participant.
3. **Pick** — they recommend an executor; you confirm, override (`/confirm <n>`), or reject. **This gate can't be skipped.**
4. **Execute** — the chosen tool builds the plan in an isolated git worktree. You get a clean, reviewable diff.

`/tales` from inside Claude Code or Codex opens the same terminal with that tool pre-connected.

## Architecture

A Cargo workspace with a strictly UI-agnostic core — frontends talk to it only through the bus.

- **`tales-core`** — the orchestrator. Every tool is one `AgentAdapter` emitting normalized
  `AgentEvent`s; the wildly different CLIs (Claude's bidirectional `stream-json` vs Codex's
  turn-based `exec`/`resume`) differ only through `AgentCaps` flags — never an `if claude {…}`.
- **`tales-cli`** — the `tales` binary: bare `tales` opens the interactive terminal; `run` / `discuss` / `solo` are the scriptable counterparts.
- **`tales-tui`** — the interactive terminal: connect → plan → pick → execute.
- **`tales-web`** — a local browser view (axum + WebSocket) of the same session.

Adding a tool (Gemini, Aider, …) is one adapter impl + one row in `KNOWN_TOOLS`; the picker,
CLI, and orchestrator all read that registry. Full details in the
[**documentation**](https://cesarfrancots.github.io/tales/docs.html).

## Built by Tales

This repo dogfoods itself. The [`landing/`](landing/) website *and* its
[documentation](https://cesarfrancots.github.io/tales/docs.html) were produced **with Tales** —
Claude Code and Codex drafting and reviewing each other through `tales discuss`, finalized
against the source. Eat your own cooking.

## Status

`M0–M8` done: live multi-agent discussion, recommendation + a hard confirmation gate,
git-worktree execution & merge, the interactive terminal workspace, and an Open Code adapter.
Hardened against deadlocks and zombie processes; the test suite stays green on every change.

---

<div align="center">
<sub>two models, one terminal, you on the trigger · <a href="LICENSE">MIT</a></sub>
</div>
