# Tales

A lightweight multi-agent AI coding orchestrator. It drives multiple AI coding
CLIs (Claude Code, Codex, Open Code) as subprocesses so they can collaborate on
the **same project** in real time — debate a plan, code in isolated git
worktrees, then **recommend who should execute** while requiring **your
confirmation** before anything runs.

The unified console *is* the tool — it works like any other AI terminal (Claude
Code, Codex, Warp), you just have more than one model in the room. Just type
`tales`: it opens a terminal workspace with Tales as the default pane, plus
sibling panes for shells and agent CLIs.

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
  - `agent/` — the `AgentAdapter` trait + the Claude / Codex / Open Code
    adapters, plus `KNOWN_TOOLS` + `make_adapter` (one registry the picker, the
    CLI, and the orchestrator all read from).
  - `event.rs` / `bus.rs` — the only core↔frontend contract (broadcast events,
    mpsc commands), so a web dashboard drops in later with zero core changes.
  - `supervisor.rs` — process lifecycle / zombie prevention.
- **`tales-cli`** — the `tales` binary: bare `tales` or `tales term` opens the
  terminal workspace; `solo` / `discuss` / `run` are the scriptable counterparts.
- **`tales-tui`** — the `tales-tui` binary: the interactive terminal workspace
  with a default Tales orchestrator pane and sibling shell/agent panes.
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
| — | Interactive terminal workspace (Tales default pane, shell/agent panes, plan handoff) + Open Code adapter | ✅ done |

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

### The interactive terminal workspace (just type `tales`)

```sh
cargo build --release
tales                       # opens Tales as the default terminal pane
tales term                  # same as bare tales
tales-tui --demo            # try the whole flow with no API calls
tales-tui --classic         # old connect → prompt → plan screen
```

`tales` with no subcommand opens the terminal workspace:

1. **Tales pane** — the default pane is the Tales orchestrator. Type the planning
   prompt and press `Enter`; Claude/Codex discuss and draft the plan in place.
2. **Terminal panes** — `Ctrl-N` opens a shell, `Ctrl-X` opens Codex, `Ctrl-L`
   opens Claude Code, and `Ctrl-O` opens Open Code. `Tab` switches focus.
3. **Observe / intervene** — focused agent panes receive your keystrokes
   directly. `Ctrl-A` sends an explicit approval only when that pane is waiting.
4. **Execute / hand off** — at the gate, `Enter` or `/confirm <n>` launches the
   selected executor in its own live pane and sends it the Tales plan. You can
   also focus any existing agent pane and press `Ctrl-S` to send the plan there.

The legacy single-pane planner is still available with `tales-tui --classic`:
connect tools, type a task, interject with chat commands, then confirm or reject
the recommended executor.

Launch from inside a harness via the bundled skill — `/tales` opens this same
terminal with the harness you're in pre-connected: a Claude Code command
(`.claude/commands/tales.md`) and a Codex prompt (`codex/prompts/tales.md`).
See `skill/tales/SKILL.md`.

You can also pre-connect / pre-fill explicitly, or pass a task to skip setup:

```sh
tales-tui --connect claude --connect codex
tales-tui --connect claude --prefill "Design a rate limiter"
tales-tui "Design and build a rate limiter" --drafter claude --critic codex  # immediate
```

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
./target/debug/tales solo --model sonnet "Reply with exactly: hello from tales"
```

Requires the `claude` CLI installed and authenticated.
