# Proposal: version `package.json` the way `rf` versions `Cargo.toml`

Status: implemented, on `roll/41-1016-add-package-dot-json-versions`. File
and function names below are from the proposal as originally written against
`develop` at `20783d7`; the implementation deviates in a few places, listed
here rather than edited into the body below so the body still reads as the
design that was reviewed:

- Every `version` free function that reads or compares (`read_version`,
  `check`, `check_against`, `versions_at`, `version_at`) takes a
  `forced: Option<ManifestKind>` parameter, not just the write paths. A first
  cut that stopped at the writes left the gate's own comparison
  auto-detecting regardless of `version_manifest`, which could bump one
  manifest while gating against the other.
- `write_version` takes an explicit `ManifestKind` rather than detecting one
  itself, since its callers already resolved one (often via the override)
  and must write to exactly that file, not whatever plain detection would
  pick.
- `ManifestKind::detect`/`detect_at` (and their `forced`-aware
  `detect_forced`/`detect_at_forced` twins) require the candidate manifest to
  carry a *readable* version, not just exist. A `package.json` with no
  `version` key — the ordinary starting point for most Node apps, and for
  tooling-only manifests (prettier, husky) — reads the same as no manifest at
  all, never as `Unreadable`.
- The version-gate and dev-marker-ownership error messages no longer name a
  specific file (`"version (...) is unchanged from ..."` rather than
  `"Cargo.toml version (...) ..."`), since one message now serves two kinds.
  `write_version`'s own "couldn't find the key" error still names the kind's
  manifest, since that error is inherently about one specific file.
- The merge-driver subprocess (`__merge-driver-version`) and
  `ops::wire_driver_quietly`'s own callers have no `Config` and so are not
  `version_manifest`-aware; only `ensure_version_merge_driver` (`rf init`) is.
  This is harmless: attribute lines only accumulate, and a merge is resolved
  by the file actually being merged, not by which kind was wired first.

## Why

Every version feature in `rf` is keyed to one hard-coded file pair:
`Cargo.toml` and `Cargo.lock`. A Node repo managed by roll-flow therefore has
to turn `version_gate` off and reimplement the gate as a shell script in its
gate lists (christina-website does exactly this today with
`scripts/check-version.sh`, plus copies of the three workflows). That script
only covers the bump comparison; it gets none of the dev markers, the merge
driver, the in-merge bump, the TUI column, or the release tag.

The policy itself has nothing Cargo-specific in it. `package.json` carries a
semver `version` at its root and `package-lock.json` repeats it twice (root
`version`, and `packages[""].version` in lockfile v2/v3), which is the same
shape as `Cargo.toml` plus the crate's own `[[package]]` entry in
`Cargo.lock`. The markers `rf` writes, `X.Y.Z-roll<N>` and `X.Y.Z-dev`, are
valid npm prerelease versions, and npm orders them below the bare release
they mark, matching the one property `rf`'s own gates actually rely on
(`0.2.4-dev < 0.2.4`). It is not a full match: semver §11 compares
alphanumeric prerelease identifiers lexically, so npm itself would rank
`-dev` below `-roll9` — the reverse of `Semver`'s derived `Ord`, which ranks
`Roll(_) < Dev` by declaration order. This never surfaces: `rf`'s own `Ord`
is what every comparison here uses, and a dev marker is refused outright
before it ever reaches a tool (npm, `sort -V`) that would apply its own
ordering to it. So the whole pipeline ports without changing a rule.

## Shape of the change

One new enum, `ManifestKind`, becomes the single place that knows which files
a repo versions through and how to read and rewrite them. Everything that
currently names `Cargo.toml`, `Cargo.lock`, `version::VERSION_FILE` or
`version::LOCK_FILE` asks the kind instead. The Cargo arm is a pure move of
the code that exists today, so phase one is a refactor with no behaviour
change; the npm arm is phase two.

