Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

cargo fmt --check
cargo test --workspace
cargo build --release --workspace
