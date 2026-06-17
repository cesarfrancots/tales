# /tales — launch Tales in your terminal

Launch the Tales **terminal** supervisor (Claude Code + Codex, live chat) for:

$ARGUMENTS

The terminal UI needs a real TTY, so open it in a new Terminal window running in
the current project. Do this:

1. Check the binary exists: `tales-tui --help`. If missing, tell me to build it
   from the Tales repo (`cargo build --release`) and put `target/release` on PATH.
2. Run exactly this to open it in a new Terminal window:

   ```
   printf '#!/bin/bash\ncd %q\nexec tales-tui %q\n' "$(pwd)" "$ARGUMENTS" > /tmp/tales-run.command && chmod +x /tmp/tales-run.command && open /tmp/tales-run.command
   ```

3. Tell me it opened in a new Terminal window — I can watch the two agents
   discuss, type to interject, `/attach <file>` to share media, and approve who
   executes at the gate with `/confirm`. Prefer the browser? Run `tales-web "$ARGUMENTS"`.
