# `verify`

```text
rf verify [--dry-run] [--bump <patch|minor|major>] [--yes]
```

Checks graduation/promotion readiness for the current branch:

- `roll/* -> rolling`
- `rolling -> main`

Validation includes:

- clean tree
- non-detached HEAD
- mergeability (common history, something new to merge; divergence is fine and
  only produces an informational note)
- merge conflicts: when the branches have diverged, the graduation/promotion
  merge is tried in git's object store (`git merge-tree`, git 2.38+) without
  touching the working tree. A conflict fails `verify` before any gate runs,
  naming each conflicted path and the roll (or direct commit) on the target that
  changed it, and printing the fix — `rf integrate <culprit>` from a roll branch,
  as [`graduate`](graduate.md#when-the-merge-conflicts) recommends, or merging the
  target into the source. Under `--dry-run` it is a warning instead.
- the version gate on the `rolling -> main` route (see
  [Versioning and release tags](../../README.md#versioning-and-release-tags)).
  When a bump is needed, `verify` offers one; `--bump <level>` applies it without
  asking and `--yes` accepts the default (patch) non-interactively
- configured gate command execution

## Example

```text
$ rf verify
merging 'roll/3-0612-second' into 'rolling' would conflict in 1 file

  shared.txt
    roll/1-0611-first  (548fd6b: Graduate roll/1-0611-first into rolling)

The conflicting change is already on 'rolling' — it came in with roll/1-0611-first.
From 'roll/3-0612-second', take the conflict onto it, resolve, commit, then re-run rf verify:
  rf integrate roll/1-0611-first
Or merge 'rolling' itself instead:
  git checkout roll/3-0612-second && git merge rolling
Error: verification failed: 'roll/3-0612-second' does not merge cleanly into 'rolling'
```
