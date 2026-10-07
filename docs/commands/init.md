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
  [below](#the-version-merge-driver)) and reports when it had to. Runs on every
  `rf init`, independent of whether `.roll-flow.toml` itself needed updating.

## The version merge driver

Every roll wears its own `-roll<N>` marker (see
[`create`](create.md#dev-versions)) in `Cargo.toml` *and* in the crate's own
`[[package]]` entry in `Cargo.lock`, so any merge between two dev-marked
branches — [`integrate`](integrate.md), `[i]`/`[I]`, [`update`](update.md),
[`graduate`](graduate.md#dev-versions) — touches the same version line on both
sides of both files. The driver resolves those lines by one rule: **keep the
checked-out side's marker, take the higher of the two `X.Y.Z` numbers.** Only
the crate's own lockfile entry is touched; a dependency that moved differently
on each side is still a conflict.

Wiring it is two clone-local settings, neither of which is ever in the tree:

- `Cargo.toml merge=rf-version` and `Cargo.lock merge=rf-version` in the
  clone's `info/attributes` (`git rev-parse --git-path info/attributes`), so it
  applies on every branch whether or not that branch carries a `.gitattributes`.
  A `.gitattributes` line written by an older `rf init` names the same driver
  and keeps working.
- `git config --local merge.rf-version.driver` pointing at
  `rf __merge-driver-version %O %A %B %P` — what a driver name runs is never
  stored in a repo, so every clone needs this.

`rf init` is not the only thing that sets them: every merge `rf` makes checks
them first and wires them silently if they are missing, so a clone that never
ran `rf init` is covered too. And if git stops on a merge anyway with *only*
those version lines in conflict — `rf` not on the `PATH` git sees, for
instance, so the driver could not run — `rf` resolves them by the same rule and
completes the merge. If anything else conflicts, the merge is left exactly as
git left it.

See [Config](../config.md) for the file it writes.
