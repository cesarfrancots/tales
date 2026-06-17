# /tales — launch the Tales live supervisor

Start a **Tales** browser supervisor for this task:

$ARGUMENTS

Tales runs Claude Code and Codex as collaborators in a live chat that you watch
and steer from your browser. Do this:

1. Check the binary exists: `tales-web --help`. If missing, tell me to build it
   from the Tales repo (`cargo build --release`) and put `target/release` on PATH.
2. Launch it detached (it auto-opens the browser at http://127.0.0.1:7878):

   ```
   tales-web "$ARGUMENTS" --drafter codex --critic claude >/tmp/tales-web.log 2>&1 &
   ```

   (Here Codex drafts and Claude critiques — swap `--drafter`/`--critic` if I ask.)
3. Tell me it's live: I can watch the two agents discuss, type to interject, and
   approve who executes at the gate. To stop it: `pkill -f tales-web`.
