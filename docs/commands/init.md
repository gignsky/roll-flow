# `init`

```text
rf init [--rolling-branch <name>] [--stable-branch <name>] [--roll-prefix <prefix>] [--username <user>] [--hosts <h1,h2>] [--mode <manage|assist>] [--force] [--yes]
```

- Writes `.roll-flow.toml` at repository root
- Detects branch defaults from repo (`rolling`/`develop`/`integration`, and `main`/`master`)
- Ensures the rolling branch exists (creates it from stable branch when absent)
- `--mode` selects `manage` (rf drives the workflow) or `assist` (a human drives;
  rf reports and derives state). Preserved across re-init when omitted
- `--force` overwrites the config even when it already matches, skipping the diff
  prompt; `--yes` applies detected changes without prompting, for non-interactive use
- In a repo with a `Cargo.toml`, wires up the version merge driver (see
  [`graduate`'s "Dev versions" section](graduate.md#dev-versions)): adds
  `Cargo.toml merge=rf-version` to `.gitattributes` if it isn't there already,
  and sets `git config --local merge.rf-version.driver`. Runs on every
  `rf init`, independent of whether `.roll-flow.toml` itself needed updating —
  the git config half is local to this clone and isn't stored in the repo, so
  a fresh clone needs this even when the config file is already up to date.

See [Config](../config.md) for the file it writes.
