# `list`

```text
rf list [--no-tui] [--deps] [--json]
```

Lists roll branches and states, with the same `--no-tui`/`--json` options as
[`status`](status.md). `--deps` adds the `deps`/`dependants` columns, which `list`
leaves off by default.

The `version` column appears here too, on the same terms: shown whenever the repo
versions through a manifest, one per manifest when there are several, absent
otherwise, and never flagged. `--json` adds a `version` field per roll — the
first manifest's, the one that names the release tag, `null` where there is
none — and a `versions` object holding every manifest's, keyed by path
(`{"package.json": "0.2.8-roll3", "keeper/Cargo.toml": "0.1.2-roll3"}`).

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
