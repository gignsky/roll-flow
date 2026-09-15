# Documentation is a gate

`tests/docs_sync.rs` asks the freshly built binary for its own CLI surface
(`rf --help`, then `rf <sub> --help`) and asserts that the documentation covers
every subcommand and every long flag. Each subcommand needs:

- a line in the `## Commands` block in `README.md` listing all of its long flags, and
- its own file at `docs/commands/<sub>.md`, headed `` # `<sub>` ``.

The per-command files are split out deliberately. When every command's prose
lived in one `README.md`, each new command appended a `###` section at the same
place, so two rolls adding commands conflicted every time — and twice the
conflict was resolved by keeping both copies. One file per command means a new
command is a new file, which cannot conflict at all.

It is an ordinary test on purpose. `cargo test --locked` is already both a CI
step and a configured gate in `.roll-flow.toml`, so documentation drift fails
`rf verify`, `rf graduate`, and `rf promote` locally, and the PR check remotely,
with no separate workflow to keep in sync. Two carve-outs are allowlisted in the
test: clap's generated `help` subcommand, and the universal `--help`/`--version`.