```rust
// src/core/version.rs

/// Which manifest a repo carries its version in. Declared in precedence
/// order: `detect` returns the first kind whose manifest exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestKind {
    Cargo,
    Npm,
}

impl ManifestKind {
    pub const ALL: [ManifestKind; 2] = [ManifestKind::Cargo, ManifestKind::Npm];

    pub fn manifest(self) -> &'static str   // "Cargo.toml" | "package.json"
    pub fn lockfile(self) -> &'static str   // "Cargo.lock" | "package-lock.json"
    /// Both names, manifest first: what every commit and `git add` lists.
    pub fn paths(self) -> [&'static str; 2]

    /// The kind this working tree versions through, or `None` for a repo
    /// that does not version this way (the dotfiles repo).
    pub fn detect(repo: &Path) -> Option<ManifestKind>
    /// The kind at a ref, by trying each manifest at that ref in order.
    pub fn detect_at(repo: &Path, refspec: &str) -> Result<Option<ManifestKind>, RfError>
    /// Which kind a repo-relative path belongs to, by file name, and whether
    /// it is the manifest or the lockfile. Replaces `merge_driver::VersionFile::for_path`.
    pub fn for_path(path: &Path) -> Option<(ManifestKind, Role)>

    pub fn parse_version(self, text: &str) -> Option<Semver>
    pub fn parse_name(self, text: &str) -> Option<String>
    pub fn replace_version(self, text: &str, new: Semver) -> Option<String>
    pub fn parse_lock_version(self, text: &str, name: Option<&str>) -> Option<Semver>
    pub fn replace_lock_version(self, text: &str, name: Option<&str>, new: Semver) -> Option<String>
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role { Manifest, Lockfile }
```

`Semver`, `Marker`, `BumpLevel`, `VersionStatus` and `VersionCheck` are
untouched. The free functions `read_version`, `versions_at`, `check`,
`check_against` and `write_version` keep their signatures and detect the kind
internally, so `main.rs`, `tui/rolls.rs` and most of `ops.rs` do not change at
their call sites. `parse_version`, `parse_package_name`,
`replace_package_version`, `parse_lock_package_version` and
`replace_lock_package_version` become the Cargo arm's private helpers behind
the methods above.

### Detection

`detect` looks at the working tree: `Cargo.toml` first, then `package.json`.
Cargo wins when both exist, because that keeps every existing repo byte-for-
byte on its current behaviour and because a Rust project with a `package.json`
for tooling is more common than the reverse. For the rare repo that wants the
other answer (a Tauri app that versions through `package.json`, say), add one
optional config key:

```toml
# .roll-flow.toml
version_manifest = "auto"   # "auto" (default) | "cargo" | "npm"
```

`Config` gains `version_manifest: VersionManifest` with `#[serde(default)]`,
and `detect` honours it. A forced kind whose manifest is missing reads as
`NotApplicable`, same as today's missing `Cargo.toml`.

Ref-based reads (`check`, `check_against`, `versions_at`,
`check_dev_marker_ownership`, `version_behind_stable`) cannot look at the
working tree for the branch they are reading, so they go through `detect_at`:
try `Cargo.toml` at the ref, then `package.json`. `versions_at` already does
one `git cat-file --batch` for the whole table; request both specs per ref and
take the first that parses. `check`'s "no manifest on either side" case means
no manifest of *any* kind on either side.

### Reading and rewriting `package.json`

`package.json` is hand-edited, so its formatting is arbitrary: two-space,
four-space, tabs, keys in any order, sometimes no trailing newline. A
`serde_json` round-trip would reformat the file, which is the same reason
`replace_package_version` is line-based rather than a `toml` serialize. The
Cargo arm's trick, "find the `[package]` table then the first `version` line",
has a JSON equivalent: find the string value at a key path, respecting nesting
and string escapes.

```rust
/// Byte range of the string value at `path` (e.g. `["version"]`, or
/// `["packages", "", "version"]` for a lockfile), excluding the quotes.
/// `None` when the path is absent or the value is not a string.
fn json_string_at(text: &str, path: &[&str]) -> Option<Range<usize>>
```

A small scanner, no allocation beyond the key stack: walk the bytes tracking
whether we are inside a string (honouring `\"`), the current `{`/`[` depth, and
for each object level the most recent key seen at that level. When the key
stack equals `path` and the next non-whitespace after `:` is a `"`, return the
range of that string. Arrays push a placeholder so a `"version"` inside
`"workspaces": [...]` or any nested object never matches the root path.

Then:

- `parse_version` for npm = validate with `serde_json::from_str::<Value>`
  (so malformed JSON reads as `Unreadable`, not as whatever the scanner
  happens to find), then `Semver::parse(&text[json_string_at(text,
  &["version"])?])`. Validation through `serde_json` already in
  `[dependencies]`, nothing new to add.
- `replace_version` for npm = splice `new` into that range. Every other byte,
  including a missing trailing newline, is preserved. `None` when the key is
  absent, which `write_version` turns into the same "could not find a
  `version` key" error it raises for `Cargo.toml`, with the file name taken
  from the kind.
