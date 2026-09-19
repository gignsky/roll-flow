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

If the roll's `Cargo.toml` still carries a `-roll<N>` dev marker (see
[`create`](create.md#dev-versions)), it is stripped in a commit on the roll
branch **before** the gates run — the same sequencing the version bump uses, and
for the same reason: it rewrites `Cargo.lock`, and `roll_to_rolling_gates`
contains `cargo update --workspace --locked`, which fails against a stale one.

`--dry-run` leaves the marker alone, since a preview must not commit. So a dry
run reports what the gates say about the *unstripped* version, which is the
honest answer for a run that changes nothing.
