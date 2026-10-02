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

## Dev versions

Before anything else — including the merge-state checks, let alone the gates —
`rf graduate` confirms a dev marker the roll carries is actually its own: a
roll's `Cargo.toml` should only ever read `-roll<N>` for its own number (what
[`create`](create.md#dev-versions) and `rf verify` write). One that names a
different roll means the version history got mixed with another roll's
somewhere upstream (an `[i]` integrate merge, a stray cherry-pick), and
graduation refuses rather than silently stripping evidence of that. This check
is read-only and runs first because it is nearly free next to a full gate run.

If the roll's `Cargo.toml` still carries its own `-roll<N>` dev marker, it is
stripped in a commit on the roll branch **before** the gates run — the same
sequencing the version bump uses, and for the same reason: it rewrites
`Cargo.lock`, and `roll_to_rolling_gates` contains
`cargo update --workspace --locked`, which fails against a stale one. This
happens for every path that graduates a roll — `rf graduate` itself, the
`rf promote` fall-through when run from a roll branch, and the TUI's `[g]` —
since all three call the same `ops::graduate`. It also has to happen before the
merge is attempted, not just the gates: if rolling has moved since the roll
branched (another roll's version bump, say), the roll's own `-roll<N>` line and
rolling's new one land on the same spot in `Cargo.toml`, and a roll that still
carries its marker conflicts with that — stripping first makes the roll's line
match the merge base, so the merge takes rolling's version instead of
colliding with it.

That strip commit is rolled back if anything after it fails — a gate failure,
or a real merge conflict on some other file. Without that, a failed graduation
would leave a half-done result behind: a roll whose version marker is gone but
that never actually graduated, which reads as "the version got reverted"
rather than "graduation failed," since nothing else about the failure is
visible on the roll branch itself. A failed `rf graduate` now leaves the roll
exactly as it was before the attempt.

`--dry-run` leaves the marker alone, since a preview must not commit. So a dry
run reports what the gates say about the *unstripped* version, which is the
honest answer for a run that changes nothing.
