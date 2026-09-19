# `hotfix`

```text
rf hotfix [<slug>] [--date MMDD] [--land] [--dry-run]
```

Creates `hotfix/N-MMDD-slug` off the stable branch, for urgent fixes that cannot
wait for the next promotion. `--land`, run from the hotfix branch, merges it into
stable with `--no-ff` and then reintegrates stable into rolling so the two do not
drift. `--date` and `--dry-run` behave as they do for [`create`](create.md).

Hotfix branches show up in [`status`](status.md) and [`list`](list.md) beneath
the rolls, numbered `h<N>`, and read `✓ landed` once their `Hotfix hotfix/N-slug
into <stable>` merge is on the stable branch. A hotfix merged by hand with a
plain `Merge branch 'hotfix/…'` counts too — landing is read from the merge
subject's *source*, never its target, so the reintegration merge that follows a
landing (`Reintegrate <stable> into <rolling> …`) is never mistaken for one.
