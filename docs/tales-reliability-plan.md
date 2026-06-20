# Tales Reliability and Safety Plan

Generated: 2026-06-20

## Goal

Make Tales dependable for real repository work where agents may time out, tools may stall, and project-local configuration may contain secrets.

## P0 - This Implementation Pass

- Persist every interactive run immediately under `.tales/runs/<run-id>/`.
- Keep a rolling `plan.md`, `manifest.json`, and `events.jsonl` so a timeout never loses the plan or transcript.
- Keep writing `.tales/last-plan.md` when an executor is selected, and copy that executor plan into the run artifact directory.
- Persist non-interactive `tales run` and `tales discuss` sessions under the same artifact layout.
- Add `tales recover` to list saved runs and print the newest or selected saved plan.
- Add `/artifacts`, `/handoff [executor|number]`, and `/switch <executor|number>` recovery commands in the terminal.
- Detect project-local MCP config files before planning starts.
- Warn clearly when MCP config files may load secrets.
- Launch Claude with project MCP loading disabled when local MCP config files are present.
- Improve startup/help/command copy so users know about artifacts, recovery, and executor handoff.

## P1 - Next Pass

- Extend `tales recover` from inspection into true resume/reopen flows.
- Add explicit retry/switch-executor actions after agent failures, including failed process-pane detection.
- Add GitHub auth preflight checks for PR workflows, including missing `workflow` scope when a plan touches `.github/workflows`.
- Add structured status events for elapsed time, phase, active agent, artifact paths, and current executor pane.
- Add a workspace policy preflight that reads `.tales/policy.toml` before any model process starts. The first version should support explicit MCP mode (`block`, `warn`, `allow`), allowed config paths, default executor preferences, and artifact retention days.
- Add a redaction boundary for logs and artifacts before they are written to disk. Start with env-style keys, known API key prefixes, bearer tokens, and MCP config environment blocks.

## P2 - Hardening

- Add configurable secret scanners before model/CLI launch. The scanner should load built-in rules plus workspace policy additions, run against known MCP/tool config files and selected plan context, and report only labels/path metadata by default.
- Expand `.tales/policy.toml` with per-executor MCP settings, denylisted config paths, custom redaction patterns, retention max size, and an emergency override that requires an explicit CLI flag.
- Add artifact retention cleanup and redaction rules. Cleanup should be opt-in at first, never delete the active run, and keep a manifest entry describing what was removed.
- Add terminal replay from `events.jsonl` for debugging and demos.

## Safety Gaps Found

- MCP detection is currently hard-coded to known config paths and marker substrings. It does not yet support workspace-specific scanner rules or explicit user policy.
- Claude safe-launch is conservative when a known project-local config exists, but equivalent safe-launch handling has not been defined for every MCP-capable adapter.
- Artifact persistence is useful for recovery, but the write path needs a centralized redaction layer before storing prompts, executor output, tool calls, and errors.
- Run artifacts do not yet have retention enforcement, max-size controls, or cleanup manifests, so long-lived workspaces can accumulate sensitive history.
