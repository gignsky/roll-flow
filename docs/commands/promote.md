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

A `--roll` that depends on rolls which have graduated but not yet promoted
promotes those first, each as its own step, and says so before asking:

```text
  roll/7-0918-verify-button  (dependency of 8, ✓ graduated)
(dependencies added ahead of what was named)

Promote these in order? [y/N]
```

`--yes` confirms; unattended, the plan is printed and nothing is merged. Since a
per-roll promotion advances stable to a graduation commit that already carries
everything graduated before it, the dependency step often reports
`already contained` once the first merge lands — that is correct, and the point
of ordering them. A dependency that has not graduated is refused outright: there
is nothing on rolling to advance stable to. `[m]` on a roll row in the TUI
performs the same expansion and lists it in its confirm modal.

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
