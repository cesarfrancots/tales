# Changelog

All notable Tales release changes are tracked here.

Tales uses one lockstep SemVer version for the Rust workspace. The version lives in the root `Cargo.toml`.

## Unreleased

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
