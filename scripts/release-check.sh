#!/usr/bin/env bash
set -euo pipefail

cargo fmt --check
cargo test --workspace
cargo build --release --workspace
