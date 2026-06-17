---
name: tales
description: Start the Tales multi-agent live chat — launch Claude Code and Codex (and other AI CLIs) to collaborate on a task in a real-time chat you can watch and steer. Use when the user says "start tales", "launch the multi-agent chat", "have claude and codex collaborate", "tales chat", or wants two AI tools to plan/execute a task together with a human in the loop.
---

# Tales launcher

Tales runs multiple AI coding CLIs as collaborators on one task: they discuss in a
live chat, recommend who should execute, and **you** confirm the executor. This
skill starts Tales from whatever harness you're in (Claude Code, Codex, or a plain
shell) and lets you pick which tools to connect.

## Prerequisites (one-time)

- The Tales binaries built: from the repo root run `cargo build --release`.
  Binaries land at `target/release/tales` (headless) and `target/release/tales-tui`
  (live chat).
- The CLIs you want to connect, installed and authenticated:
  - **Claude Code** — `claude` on PATH (`claude --version`).
  - **Codex** — `codex` on PATH (`codex --version`).
- Run inside a git repository if you want the execute step isolated in a worktree.

## Pick the tools to connect

Tales connects two roles for a discussion: a **drafter** (proposes/executes) and a
**critic** (reviews/questions). Choose any installed CLI for each role:

| Role    | Flag        | Values            | Default  |
|---------|-------------|-------------------|----------|
| Drafter | `--drafter` | `claude` \| `codex` | `claude` |
| Critic  | `--critic`  | `claude` \| `codex` | `codex`  |

Per-role models: `--drafter-model`, `--critic-model` (e.g. `opus`, `sonnet`,
`haiku` for Claude; a model id for Codex). Other knobs: `--turns N` (discussion
length), `--sandbox read-only|workspace-write` (Codex write policy), `--cwd PATH`.

> Adding more tools (Gemini, Aider, …) is a matter of implementing one
> `AgentAdapter` in `tales-core`; the chat, gate, and skill don't change.

## Start the live chat

```bash
# From the Tales repo (or anywhere the binaries are on PATH):
tales-tui "Design and implement a rate limiter for our API" \
  --drafter claude --critic codex --turns 4
```

You'll see Claude and Codex discuss in real time. **Type in the box to talk to
them** (you're a participant), and at the gate decide the executor:

- type a message + Enter → injected into the conversation (human-in-the-loop)
- `/confirm` → execute with the recommended agent
- `/confirm claude` → override the executor
- `/reject` → decline to execute
- `/quit` or `Ctrl-C` → exit

Try it with **no API calls** first: `tales-tui "demo" --demo`.

### Headless (no TUI)

```bash
tales discuss "Design a rate limiter" --drafter claude --critic codex --turns 4
tales solo "Summarize this repo" --agent codex   # drive a single tool
```

## Launch from inside a harness

### Claude Code
A slash command is provided at `.claude/commands/tales.md` (copy it into your
project's `.claude/commands/` or `~/.claude/commands/`). Then in Claude Code:

```
/tales Design and implement a rate limiter   (claude drafter, codex critic)
```

It shells out to `tales-tui` with your prompt.

### Codex
A prompt is provided at `codex/prompts/tales.md`. Install it into Codex's prompts
dir (`~/.codex/prompts/`), then in Codex:

```
/tales Design and implement a rate limiter
```

### Any other harness / plain shell
Just call the binary directly:

```bash
tales-tui "your task here" --drafter <tool> --critic <tool>
```

## What happens under the hood

1. **Discuss** — drafter and critic alternate; the orchestrator relays each turn
   and streams it to the chat. Your messages are folded into the conversation.
2. **Recommend** — each tool votes (with confidence) on who should execute.
3. **Gate** — the run blocks until *you* confirm/override/reject. Execution is
   unreachable without your confirmation.
4. **Execute** — the chosen tool implements the plan (in its own git worktree
   when run in a repo), and the result can be merged.
