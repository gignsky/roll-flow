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

The `## Commands` code block itself is the one remaining shared spot: every
flag for every subcommand lives on one line each, in one fenced block, so two
rolls adding a flag to two *different* commands still land on adjacent lines —
and adjacent lines changed by different sides is exactly the case git's merge
cannot split, even though neither side's edit actually overlaps the other's.
`roll/13` adding flags to `verify` and `roll/16` adding a flag to `graduate`
hit this directly. There's no file-per-line fix available here the way there
was for command prose, so the mitigation is social: expect a trivial conflict
on this block whenever two rolls both touch it, and resolve it by keeping both
sides' flags — `tests/docs_sync.rs` will fail loudly if a flag gets dropped in
the process.

The `## Caveats` bullets have the same shape for a different reason: they used
to be wrapped prose, where inserting one TUI key mid-sentence reflowed several
lines and collided with any other roll adding a different key in the same
release window. They are now one list item per key, so two rolls adding
different keys add different lines and merge without a conflict — the same
"a new entry is a new line" principle as the per-command split above, applied
inside a single file instead of across files.
