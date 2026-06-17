---
description: Launch the Tales live supervisor (Claude Code + Codex) in your browser
argument-hint: <task> [--drafter claude|codex] [--critic claude|codex]
allowed-tools: Bash(tales-web:*), Bash(tales:*), Bash(pkill:*)
---

Start the Tales browser supervisor for this task: **$ARGUMENTS**

Launch it detached — it serves a live chat and auto-opens your browser at
http://127.0.0.1:7878:

!`tales-web "$ARGUMENTS" --drafter claude --critic codex >/tmp/tales-web.log 2>&1 & sleep 1; echo "Tales supervisor → http://127.0.0.1:7878 (log: /tmp/tales-web.log)"`

Then tell me, briefly:
- It's running in my browser; I can watch Claude Code and Codex discuss live,
  type to interject (I'm in the loop), and **approve the executor** at the gate.
- To stop it: `pkill -f tales-web`.

If the `tales-web` binary isn't found, tell me to build it from the Tales repo
with `cargo build --release` and put `target/release` on PATH.
