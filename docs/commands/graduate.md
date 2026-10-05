# `graduate`

```text
rf graduate [--dry-run] [--force --reason <text>]
```

Merges the current roll branch into rolling with `--no-ff` and a structured
subject (`Graduate roll/N-slug into rolling`), then returns to the roll branch.
Divergence between the roll and rolling is handled by the merge; a conflicting
merge is aborted and the original branch restored, leaving the repo clean.

`--force` proceeds past failing gates and requires `--reason <text>`, which is
recorded as a `Force-Reason:` trailer in the merge commit so the bypass stays
auditable in git history.

## Which branch the gates see

The TUI's `[G]` can graduate any eligible roll row regardless of which branch
is actually checked out — selecting a row doesn't check it out first. So
before the gates run, `rf graduate` checks the named roll out itself (rather
than running the configured gates against whatever happened to be checked out
already), stages the merge, and runs the gates against the staged result —
the content that will actually land on rolling, not the roll in isolation and
not whatever branch the TUI started from. Whichever branch was checked out
before the call is restored afterward, success or failure; for the CLI, which
already requires being on the roll, this is a no-op.

## Dev versions

Before anything else — including the merge-state checks, let alone the gates —
`rf graduate` confirms a dev marker the roll carries is actually its own: a
roll's `Cargo.toml` should only ever read `-roll<N>` for its own number (what
[`create`](create.md#dev-versions) and `rf verify` write). One that names a
different roll means the version history got mixed with another roll's
somewhere upstream (an `[i]` integrate merge, a stray cherry-pick), and
graduation refuses rather than silently papering over evidence of that. This
check is read-only, independent of the rest of this section, and runs first
because it is nearly free next to a full gate run.

Merging a roll that still carries its `-roll<N>` marker into rolling is
otherwise handled entirely at the git level, by a merge driver scoped to
`Cargo.toml`'s `version` line — not by `rf` stripping or committing anything
on the roll branch first. `rf init` wires this up (see [`init`](init.md)): it
adds `Cargo.toml merge=rf-version` to `.gitattributes` and points
`git config merge.rf-version.driver` at `rf` itself (the `__merge-driver-version`
subcommand). The rule it applies, whenever a merge needs to reconcile the
line: **keep whichever side is being merged *into* (`rolling`, which never
carries a marker) — its own value if it has one, otherwise the higher of the
two numbers.** For graduation that means rolling's own version always wins,
untouched by whatever the roll's `Cargo.toml` said; the roll's marker simply
never reaches rolling. The same driver, same rule, is what [`rf update`](update.md)
relies on to keep a roll's marker while raising its base number — "ours" is
whichever branch is checked out, so the one driver and rule cover both
directions.

A git merge driver only runs when both sides actually changed the line —
that's when a plain text merge would otherwise conflict, which is exactly the
case this exists for: rolling has moved since the roll branched (another
roll's version bump, say), so the roll's `-roll<N>` line and rolling's new one
land on the same spot and a plain merge would reject it. The far more common
case is the opposite — rolling's own version hasn't moved at all, so only the
roll changed the line — and git resolves *that* trivially by taking the
changed side, with no driver involved. `rf graduate` corrects this case itself,
inside the staged merge, before the gates run: it reads both sides' version
ahead of the merge and, after staging, rewrites `Cargo.toml` (and refreshes
`Cargo.lock`) to the same rule's answer if the merge didn't already land there
on its own. Either way — driver-resolved or corrected in the staged tree —
nothing is committed to the roll branch itself, and nothing needs rolling
back: the merge either succeeds as one commit with the right version already
in it, or it's aborted entirely and the roll is untouched, exactly as any
other failed graduation.

`--dry-run` doesn't stage a merge at all, so the gates report on the roll's
branch exactly as it is — the honest answer for a run that changes nothing.
