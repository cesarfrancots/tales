# Changelog

All notable Tales release changes are tracked here.

Tales uses one lockstep SemVer version for the Rust workspace. The version lives in the root `Cargo.toml`.

## Unreleased

- Added the **orchestration coordinator** (`tales-core::coordinator`): a tiny,
  dependency-free MLP that routes a task to a collaboration *shape* — solo,
  debate, or tiered — plus a difficulty/tier estimate for cheap-first escalation
  (Fugu's "everyday vs Ultra" as a learned routing decision, not an `if/else`).
  Trained by deterministic gradient descent on an embedded seed corpus (the
  structural priors from the README benchmark) and serialized to a few-KB
  `.tales/coordinator.json`. Pure `std` — no ML runtime, no new dependencies.
- Added the `tales coordinator` CLI (`train`, `predict`, `show`): retrain from the
  seed corpus plus this workspace's run traces, route a task, or inspect the
  cached model. Zero-config — the model auto-seeds and caches on first use.
- Added run traces (`tales-core::trace`): each non-demo `tales run` appends a
  `RunTrace` to a local, append-only `.tales/traces.jsonl`, and the coordinator
  retrains on the successful ones — a local, telemetry-free learning flywheel.
- `tales run` now prints the coordinator's routing recommendation alongside the
  chosen seats (advisory only — the hard human execution gate is unchanged).
- Added the **verify-and-iterate loop** (`tales-core::verify`, Phase B of the
  roadmap): `tales run --verify "<cmd>"` runs a check in the executor's working
  tree after execution and, on failure, feeds the failing output back to the
  executor to iterate up to `--verify-max` attempts (default 3). The check is a
  command — deterministic ground truth — so `cargo test`, `pytest`, a build, or a
  lint all work. Adds `Phase::Verifying`; the run records `verified` (not merely
  `executed`) into the coordinator's trace flywheel. Runs in the executor's
  worktree when `--worktree` is set.
- Added the **local LLM conductor** (`tales-core::llm_conductor`, Phase D): an
  opt-in `LlmConductor` that asks a fine-tuned model served over a local
  OpenAI-compatible endpoint how to route a task — the same `CONDUCTOR_SYSTEM`
  prompt and `{"shape","difficulty"}` reply it was trained on — and turns the
  answer into a `coordinator::Strategy`. Wired into `tales run` as
  `--conductor llm [--conductor-url http://localhost:8080/v1]`, replacing the
  advisory keyword routing chip when enabled. It **never hard-fails**: an
  unreachable server, an unparseable reply, or a binary built without the
  feature all fall back to the keyword `coordinator`, and the chip labels the
  fallback honestly (`conductor[llm→keyword]`). Behind the `llm-conductor` cargo
  feature so the lean default build pulls no HTTP client; the keyword coordinator
  stays the zero-cost default and the human execution gate is unchanged.
- Added **verify-failure escalation** (Phase C): `tales run --verify "<cmd>"
  --escalate <tool> [--escalate-model <m>]` hands the back half of the fix
  attempts to a stronger, distinct executor when the primary stalls — Fugu's
  deeper-pool escalation applied to fixing, not just routing. The escalation tool
  shares the executor's working tree so its fixes face the same check;
  `--escalate` requires `--verify` and must differ from
  `--drafter`/`--critic`/`--execute`.
- Documented the path toward Fugu-class behavior in `docs/tales-fugu-roadmap.md`.

## 0.4.5

- Fixed the two pre-existing `clamp`-like lint warnings in the TUI input-height sizing (`.min(8).max(1)` → `.clamp(1, 8)`) so the workspace is clean under `cargo clippy --all-targets -- -D warnings`.
- Reformatted the editor and prompt-test code added in 0.4.1–0.4.3 so `cargo fmt --check` passes across the workspace.

## 0.4.4

- Added slash-command type-ahead: pressing `/` turns the footer into a live, filtered list of the matching commands with one-line descriptions, so the command set is discoverable without memorizing `/commands`.
- The hint narrows as you type (e.g. `/ha` → `/handoff`) and disappears for normal messages or an unknown `/token`.

## 0.4.3

- Sharpened the planner system prompts so the discussion produces better executor handoffs: the drafter is asked to name the concrete files/components it would change and to ground claims in the cached project context instead of guessing.
- The critic is now told to green-light a sound approach plainly instead of manufacturing concerns, and both planners are nudged to converge on what is settled and end with an executable, file-level handoff (files to change, order of steps).
- Applied the same "concrete and grounded" framing to the parallel round-1 and merge prompts.
- Factored the drafter/critic role intros into shared helpers so the resumable and stateless prompt paths can't drift apart.

## 0.4.2

- Added transcript scrollback: PageUp/PageDown scroll the conversation so earlier discussion is readable once it grows past the viewport, with a right-aligned "↑ scrolled" indicator while scrolled up.
- Sending a message, or the executor gate opening, snaps the view back to the live tail so you never miss new output or the action banner.
- Repurposed the page keys from the input box (the editor now follows the cursor on its own) to the conversation, matching how a chat/pager is expected to behave.

## 0.4.1

- Rewrote the Tales prompt as a full multi-line editor so long prompts are comfortable to write: insert and edit anywhere (not just append), Left/Right and Home/End motion, word motion (Alt+Left/Right), and word/line kills (Ctrl-W, Ctrl-U, Ctrl-K).
- Added real newlines in the prompt — Alt+Enter, Shift+Enter (where the terminal reports it), or Ctrl-J insert a line break; plain Enter still sends.
- Enabled bracketed paste so a pasted multi-line block lands as one edit instead of firing several premature submits; pasting into a focused agent pane forwards to its stdin.
- Added a visible block cursor that the input view follows, with the prompt box growing up to its max height as you type.
- Shared the new editor across the live chat, the classic prompt screen, and the terminal workspace pane, and surfaced the Alt+Enter newline hint in the footers, welcome tips, and help text.

## 0.4.0

- Changed the default Tales terminal flow from planning-first to discussion-first, with executor handoff proceeding unless both planner votes request a formal plan.
- Added `needs_plan` vote/report metadata and surfaced formal-plan consensus at the executor gate.
- Relabeled terminal guidance and recovery copy around discussions and executor handoffs while keeping `.tales/runs/<run>/plan.md` and `.tales/last-plan.md` compatible.
- Added Windows/Linux CI, a Windows PowerShell release check, and cross-platform `tales open` launcher support.
- Added a technical `cd` path prompt to the startup workspace browser.
- Added a startup workspace folder browser and permission prompt before the default Tales terminal opens.

## 0.3.0

- Added `.tales/runs/<run>/` recovery artifacts for terminal, `tales run`, and `tales discuss` sessions with `plan.md`, `events.jsonl`, and `manifest.json`.
- Added `tales recover` to list saved runs and print the newest or selected saved plan.
- Added terminal recovery commands: `/artifacts`, `/handoff [executor|number]`, and `/switch <executor|number>`.
- Added project-local MCP/tool config detection with safety warnings and Claude launches that disable project MCP loading when risky configs are present.
- Documented the reliability roadmap for policy, scanner, redaction, and retention hardening.

## 0.2.0

- Added the default terminal workspace welcome screen with Tales pixel art, tips, command cues, and readable tool status rows.
- Added `help`/`commands` guidance in the startup pane and `/help`/`/commands` during active Tales chats.
- Added the scriptable `tales commands` reference.
- Improved terminal pane readability with wrapped input/output and safer carriage-return handling for live CLI output.
- Saved selected executor handoff plans to `.tales/last-plan.md` before launching the executor pane.
- Added shared build/version metadata for Tales binaries and JSON session outputs.
- Added repository versioning rules for AI agents in `AGENTS.md`.

## 0.1.0

- Initial pre-1.0 workspace version.
