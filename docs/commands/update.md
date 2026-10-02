# `update`

```text
rf update [--roll <branch>]... [--dry-run]
```

Merges the stable branch into every active local roll branch, bringing them all
up to the current baseline in one pass. Pass one or more `--roll <branch>` to
update only those branches instead of all of them — a named branch must be an
active, local roll, or the command errors rather than silently skipping it.
Supports `--dry-run`.

The TUI's `[u]` mirrors this: with a roll row selected, it updates just that
roll; with nothing (or a base-branch row) selected, it updates every active
local roll, as before.
