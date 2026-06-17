---
description: Launch Tales (Claude Code + Codex) live in your terminal
argument-hint: <task description>
allowed-tools: Bash(printf:*), Bash(chmod:*), Bash(open:*), Bash(tales-web:*)
---

Launch the Tales **terminal** supervisor for this task: **$ARGUMENTS**

The terminal UI needs a real TTY, so open it in a new Terminal window (running in
this project's directory):

!`printf '#!/bin/bash\ncd %q\nexec tales-tui %q\n' "$(pwd)" "$ARGUMENTS" > /tmp/tales-run.command && chmod +x /tmp/tales-run.command && open /tmp/tales-run.command && echo "Tales is launching in a new Terminal window."`

Then tell me, briefly:
- It opened in a new Terminal window — I can watch Claude Code and Codex discuss
  live, type to interject (I'm in the loop), `/attach <file>` to share an image
  or PDF, and approve the executor at the gate with `/confirm` (or `/reject`).
- Prefer the browser instead? Run `tales-web "$ARGUMENTS"`.

If `tales-tui` isn't found, tell me to build it from the Tales repo
(`cargo build --release`) and put `target/release` on PATH.
