# `update`

```text
rf update [--roll <branch>]... [--dry-run] [--state <all|active|blocked>]
```

Merges the stable branch into every active local roll branch, bringing them all
up to the current baseline in one pass. Supports `--dry-run`.

## Updating a named subset

`--roll <branch>` (repeatable) narrows the pass to specific branches. `--state`
narrows it to a lifecycle category instead — `active`, `blocked`, or `all`
(the default, equivalent to the bare `rf update`) — and is mutually exclusive
with `--roll`. Unlike [`rf verify --all --state`](verify.md#verifying-many-at-once),
there is no separate `--all` flag to opt into the bulk pass first: `rf update`
with no `--roll` is already the bulk pass, so `--state` just picks which rolls
it covers.

The TUI's `[U]` is the same menu: it opens a picker listing each state with how
many rolls it currently covers, and a digit runs the merge over that set
immediately — the picker's count is the confirmation, the same way `[b]`'s
bump picker needs no second "are you sure?" once a level is chosen. `[u]` with
nothing selected remains the plain "every active local roll" pass; `[U]` is
for reaching for a narrower slice without leaving the keyboard.

## Dev versions

A roll carrying a `-roll<N>` dev marker (see [`create`](create.md#dev-versions))
merges stable in like any other branch — nothing strips or reapplies the
marker separately. If stable's own version has moved since the roll branched,
the [version merge driver](init.md#the-version-merge-driver) resolves the
line, in `Cargo.toml` and in `Cargo.lock`'s own entry alike: the roll keeps
its own `-roll<N>` marker, and the base number becomes whichever side is
higher — in practice stable's, since that's the point of updating. Without it
this would be a real conflict (both the roll's marker and stable's bump touch
the same line); with it, the merge just succeeds. `rf update` wires the driver
itself if this clone never ran `rf init`, and finishes the merge by the same
rule if git stopped on nothing but those lines.
