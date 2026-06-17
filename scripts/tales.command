#!/bin/bash
# Tales — double-click to open the terminal workspace.
cd "$(cd "$(dirname "$0")/.." && pwd)" || exit 1
clear
printf '\033[36m❯ tales\033[0m  — terminal workspace with Tales as the default pane\n\n'
# Prefer an installed `tales-tui`; fall back to the release build in this repo.
if command -v tales-tui >/dev/null 2>&1; then
  exec tales-tui
else
  exec ./target/release/tales-tui
fi
