# `promote`

```text
rf promote [--roll <branch>]... [--dry-run] [--force --reason <text>] [--bump <patch|minor|major>] [--no-tag] [--yes]
```

Merges rolling into the stable branch with `--no-ff` and a structured subject
(`Promote roll/N-slug to main`, or `Promote rolling to main` with the included
rolls listed in the body when several graduated rolls ride along). Run from a
roll branch it redirects to graduation. Conflicts abort and restore, same as
[`graduate`](graduate.md), and `--force`/`--reason` behave the same way.

The gates run against the *staged merge result* rather than whatever was checked
out, so what they check is what lands on stable. A gate that modifies tracked
files mid-merge aborts the promotion rather than committing content the gates
never saw.

`--roll <branch>` promotes one graduated roll instead of the whole branch, by
advancing stable to that roll's graduation commit on rolling. Stable therefore
stays a prefix of rolling, and `main` still only ever receives merges from
`rolling` — a roll branch is never merged into stable directly. The flag is
repeatable, works from any branch, and orders the rolls it is given by
graduation, so promoting a roll necessarily carries whatever graduated ahead of
it; a roll already contained in stable is reported as skipped rather than
failing.

Each `--roll` is its own merge behind its own gate run, so a two-roll promotion
runs the gates twice and verifies both intermediate states of stable. Promoting
the whole branch is a single merge, so one gate run covers it. If a later step's
gates fail, the merge is aborted and the earlier steps stay committed.

Promotion also owns the release mechanics that previously only happened when you
opened a PR — see
[Versioning and release tags](../../README.md#versioning-and-release-tags):

- refuses to promote unless `Cargo.toml`'s version is above the stable branch's,
  offering to bump it (`--bump <patch|minor|major>` to skip the prompt, `--yes`
  to take the patch default non-interactively). A `--roll` promotion merges a
  commit that already exists on rolling, so it reports a short version rather
  than offering a bump there is nowhere to put
- creates an annotated `vX.Y.Z` tag on each promotion merge commit, then offers to
  push it to `origin`. `--no-tag` skips tagging entirely
- `--force --reason "<why>"` overrides the version gate, recording it in the
  merge commit alongside any other bypassed gate
