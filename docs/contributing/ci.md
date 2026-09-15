# What CI checks

Every PR is gated by the `build · test · fmt · clippy` job defined in
`.github/workflows/ci.yml`:

- `cargo update --workspace --locked` (Cargo.lock is up to date)
- `cargo fmt --all --check`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo build --locked --verbose`
- `cargo test --locked --verbose`

These are the same checks configured as this repo's roll-flow gates in
`.roll-flow.toml` (`roll_to_rolling_gates` / `rolling_to_main_gates`), so
`rf verify`/`rf graduate`/`rf promote` fail locally before CI would fail
remotely. Note that `cargo test` carries `tests/docs_sync.rs`, so the
documentation check rides along with them — see
[Documentation is a gate](docs-gate.md).

A second required check, `.github/workflows/version-bump-check.yml`, enforces the
version bump described in [Releases and version bumps](releases.md).
