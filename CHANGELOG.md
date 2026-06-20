# Changelog

All notable Tales release changes are tracked here.

Tales uses one lockstep SemVer version for the Rust workspace. The version lives in the root `Cargo.toml`.

## Unreleased

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
