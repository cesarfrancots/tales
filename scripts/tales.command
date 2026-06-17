#!/bin/bash
# Tales — double-click to launch the terminal supervisor (Claude Code + Codex).
cd "$(cd "$(dirname "$0")/.." && pwd)" || exit 1
clear
printf '\033[36m❯ tales\033[0m  — Claude Code + Codex, live in your terminal\n\n'
read -r -p "Task: " task
[ -z "$task" ] && task="Plan and improve this project"
exec ./target/release/tales-tui "$task"
