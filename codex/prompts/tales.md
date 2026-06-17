# /tales — launch a Tales multi-agent collaboration

You are being asked to start a **Tales** multi-agent session on the task:

$ARGUMENTS

Tales lets Claude Code and Codex collaborate on a task in a live chat, then a
human confirms who executes. Do the following:

1. Verify the `tales` binary is available: run `tales --help`. If it is missing,
   tell me to build it from the Tales repo with `cargo build --release` and to
   put `target/release` on PATH.
2. Run the headless discussion and show me the output:

   ```
   tales discuss "$ARGUMENTS" --drafter codex --critic claude --turns 4 --sandbox read-only
   ```

   (Here Codex drafts and Claude critiques — swap `--drafter`/`--critic` if I ask.)
3. Summarize the agreed plan and the recommended executor.
4. Tell me that for the **interactive live chat** — where I can type to the agents
   and confirm the executor myself — I can run in a terminal:

   ```
   tales-tui "$ARGUMENTS" --drafter codex --critic claude
   ```