- `parse_name` for npm = the string at `["name"]`. The npm lockfile does not
  need it (see below), but `merge_driver::package_name` and the error
  messages do.

Unit tests to add next to the existing `reads_package_version_not_dependency_version`
and `rewrite_*` tests: root `version` found when a nested object (`"engines"`,
a made-up `"publishConfig": {"version": "9.9.9"}`) also has one; a value
containing an escaped quote elsewhere in the file does not derail the scanner;
four-space and tab indentation survive a rewrite unchanged; a file with no
trailing newline stays without one; a `package.json` with no `version`
(private packages sometimes omit it) returns `None`.

### The lockfile

`package-lock.json` is written by npm as `JSON.stringify(lock, null, 2) +
"\n"`, so it is regular, but the same scanner handles it and keeps the
"touch nothing else" guarantee the Cargo arm makes:

- `parse_lock_version(text, _name)` = the string at `["version"]`. The root
  entry exists in every lockfile version (1, 2 and 3).
- `replace_lock_version(text, _name, new)` = splice at `["version"]`, then,
  if `["packages", "", "version"]` resolves (lockfile v2/v3), splice there
  too. The two splices are done back to front so the first range stays valid.
  Returns the text unchanged, not `None`, when the file has a root version
  but no `packages[""]` entry (v1).

The `name` parameter is ignored by the npm arm. It stays on the signature
because the Cargo arm needs it to find the crate's own `[[package]]` entry,
and `merge_driver` passes it uniformly. `npm-shrinkwrap.json` is out of scope;
a repo that ships one instead of `package-lock.json` gets manifest-only
versioning, exactly as a crate without a committed `Cargo.lock` does today.

### Refreshing the lockfile

`ops::refresh_lockfile` does two things for Cargo: rewrite the crate's own
entry directly (`sync_lockfile_own_entry`), then best-effort `cargo update
--workspace --offline`, falling back to online. The direct rewrite is what
actually matters; cargo is only there to pick up anything else a manifest
change implies.

For npm, a root version change implies nothing else in the lockfile: the two
fields above are the only ones that move. So the npm arm does the direct
rewrite and stops. No `npm` process, no network, no toolchain requirement,
which also keeps the integration tests offline. If a later need appears,
`npm install --package-lock-only --ignore-scripts --no-audit --no-fund` is
the equivalent of the cargo call, but I would not add it now.

`refresh_lockfile` and `sync_lockfile_own_entry` become kind-dispatching:
`let Some(kind) = ManifestKind::detect(repo) else { return }`, read
`kind.lockfile()`, rewrite with `kind.replace_lock_version`, and run the
toolchain refresh only for `Cargo`.

### Merge driver

`merge_driver::VersionFile` is already the right abstraction; it just only
knows two files. Replace it with `(ManifestKind, Role)` via
`ManifestKind::for_path`, and make `VersionFile::ALL` the four names. The
rule in `resolve` and the three-way logic in `merge_texts` are untouched:
`read` and `rewrite` dispatch to the kind's methods. `package_name` returns
`None` for an npm lockfile (it is only consulted when `Role::Lockfile` and
the Cargo arm asks for it).

`ops::wire_version_merge_driver` currently returns early when `Cargo.toml` is
missing and writes attribute lines for `VersionFile::ALL`. Change it to
return early when `detect` is `None`, and write lines only for the detected
kind's `paths()`. The driver command string
`rf __merge-driver-version %O %A %B %P` does not change; `%P` is what tells
`run` which of the four files it has. The `rf init` message in `main.rs`
("Configured the version merge driver for Cargo.toml and Cargo.lock") and the
hidden subcommand's doc comment in `cli/mod.rs` should name the detected
files.

`ops::settle_version_only_conflicts` uses `VersionFile::for_path` on each
unmerged path and so follows for free.

### Every other literal

`grep -n 'Cargo\|VERSION_FILE\|LOCK_FILE' src/**/*.rs` is the checklist. From
`develop` today, the sites that need `kind.paths()` or `kind.manifest()`
instead of a literal:

