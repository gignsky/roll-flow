# `hotfix`

```text
rf hotfix [<slug>] [--date MMDD] [--land] [--dry-run]
```

Creates `hotfix/N-MMDD-slug` off the stable branch, for urgent fixes that cannot
wait for the next promotion. `--land`, run from the hotfix branch, merges it into
stable with `--no-ff` and then reintegrates stable into rolling so the two do not
drift. `--date` and `--dry-run` behave as they do for [`create`](create.md).
