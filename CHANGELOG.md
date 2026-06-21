# Changelog

All notable Tales release changes are tracked here.

Tales uses one lockstep SemVer version for the Rust workspace. The version lives in the root `Cargo.toml`.

## Unreleased

## 0.6.0

- Added transcript scrollback: PageUp/PageDown scroll the conversation so earlier discussion is readable once it grows past the viewport, with a right-aligned "↑ scrolled" indicator while scrolled up.
- Sending a message, or the executor gate opening, snaps the view back to the live tail so you never miss new output or the action banner.
- Repurposed the page keys from the input box (the editor now follows the cursor on its own) to the conversation, matching how a chat/pager is expected to behave.

## 0.5.0

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
