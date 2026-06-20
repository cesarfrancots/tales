# Agent Instructions

## Versioning

Tales uses one lockstep product version for the Rust workspace.

- The single source of truth is `[workspace.package].version` in the root `Cargo.toml`.
- Every shipped Rust crate must inherit it with `version.workspace = true`.
- Do not add per-crate package versions unless the project intentionally moves away from lockstep releases.
- `schema_version` fields are data/protocol contract versions. Do not bump them for product releases.
- Add product/build metadata beside JSON schema fields when needed; use `tales_core::build_info::json()`.
- All shipped binaries should expose the workspace version through Clap `--version`.

Pre-1.0 bump rules:

- Patch, for example `0.1.1`: bug fixes, docs fixes that affect released behavior, packaging fixes, or small internal corrections.
- Minor, for example `0.2.0`: user-visible features, CLI behavior changes, report/session JSON additions, or breaking pre-1.0 changes.
- Major, `1.0.0`: only when the CLI flags, report schemas, workspace/session formats, and release process are stable enough to support.

Before tagging a release:

1. Update the root `Cargo.toml` workspace version.
2. Update `CHANGELOG.md`.
3. Run `scripts/release-check.sh` on macOS/Linux or `.\scripts\release-check.ps1` on Windows.
4. Verify `tales --version`, `tales-tui --version`, and `tales-web --version`.
5. Commit the version/changelog changes and tag the commit as `vX.Y.Z`.

Use git tags in the form `v0.1.0`. Do not create unprefixed release tags.
