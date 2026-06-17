---
name: tales
description: Open the Tales multi-agent terminal — Claude Code, Codex, and Open Code collaborate on a task and you pick who executes. Use for "/tales", "start tales", "open tales", "launch the multi-agent terminal", "have claude and codex collaborate".
---

# Open Tales

One-step launcher. Do exactly this and nothing else, then stop:

Run the Bash command:

```
tales open --connect claude
```

If the user gave a task, append it as one quoted argument, e.g.
`tales open --connect claude "build a rate limiter"`.

That opens the interactive Tales terminal in a new window with Claude Code
pre-connected (the user then adds Codex / Open Code, types a task, plans, and
picks who executes). **Do not** inspect the binary, read files, check PATH, weigh
options, or explain — just run the single command above, then stop.

Full documentation (flags, the connect→plan→execute flow, scriptable `tales
run`/`discuss`/`solo`) lives in the Tales repo README.
