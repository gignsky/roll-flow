# `list`

```text
rf list [--no-tui] [--deps] [--json]
```

Lists roll branches and states, with the same `--no-tui`/`--json` options as
[`status`](status.md). `--deps` adds the `deps`/`dependants` columns, which `list`
leaves off by default.

The `⟳` outdated marker on the state cell, and the `outdated` array in `--json`,
work as in [`status`](status.md).

The keymap, the `sync`/chevron columns and the output panel are shared with
`status` — see [`status`](status.md#keys).
