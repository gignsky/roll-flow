# `update`

```text
rf update [--roll <branch>]... [--dry-run]
```

Merges the stable branch into every active local roll branch, bringing them all
up to the current baseline in one pass. Supports `--dry-run`.

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
