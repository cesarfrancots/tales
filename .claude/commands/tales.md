---
description: Launch a Tales multi-agent collaboration (Claude + Codex) on a task
argument-hint: <task description> [--drafter claude|codex] [--critic claude|codex]
allowed-tools: Bash(tales:*), Bash(tales-tui:*)
---

Start a Tales multi-agent session on this task: **$ARGUMENTS**

Run the headless discussion (Claude drafts, Codex critiques) and stream the
result here:

!`tales discuss "$ARGUMENTS" --drafter claude --critic codex --turns 4 --sandbox read-only`

Then summarize the agreed plan and the recommended executor. Tell me that for the
**interactive live chat** — where I can talk to them and confirm the executor
myself — I should run this in my own terminal:

```
tales-tui "$ARGUMENTS" --drafter claude --critic codex
```
