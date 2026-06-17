# Tales

A lightweight multi-agent AI coding orchestrator. It drives multiple AI coding
CLIs (Claude Code today, Codex next) as subprocesses so they can collaborate on
the **same project** in real time — debate a plan, code in isolated git
worktrees, then **recommend who should execute** while requiring **your
confirmation** before anything runs.

The unified console *is* the tool. You watch the orchestrator, not the
individual apps.

## Why CLIs, not the desktop apps

The desktop apps expose no way for another program to read their stream or inject
messages. The CLIs are built for exactly this:

- **Claude Code** — `claude -p --input-format stream-json --output-format
  stream-json --include-partial-messages --verbose`: bidirectional realtime
  streaming. Inject messages mid-turn, read tokens as they arrive.
- **Codex** — `codex exec --json` (turn-based) + `codex exec resume`.

The asymmetry (Claude streams mid-turn, Codex works in discrete turns) is hidden
behind a single capability flag, `AgentCaps::midturn_injection`. The engine and
UI never branch on *which* tool — only on capabilities.

## Architecture

A Cargo workspace enforces a UI-agnostic core:

- **`tales-core`** — orchestration. Cannot depend on any frontend.
  - `agent/` — the `AgentAdapter` trait + `ClaudeAdapter` (the heart).
  - `event.rs` / `bus.rs` — the only core↔frontend contract (broadcast events,
    mpsc commands), so a web dashboard drops in later with zero core changes.
  - `supervisor.rs` — process lifecycle / zombie prevention.
- **`tales-cli`** — the `tales` binary (`solo` / `discuss` / `run`).
- **`tales-tui`** — the `tales-tui` binary: the terminal live chat.
- **`tales-web`** — the `tales-web` binary: a browser live chat (axum + WebSocket).

All three frontends talk to the core *only* through the bus — adding `tales-web`
needed zero changes to `tales-core`.

## Status

| Milestone | What | State |
|---|---|---|
| M0 | Workspace + event/command bus + stub frontend | ✅ done |
| M1 | Process supervisor + Claude stream-json adapter (live tokens, tool calls, clean reaping) | ✅ done |
| M2 | Codex adapter (exec + resume) behind the same trait | ✅ done |
| M3 | Worktree manager (per-agent isolation, diffs) — *integration-tested* | ✅ done |
| M4 | Orchestrator + discussion loop (drafter/critic) — *mock-tested + live Claude↔Codex* | ✅ done |
| M5 | Recommendation stage + required confirmation gate — *gate-tested* | ✅ done |
| M6 | Gated execution + git-worktree isolation + merge (`tales run --worktree`) | ✅ done |
| M7 | **Live chat TUI** — watch + interject + decide (human-in-the-loop) | ✅ done |
| M8 | Hardening — per-turn timeout, cancellable graceful-then-kill shutdown | ✅ core (`tales-web` future) |
| — | Launcher skill (Claude Code + Codex commands) | ✅ done |

Two adversarial-review passes (run as multi-agent Workflows) found **17 real
bugs** total — 12 in the core (a Codex `turn.failed` deadlock, worktree merge
misclassification, branch collisions, …) and 5 in the worktree/shutdown code
(worktree+task leaks on error paths, per-turn-timeout not terminating the stuck
agent, process-group kill for tool/MCP grandchildren, …) — all fixed.

### Watch in your browser (easiest)

```sh
cargo build
./target/debug/tales-web "Design and build a rate limiter"   # then open http://127.0.0.1:7878
./target/debug/tales-web "demo" --demo                        # no API calls
```

A local web page streams the Claude↔Codex chat live; type to interject, and
click **Approve & run** (or **Reject**) at the gate. Nothing executes until you do.

### Live chat in the terminal

```sh
# real run (talk to them, decide the executor):
./target/debug/tales-tui "Design and build a rate limiter" --drafter claude --critic codex
# try the UI with no API calls:
./target/debug/tales-tui "demo" --demo
```

In the chat: **type to talk to them** (you're a participant), `/confirm` (or
`/confirm <agent>`) at the gate to execute, `/reject` to decline, `/quit` or
`Ctrl-C` to exit. The executor cannot run until you confirm.

Launch from inside a harness via the bundled skill: a Claude Code command
(`.claude/commands/tales.md`) and a Codex prompt (`codex/prompts/tales.md`).
See `skill/tales/SKILL.md`.

### Non-interactive pipeline & the dogfood

`tales run` is the scriptable full pipeline (discuss → recommend → auto-confirm
an executor → execute):

```sh
tales run "Build X" --drafter codex --critic claude --execute claude --turns 2
```

**This project's own `landing/` page was built by Tales** using exactly that
command — Codex drafted the plan, Claude critiqued it, both voted, and Claude
Code executed it into `landing/index.html` + `landing/style.css`. Open
`landing/index.html` in a browser. The executor is restricted to file-writing
tools so it can't stall on an unapproved shell prompt in headless mode.

Add `--worktree` to run the executor inside its own `git worktree` and merge the
result back into the current branch (clean diff + reviewable hand-off):

```sh
tales run "Add a /health endpoint" --execute claude --worktree
```

### Try the live discussion (M4)

```sh
cargo build
./target/debug/tales discuss --drafter claude --critic codex --turns 2 \
  --drafter-model sonnet --sandbox read-only \
  "Design a minimal rate limiter for a public REST API. Keep it to 5 bullet points."
```

Claude drafts, Codex critiques in real time, relayed through the orchestrator
and streamed to your console.

Full design: see the plan at
`~/.claude/plans/i-want-to-create-lazy-willow.md`.

## Try it (M1)

```sh
cargo build
./target/debug/tales --model sonnet "Reply with exactly: hello from tales"
```

Requires the `claude` CLI installed and authenticated.
