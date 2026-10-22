# `list`

```text
rf list [--no-tui] [--deps] [--json]
```

Lists roll branches and states, with the same `--no-tui`/`--json` options as
[`status`](status.md). `--deps` adds the `deps`/`dependants` columns, which `list`
leaves off by default.

The `version` column appears here too, on the same terms: shown whenever the repo
has a `Cargo.toml`, absent otherwise, and never flagged. `--json` adds the same
value as a `version` field per roll, `null` where there is none.

The `build` column — this machine's most recent local rebuild — appears here
too, on the same terms as `status`; see
[`status`'s description of it](status.md#build-column). `--json` adds a
`recent_rebuilds` array per roll (e.g. `["nixos:test"]`, empty on every roll
when nothing rebuilt here points at a roll specifically — the common case of
a plain switch directly on `rolling`/`main` is only visible via `rf status
--json`'s `local_rebuild`, since `list`'s roll array has no row for a branch
that isn't a roll).

The keymap, the `sync`/chevron columns and the output panel are shared with
`status` — see [`status`](status.md#keys).