- `ops::reconcile_staged_version` (graduate's staged rewrite and `git add`)
- `ops::commit_version_change` (`git::commit_paths(repo, &["Cargo.toml",
  "Cargo.lock"], ...)`; `commit_paths` already skips missing files, so
  `kind.paths()` drops in)
- the per-roll promotion step's in-merge bump in `run_promote_step`
  (`write_version`, `refresh_lockfile`, `git add` of both files)
- `ops::check_dev_marker_ownership` (reads the manifest at a branch; its error
  text says "Cargo.toml version")
- `ops::version_behind_stable`
- `ops::apply_version_bump`'s "no readable version in Cargo.toml" error
- `version::write_version`'s "could not find a `version` key in the
  [package] table" error (wording per kind: "in the [package] table" vs "at
  the root")

The TUI's `[b]` bump and version column go through `read_version` /
`versions_at` / `apply_version_bump` and need no edits.

### Config and docs

- `config.rs`: the optional `version_manifest` key above; every doc comment
  that says "Repos with no `Cargo.toml`" becomes "repos with neither a
  `Cargo.toml` nor a `package.json`".
- `docs/config.md`: add the key to the table and the paragraph that says the
  five release settings "are inert in repos without a `Cargo.toml`".
- `README.md` "Versioning and release tags": first sentence becomes "When the
  repository has a `Cargo.toml` or a `package.json`"; the bullet about the
  bump landing before the gates because of `cargo update --workspace
  --locked` gets a sentence saying the npm lockfile is rewritten directly and
  `npm ci` is the equivalent gate.
- `docs/internals/algorithms.md` "Version gate and release tags" and the
  `merge_driver.rs` module doc: same generalisation.
- `tests/docs_sync.rs` is unaffected: no new subcommand, no new flag.

### Tests

`tests/harness/mod.rs` gets the npm twin of the Cargo fixture:

- `Sandbox::npm()`: a plain repo with a committed `package.json`.
- `write_package_version(version)`: four-space indented on purpose, with a
  nested object carrying its own `"version"` key that must never be touched,
  and no trailing newline, so formatting preservation is exercised by every
  test that bumps.
- `commit_package_version(version)`.
- `write_package_lock(own)`: a lockfile v3 shape with the root `version`,
  `packages[""].version`, and one dependency entry with its own `version`.

Then `tests/version_release.rs` is parametrised over the two kinds for the
cases where the file shape matters, or gets an npm sibling file covering:

- the gate refuses an unchanged and a lower `package.json` version, and the
  error names `package.json`;
- `rf promote --bump patch --yes` commits both files, both lockfile fields
  match the manifest, and the manifest's formatting is byte-identical apart
  from the version string;
- `rf start` writes `X.Y.Z-roll<N>` into `package.json`, `rf graduate`
  swaps it for `-dev`, a final `rf promote` strips it and tags `vX.Y.Z`;
- two rolls with different markers in `package-lock.json` merge cleanly
  through the driver, and a real dependency conflict in the lockfile is still
  reported;
- a repo with both manifests versions through `Cargo.toml` unless
  `version_manifest = "npm"`;
- a repo with neither is untouched (the existing
  `promote_is_unaffected_in_a_repo_without_cargo_toml` keeps its meaning;
  rename it to say "without a manifest").

### Order of work

1. Introduce `ManifestKind` and `Role` with only the `Cargo` arm, move the
   existing functions behind them, thread `kind.paths()` through the sites
   listed above. `cargo test` must stay green with no test edits: this phase
   is pure refactor.
2. Add `json_string_at` with its unit tests, then the `Npm` arm of every
   method, then `detect`/`detect_at`/`for_path` for two kinds.
3. Merge driver: four files, attributes wiring per detected kind.
4. Harness fixture and the integration tests.
5. Docs and the config key.

Phase 1 can ship on its own as a release; nothing user-visible changes.

## What it means for a Node repo once this lands

christina-website's `.roll-flow.toml` goes back to the defaults: delete the
`scripts/check-version.sh rolling` / `master` gate lines and the script,
set `version_gate = true`, `tag_on_promote = true`, `tag_on_graduate = true`
and `dev_versions = true`, and leave `npm ci` in the gates as the lockfile
check (it fails when `package-lock.json` disagrees with `package.json`). The
three GitHub workflows stay as the PR-side backstop, which is the same
division roll-flow's own repo has between `rf` and its workflows.

## Open questions

- Precedence when both manifests exist: Cargo first is the conservative
  default proposed here. If you would rather fail loudly and require
  `version_manifest` to be set in that case, `detect` is the one place to
  change.
- Whether the workspace root is the only `package.json` that counts. This
  proposal says yes, matching the Cargo arm, which only ever reads the root
  `Cargo.toml`.
- `rf version` prints `rf`'s own version and is unrelated; leaving it alone.
