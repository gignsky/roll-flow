# Two `rf` binaries, and which one you are running

`nix develop` (or direnv) puts an `rf` on `PATH`, but it is the **built flake
package** — `self.packages.<system>.default` in `flake.nix` — not your working
tree. It will happily ignore every edit you have just made.

So there are two binaries, with two jobs:

- **`rf ...`** — the packaged build. Use it to *drive the workflow*: `rf create`,
  `rf verify`, `rf graduate`. This is the bootstrap: a stable `rf` is what moves
  your changes to `rf` through the pipeline.
- **`cargo run -- ...`** — your working tree. Use it to *exercise your changes*:
  `cargo run -- status`, `cargo run -- list --no-tui`. `cargo build` also leaves
  a binary at `target/debug/rf` if you would rather invoke it directly.

The dev shell prints this distinction on entry; run `rf-dev` to reprint the
banner once it has scrolled away.
