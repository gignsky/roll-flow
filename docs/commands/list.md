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

The keymap, the `sync`/chevron columns and the output panel are shared with
`status` — see [`status`](status.md#keys).
