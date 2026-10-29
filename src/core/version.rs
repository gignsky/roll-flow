//! Crate-version policy: read, compare, and bump the `version` carried by a
//! repo's manifest — `Cargo.toml` for a Rust crate, `package.json` for a Node
//! package — plus its lockfile's own repeated copy of the same value.
//!
//! This mirrors, by construction, the three CI workflows that own the same
//! policy today, so `rf` and GitHub Actions can never disagree:
//!
//! - `.github/workflows/version-bump-check.yml` — a promotion must raise the
//!   version strictly above the branch it targets, and must not still carry a
//!   dev marker (a roll's `-roll<N>`, or rolling's own `-dev`). That second
//!   rule needs its own check on both sides: `sort -V` ranks `0.2.4-roll9` and
//!   `0.2.4-dev` *above* `0.2.4`, and the derived `Ord` here would too, so each
//!   side states the rule explicitly rather than letting the comparison imply
//!   it — see [`VersionStatus::DevVersion`] and the derived [`Ord`] impls for
//!   [`Marker`] and [`Semver`].
//! - `.github/workflows/tag-on-main.yml` — a version change on stable gets an
//!   annotated `vX.Y.Z` tag, created idempotently.
//! - `.github/workflows/release-check.yml` — the tag must match the manifest.
//!
//! Pure logic only: nothing here prints or prompts. A repo with neither a
//! `Cargo.toml` nor a `package.json` (the dotfiles repo roll-flow was built
//! for) yields `VersionStatus::NotApplicable` and every caller silently skips.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::path::Path;

use crate::core::git;
use crate::error::RfError;

// ── Manifest kind ────────────────────────────────────────────────────────────

/// Which kind of manifest carries a version. Declared in precedence order:
/// among manifests at the same depth, a `Cargo.toml` is listed ahead of a
/// `package.json` (see [`Manifest::precedence`]), so it names the release tag
/// — every repo that versioned through Cargo alone before npm support existed
/// keeps its tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManifestKind {
    Cargo,
    Npm,
}

/// Which of a kind's two files a path is — the manifest itself, or the
/// lockfile that repeats its version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Manifest,
    Lockfile,
}

impl ManifestKind {
    pub const ALL: [ManifestKind; 2] = [ManifestKind::Cargo, ManifestKind::Npm];

    /// The manifest's file name: `Cargo.toml` or `package.json`. A
    /// [`Manifest`] joins it to the directory it sits in.
    pub fn manifest(self) -> &'static str {
        match self {
            ManifestKind::Cargo => "Cargo.toml",
            ManifestKind::Npm => "package.json",
        }
    }

    /// The lockfile's file name: `Cargo.lock` or `package-lock.json`.
    pub fn lockfile(self) -> &'static str {
        match self {
            ManifestKind::Cargo => "Cargo.lock",
            ManifestKind::Npm => "package-lock.json",
        }
    }

    /// Which kind `path` belongs to, and whether it is the manifest or the
    /// lockfile, by file name alone (matching at any depth, the way a merge
    /// driver sees a workspace member's own `Cargo.toml`).
    pub fn for_path(path: &Path) -> Option<(ManifestKind, Role)> {
        let name = path.file_name()?.to_str()?;
        for kind in Self::ALL {
            if kind.manifest() == name {
                return Some((kind, Role::Manifest));
            }
            if kind.lockfile() == name {
                return Some((kind, Role::Lockfile));
            }
        }
        None
    }

    /// Extract the package version from this kind's manifest text.
    pub fn parse_version(self, text: &str) -> Option<Semver> {
        match self {
            ManifestKind::Cargo => parse_cargo_version(text),
            ManifestKind::Npm => parse_npm_version(text),
        }
    }

    /// Extract the package name from this kind's manifest text — what the
    /// lockfile's own entry is keyed by. `None` for a kind whose lockfile
    /// does not need one (npm's own root entry is positional, not named).
    pub fn parse_name(self, text: &str) -> Option<String> {
        match self {
            ManifestKind::Cargo => parse_cargo_name(text),
            ManifestKind::Npm => parse_npm_name(text),
        }
    }

    /// Rewrite the version in this kind's manifest text, preserving every
    /// other byte. `None` when no version key was found.
    pub fn replace_version(self, text: &str, new: Semver) -> Option<String> {
        match self {
            ManifestKind::Cargo => replace_cargo_version(text, new),
            ManifestKind::Npm => replace_npm_version(text, new),
        }
    }

    /// Read the crate/package's own version from this kind's lockfile text.
    /// `name` identifies the entry for `Cargo` (required — `None` yields
    /// `None`) and is ignored for `Npm`, whose root entry needs no name.
    pub fn parse_lock_version(self, text: &str, name: Option<&str>) -> Option<Semver> {
        match self {
            ManifestKind::Cargo => parse_cargo_lock_version(text, name?),
            ManifestKind::Npm => parse_npm_lock_version(text),
        }
    }

    /// Rewrite the crate/package's own version in this kind's lockfile text,
    /// touching nothing else. See [`parse_lock_version`](Self::parse_lock_version)
    /// for `name`.
    pub fn replace_lock_version(
        self,
        text: &str,
        name: Option<&str>,
        new: Semver,
    ) -> Option<String> {
        match self {
            ManifestKind::Cargo => replace_cargo_lock_version(text, name?, new),
            ManifestKind::Npm => replace_npm_lock_version(text, new),
        }
    }

    /// Where the version key lives, for a "could not find it" error.
    fn version_key_location(self) -> &'static str {
        match self {
            ManifestKind::Cargo => "the [package] table",
            ManifestKind::Npm => "the root",
        }
    }
}

// ── Manifest ─────────────────────────────────────────────────────────────────

/// One versioned manifest: its kind and the repo-relative directory it sits in
/// (`""` at the root). Its lockfile is the kind's lockfile in that same
/// directory — `keeper/Cargo.toml` locks in `keeper/Cargo.lock`.
///
/// A repo can carry several, at different depths: a site's root `package.json`
/// beside a Rust service in `keeper/`. Each is versioned on its own, and
/// [`Manifest::precedence`] orders them so the first is the one that names the
/// release tag and the one a single-version display shows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Manifest {
    pub kind: ManifestKind,
    dir: String,
}

impl Manifest {
    /// `kind`'s manifest at the repo root.
    pub fn root(kind: ManifestKind) -> Manifest {
        Manifest {
            kind,
            dir: String::new(),
        }
    }

    /// The manifest at repo-relative `path`, or `None` when its file name is
    /// neither `Cargo.toml` nor `package.json`.
    pub fn from_path(path: &str) -> Option<Manifest> {
        let path = path.trim_start_matches("./");
        let (kind, Role::Manifest) = ManifestKind::for_path(Path::new(path))? else {
            return None;
        };
        let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
        Some(Manifest {
            kind,
            dir: dir.trim_end_matches('/').to_string(),
        })
    }

    /// The directory it sits in, repo-relative; `""` at the root.
    pub fn dir(&self) -> &str {
        &self.dir
    }

    fn join(&self, name: &str) -> String {
        if self.dir.is_empty() {
            name.to_string()
        } else {
            format!("{}/{name}", self.dir)
        }
    }

    /// The manifest's repo-relative path: `Cargo.toml`, `keeper/Cargo.toml`.
    pub fn manifest(&self) -> String {
        self.join(self.kind.manifest())
    }

    /// The lockfile's repo-relative path, beside the manifest.
    pub fn lockfile(&self) -> String {
        self.join(self.kind.lockfile())
    }

    /// Both paths, manifest first — what every commit and `git add` lists.
    pub fn paths(&self) -> [String; 2] {
        [self.manifest(), self.lockfile()]
    }

    /// The order a repo's manifests are listed in: shallowest first, then
    /// [`ManifestKind::ALL`] order (Cargo before npm), then path. A root
    /// manifest therefore always outranks a nested one, which is what makes it
    /// the release tag's source in a repo with a site at the root and a crate
    /// in a subdirectory.
    fn precedence(&self) -> (usize, usize, String) {
        let depth = if self.dir.is_empty() {
            0
        } else {
            self.dir.split('/').count()
        };
        let kind = ManifestKind::ALL
            .iter()
            .position(|k| *k == self.kind)
            .unwrap_or(usize::MAX);
        (depth, kind, self.manifest())
    }
}

impl fmt::Display for Manifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.manifest())
    }
}

/// Sort into [`Manifest::precedence`] order and drop duplicates.
fn ordered(mut manifests: Vec<Manifest>) -> Vec<Manifest> {
    manifests.sort_by_key(Manifest::precedence);
    manifests.dedup();
    manifests
}

/// Which manifests a repo versions through — what `version_manifest` in
/// `.roll-flow.toml` resolves to.
///
/// - `Auto` discovers every tracked `Cargo.toml`/`package.json` carrying a
///   readable version, at any depth: a tooling-only `package.json` with no
///   `version` key, or a workspace member inheriting `version.workspace`,
///   is not versioned and is left out.
/// - `Kind` narrows that discovery to one kind.
/// - `Paths` names the manifests outright and discovers nothing — the way to
///   keep a versioned fixture or vendored crate out of the gate.
///
/// Every list it yields is in [`Manifest::precedence`] order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Selection {
    #[default]
    Auto,
    Kind(ManifestKind),
    Paths(Vec<Manifest>),
}

impl Selection {
    fn admits(&self, manifest: &Manifest) -> bool {
        match self {
            Selection::Auto | Selection::Paths(_) => true,
            Selection::Kind(kind) => manifest.kind == *kind,
        }
    }

    /// Every manifest this selection could mean in the working tree, whether
    /// or not it carries a version — what a lookup across many refs tries
    /// (see [`versions_at`]). Discovery reads tracked files (`git ls-files`),
    /// so nothing under an ignored `node_modules` or `target` is ever a
    /// candidate; the two root names are always included so a manifest not
    /// yet committed is still seen.
    pub fn candidates(&self, repo: &Path) -> Vec<Manifest> {
        if let Selection::Paths(list) = self {
            return ordered(list.clone());
        }
        let mut found: Vec<Manifest> = ManifestKind::ALL.into_iter().map(Manifest::root).collect();
        let tracked = git::capture_git(repo, &["ls-files", "--", "*Cargo.toml", "*package.json"])
            .unwrap_or_default();
        found.extend(tracked.lines().filter_map(Manifest::from_path));
        ordered(found.into_iter().filter(|m| self.admits(m)).collect())
    }

    /// The manifests the working tree versions through: each candidate whose
    /// file is present and carries a readable version.
    pub fn manifests(&self, repo: &Path) -> Vec<Manifest> {
        self.candidates(repo)
            .into_iter()
            .filter(|m| read_version(repo, m).ok().flatten().is_some())
            .collect()
    }

    /// [`manifests`](Self::manifests) at `refspec` rather than the working
    /// tree, for a branch that need not be checked out.
    pub fn manifests_at(&self, repo: &Path, refspec: &str) -> Result<Vec<Manifest>, RfError> {
        let candidates = match self {
            Selection::Paths(list) => list.clone(),
            _ => {
                // `ls-tree` takes no glob pathspecs, so the whole tree is
                // listed and filtered here — one call, names only.
                let tree = git::capture_git(repo, &["ls-tree", "-r", "--name-only", refspec])
                    .unwrap_or_default();
                tree.lines()
                    .filter_map(Manifest::from_path)
                    .filter(|m| self.admits(m))
                    .collect()
            }
        };
        let mut found = Vec::new();
        for manifest in ordered(candidates) {
            if version_at(repo, refspec, &manifest)?.is_some() {
                found.push(manifest);
            }
        }
        Ok(found)
    }

    /// [`manifests_at`](Self::manifests_at) for each ref, merged: what a
    /// comparison between two refs has to cover, since either side may carry
    /// a manifest the other does not have yet.
    pub fn manifests_between(
        &self,
        repo: &Path,
        a: &str,
        b: &str,
    ) -> Result<Vec<Manifest>, RfError> {
        let mut all = self.manifests_at(repo, a)?;
        all.extend(self.manifests_at(repo, b)?);
        Ok(ordered(all))
    }
}

/// The version `manifest` carries at `refspec`. `Ok(None)` when the file does
/// not exist there or carries no readable version.
pub fn version_at(
    repo: &Path,
    refspec: &str,
    manifest: &Manifest,
) -> Result<Option<Semver>, RfError> {
    Ok(git::show_file_at_ref(repo, refspec, &manifest.manifest())?
        .and_then(|text| manifest.kind.parse_version(&text)))
}

// ── Semver ──────────────────────────────────────────────────────────────────

/// The pre-release marker a `Semver` carries: a roll branch's own `-roll<N>`,
/// rolling's steady-state `-dev`, or no marker at all (a release version).
///
/// Declared in this order — `Roll` first, `None` last — because `Ord` is
/// derived and compares variants by declaration order: a release must outrank
/// `-dev`, which must outrank any `-roll<N>` of the same numbers, matching
/// semver's "a pre-release precedes the release it marks" rule twice over
/// (roll precedes dev, dev precedes release). Reordering these variants
/// silently flips the promotion gate's sense, so don't.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Marker {
    Roll(u32),
    Dev,
    None,
}

/// A three-field version, optionally carrying a [`Marker`]: `0.2.4-roll9` on a
/// roll branch, `0.2.4-dev` on rolling, or bare `0.2.4` once promoted.
///
/// The marker is narrower than semver's pre-release syntax allows, and
/// deliberately so: `-roll<N>`/`-dev` are the only pre-releases this project
/// produces, and keeping the marker a small `Copy` enum (rather than a
/// `String`) means `Semver` itself stays `Copy`, which ripples through every
/// call site that passes a version by value. Any other suffix still fails to
/// parse, exactly as before.
///
/// `Ord` is derived, comparing fields in declaration order — major, minor,
/// patch, then [`Marker`] — so the numbers dominate the marker exactly as the
/// promotion gate needs: `0.2.5-roll9 > 0.2.4`, but `0.2.4-dev < 0.2.4`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Semver {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub marker: Marker,
}

impl Semver {
    /// Parse `X.Y.Z`, `X.Y.Z-roll<N>` (a roll branch's own dev version), or
    /// `X.Y.Z-dev` (rolling's steady-state dev version).
    ///
    /// Every other pre-release/build suffix is rejected rather than silently
    /// dropped — quietly ignoring one could let a lower version read as higher.
    pub fn parse(raw: &str) -> Option<Semver> {
        let raw = raw.trim();
        let (numbers, marker) = match raw.split_once('-') {
            Some((numbers, "dev")) => (numbers, Marker::Dev),
            Some((numbers, suffix)) => {
                let n = suffix.strip_prefix("roll")?.parse().ok()?;
                (numbers, Marker::Roll(n))
            }
            None => (raw, Marker::None),
        };
        let mut parts = numbers.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Semver {
            major,
            minor,
            patch,
            marker,
        })
    }

    /// True when this version carries any marker at all — a roll's own
    /// `-roll<N>`, or rolling's `-dev` — rather than being a plain release.
    pub fn has_marker(self) -> bool {
        self.marker != Marker::None
    }

    /// The same version with its marker removed — what a final promotion
    /// writes before merging into stable.
    pub fn release(self) -> Semver {
        Semver {
            marker: Marker::None,
            ..self
        }
    }

    /// The same numbers marked as roll `n`'s dev version — what `rf start`
    /// writes. The base numbers are deliberately left alone: graduation moves
    /// the marker to `-dev` without touching them, so the promotion gate then
    /// reports `UNCHANGED` (once finalized) and demands a real bump.
    pub fn as_roll(self, n: u32) -> Semver {
        self.with_marker(Marker::Roll(n))
    }

    /// The same numbers marked as rolling's `-dev` version — what `rf
    /// graduate` writes in place of the roll's own `-roll<N>`.
    pub fn as_rolling_dev(self) -> Semver {
        self.with_marker(Marker::Dev)
    }

    /// The same numbers with an arbitrary marker swapped in. The general form
    /// behind [`as_roll`](Self::as_roll)/[`as_rolling_dev`](Self::as_rolling_dev)/
    /// [`release`](Self::release), and what the merge driver's "keep ours's
    /// marker, take the higher number" rule needs: it doesn't know in advance
    /// which of the three it's keeping.
    pub fn with_marker(self, marker: Marker) -> Semver {
        Semver { marker, ..self }
    }

    /// The next version at `level`, zeroing the fields below it.
    ///
    /// The marker is carried through: bumping on a roll branch raises the
    /// base graduation will carry onto rolling's `-dev`, which is the other
    /// route to a promotable version besides bumping on rolling afterwards.
    pub fn bump(self, level: BumpLevel) -> Semver {
        match level {
            BumpLevel::Patch => Semver {
                patch: self.patch + 1,
                ..self
            },
            BumpLevel::Minor => Semver {
                major: self.major,
                minor: self.minor + 1,
                patch: 0,
                marker: self.marker,
            },
            BumpLevel::Major => Semver {
                major: self.major + 1,
                minor: 0,
                patch: 0,
                marker: self.marker,
            },
        }
    }

    /// The release tag for this version, e.g. `v0.1.3` or `v0.2.6-dev`.
    /// Matches the `v$crate` form `tag-on-main.yml` builds.
    pub fn tag(self) -> String {
        format!("v{self}")
    }
}

impl fmt::Display for Semver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        match self.marker {
            Marker::Roll(n) => write!(f, "-roll{n}"),
            Marker::Dev => write!(f, "-dev"),
            Marker::None => Ok(()),
        }
    }
}

/// Which field to raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum BumpLevel {
    Patch,
    Minor,
    Major,
}

impl fmt::Display for BumpLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            BumpLevel::Patch => "patch",
            BumpLevel::Minor => "minor",
            BumpLevel::Major => "major",
        };
        f.write_str(s)
    }
}

// ── Check ───────────────────────────────────────────────────────────────────

/// Verdict of comparing a source branch's version to its merge target's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionStatus {
    /// Source is strictly above target — the bump requirement is satisfied.
    Ok,
    /// Source equals target: no bump was made.
    Unchanged,
    /// Source is *below* target. Never valid; no prompt can fix it safely.
    Lower,
    /// A manifest exists but its version could not be read or parsed.
    Unreadable,
    /// Source still carries a dev marker — a roll's `-roll<N>`, or rolling's
    /// own `-dev`. Its own status rather than folded into `Unchanged`,
    /// because the fix is different: strip the marker (what `rf graduate`
    /// does for a roll, and what a *final* `rf promote` does for rolling)
    /// rather than raise the numbers.
    DevVersion,
    /// No manifest on either side, or the gate is disabled in config.
    NotApplicable,
}

/// The two versions being compared plus the verdict.
#[derive(Debug, Clone)]
pub struct VersionCheck {
    /// Which manifest this compared. `None` when nothing was applicable.
    pub manifest: Option<Manifest>,
    pub head: Option<Semver>,
    pub base: Option<Semver>,
    pub status: VersionStatus,
}

impl VersionCheck {
    pub fn not_applicable() -> VersionCheck {
        VersionCheck {
            manifest: None,
            head: None,
            base: None,
            status: VersionStatus::NotApplicable,
        }
    }

    /// The one verdict a set of per-manifest checks amounts to: the most
    /// serious unsatisfied check (`Lower`, then `Unreadable`, `DevVersion`,
    /// `Unchanged`), else the first that passed, else `NotApplicable`. A repo
    /// with one manifest gets that manifest's own check back unchanged.
    pub fn worst(checks: &[VersionCheck]) -> VersionCheck {
        let rank = |status: VersionStatus| match status {
            VersionStatus::Lower => 0,
            VersionStatus::Unreadable => 1,
            VersionStatus::DevVersion => 2,
            VersionStatus::Unchanged => 3,
            VersionStatus::Ok => 4,
            VersionStatus::NotApplicable => 5,
        };
        checks
            .iter()
            .min_by_key(|c| rank(c.status))
            .cloned()
            .unwrap_or_else(VersionCheck::not_applicable)
    }

    /// True when the check neither blocks nor has anything to report.
    pub fn is_satisfied(&self) -> bool {
        matches!(
            self.status,
            VersionStatus::Ok | VersionStatus::NotApplicable
        )
    }
}

/// Compare `manifest`'s version at `source_ref` to its version at
/// `target_ref` — the same comparison `version-bump-check.yml` makes between a
/// PR head and its base.
///
/// A side where the file is absent, or carries no readable version (a
/// tooling-only `package.json`), reads as having none: a manifest that gains
/// its first version on this very branch is a bump from nothing, not
/// `Unreadable`.
pub fn check(
    repo: &Path,
    source_ref: &str,
    target_ref: &str,
    manifest: &Manifest,
) -> Result<VersionCheck, RfError> {
    let head = version_at(repo, source_ref, manifest)?;
    let base = version_at(repo, target_ref, manifest)?;
    // Neither side versions through this manifest: nothing to compare.
    if head.is_none() && base.is_none() {
        return Ok(VersionCheck::not_applicable());
    }
    Ok(VersionCheck {
        manifest: Some(manifest.clone()),
        head,
        base,
        status: verdict(head, base),
    })
}

/// The gate's verdict on a head version against its base. `base` is `None`
/// only when the target has no version in this manifest at all, which makes
/// any readable head a bump from nothing.
fn verdict(head: Option<Semver>, base: Option<Semver>) -> VersionStatus {
    match (head, base) {
        // Checked ahead of the comparison: a dev version can be numerically
        // above its target and still must not promote — `0.2.5-roll9 > 0.2.4`
        // is true, and shipping a `-roll9` (or un-finalized `-dev`) version to
        // stable (and tagging it `v0.2.5-roll9`) is exactly what this gate
        // exists to stop.
        (Some(h), _) if h.has_marker() => VersionStatus::DevVersion,
        // A manifest added by this very branch is a bump from nothing.
        (Some(_), None) => VersionStatus::Ok,
        (Some(h), Some(b)) if h > b => VersionStatus::Ok,
        (Some(h), Some(b)) if h == b => VersionStatus::Unchanged,
        (Some(_), Some(_)) => VersionStatus::Lower,
        (None, _) => VersionStatus::Unreadable,
    }
}

/// [`check`] once per manifest in `manifests`, dropping those neither ref
/// versions through. Empty when the repo versions through none.
pub fn check_all(
    repo: &Path,
    source_ref: &str,
    target_ref: &str,
    manifests: &[Manifest],
) -> Result<Vec<VersionCheck>, RfError> {
    let mut checks = Vec::new();
    for manifest in manifests {
        let check = check(repo, source_ref, target_ref, manifest)?;
        if check.status != VersionStatus::NotApplicable {
            checks.push(check);
        }
    }
    Ok(checks)
}

/// Like [`check`], but against an already-resolved `head` rather than reading
/// `source_ref` fresh.
///
/// A per-roll promotion step needs to gate on the *finalized* (marker-
/// stripped) version a graduation commit will carry once landed on stable,
/// not the raw `-dev` value still sitting in that commit's manifest —
/// `check` alone would report `DevVersion` on every single per-roll
/// promotion step otherwise, since the raw value never actually reaches
/// stable; finalizing is exactly what landing it does.
///
/// `manifest` is `None` when the source versions through nothing; the
/// verdict is then `NotApplicable`, or `Unreadable` against a target that
/// does carry a version (passed as the manifest found there).
pub fn check_against(
    repo: &Path,
    head: Option<Semver>,
    target_ref: &str,
    manifest: Option<&Manifest>,
) -> Result<VersionCheck, RfError> {
    let Some(manifest) = manifest else {
        return Ok(VersionCheck::not_applicable());
    };
    let base = version_at(repo, target_ref, manifest)?;
    if head.is_none() && base.is_none() {
        return Ok(VersionCheck::not_applicable());
    }
    Ok(VersionCheck {
        manifest: Some(manifest.clone()),
        head,
        base,
        status: verdict(head, base),
    })
}

/// Read `manifest`'s version from the working tree. `Ok(None)` when the file
/// does not exist here or carries no readable version.
pub fn read_version(repo: &Path, manifest: &Manifest) -> Result<Option<Semver>, RfError> {
    let path = repo.join(manifest.manifest());
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(manifest.kind.parse_version(&text))
}

/// Every manifest's version at a set of refs: one column per manifest, one
/// row per ref. What the version columns of `rf list`/`rf status` and the TUI
/// draw, so a repo versioned through `package.json` *and*
/// `keeper/Cargo.toml` shows both side by side.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionTable {
    /// The manifests with a version at one or more of the refs, in precedence
    /// order — one column each. Empty in a repo that versions through none.
    pub manifests: Vec<Manifest>,
    /// Per key (a ref, or whatever the caller re-keyed it by), one entry per
    /// column of [`manifests`](Self::manifests).
    pub rows: HashMap<String, Vec<Option<Semver>>>,
}

impl VersionTable {
    /// The row for `key`: one cell per column, `None` where that manifest has
    /// no readable version there. Every column is `None` for an unknown key.
    pub fn row(&self, key: &str) -> Vec<Option<Semver>> {
        self.rows
            .get(key)
            .cloned()
            .unwrap_or_else(|| vec![None; self.manifests.len()])
    }

    /// The header for column `i`: `version` when there is only one, so a
    /// single-manifest repo reads as it always has; otherwise which manifest —
    /// its directory when nested (`keeper`), its file name at the root.
    pub fn header(&self, i: usize) -> String {
        match self.manifests.as_slice() {
            [_] => "version".to_string(),
            many => {
                let manifest = &many[i];
                if manifest.dir().is_empty() {
                    manifest.kind.manifest().to_string()
                } else {
                    manifest.dir().to_string()
                }
            }
        }
    }

    /// The same table keyed by name instead of by ref: each `(name, ref)`
    /// pair takes the row read at `ref` — a branch, say, read at the ref
    /// [`branches::content_ref`](crate::core::branches::content_ref) picked.
    pub fn keyed_by<'a>(
        &self,
        pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> VersionTable {
        VersionTable {
            manifests: self.manifests.clone(),
            rows: pairs
                .into_iter()
                .map(|(name, r)| (name.to_string(), self.row(r)))
                .collect(),
        }
    }
}

/// Read every candidate's version at each of `refs` (see
/// [`Selection::candidates`]), as a [`VersionTable`] keyed by ref. Candidates
/// with no version at any of the refs get no column.
///
/// One `git cat-file --batch` for the whole set, because the caller is a
/// table that wants its versions on every reload. A ref where a manifest is
/// absent, or carries a version this crate will not parse, gets `None` in
/// that column — rendered as a dash — and a repo with no manifest at all
/// yields an empty table rather than an error. Losing a version is never
/// worth failing a reload over.
pub fn versions_at(repo: &Path, refs: &[String], candidates: &[Manifest]) -> VersionTable {
    let spec = |r: &str, manifest: &Manifest| format!("{r}:{}", manifest.manifest());
    let specs: Vec<String> = refs
        .iter()
        .flat_map(|r| candidates.iter().map(move |m| spec(r, m)))
        .collect();
    if specs.is_empty() {
        return VersionTable::default();
    }
    let Ok(blobs) = git::show_files_at_refs(repo, &specs) else {
        return VersionTable::default();
    };
    let read = |r: &str, manifest: &Manifest| {
        blobs
            .get(&spec(r, manifest))
            .and_then(|text| manifest.kind.parse_version(text))
    };
    let manifests: Vec<Manifest> = candidates
        .iter()
        .filter(|m| refs.iter().any(|r| read(r, m).is_some()))
        .cloned()
        .collect();
    let rows = refs
        .iter()
        .map(|r| (r.clone(), manifests.iter().map(|m| read(r, m)).collect()))
        .collect();
    VersionTable { manifests, rows }
}

// ── Write ───────────────────────────────────────────────────────────────────

/// Rewrite the package version in the working tree's copy of `manifest`.
///
/// Deliberately line/byte-based rather than a serialize round-trip: that
/// would reorder keys and strip comments (TOML) or reformat whitespace
/// (JSON). We locate the version key directly and replace only its value.
pub fn write_version(repo: &Path, manifest: &Manifest, new: Semver) -> Result<(), RfError> {
    let path = repo.join(manifest.manifest());
    let kind = manifest.kind;
    let text = std::fs::read_to_string(&path)?;
    let rewritten = kind.replace_version(&text, new).ok_or_else(|| {
        RfError::Git(format!(
            "could not find a `version` key in {} of {}",
            kind.version_key_location(),
            path.display()
        ))
    })?;
    std::fs::write(&path, rewritten)?;
    Ok(())
}

// ── Cargo arm ────────────────────────────────────────────────────────────────

/// Extract `package.version` from `Cargo.toml` text.
///
/// Parsed with the `toml` crate rather than a regex so section nesting is
/// honoured — a `version = ...` under `[dependencies.foo]` can never be
/// mistaken for the package version.
fn parse_cargo_version(cargo_toml: &str) -> Option<Semver> {
    let doc: toml::Value = toml::from_str(cargo_toml).ok()?;
    let raw = doc.get("package")?.get("version")?.as_str()?;
    Semver::parse(raw)
}

/// Pure half of [`write_version`]'s Cargo arm, so the formatting-preservation
/// behaviour can be unit-tested without touching a repo. Returns `None` if no
/// version key was found in `[package]`.
fn replace_cargo_version(text: &str, new: Semver) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut in_package = false;
    let mut replaced = false;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            // Section header. `[package]` opens the table we care about; any
            // other header (including `[package.metadata]`) closes it.
            in_package = trimmed.starts_with("[package]");
        } else if in_package && !replaced && is_version_key(trimmed) {
            let indent = &line[..line.len() - trimmed.len()];
            out.push_str(indent);
            out.push_str(&format!("version = \"{new}\""));
            out.push('\n');
            replaced = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }

    if !replaced {
        return None;
    }
    // `lines()` drops a missing trailing newline; restore the original shape.
    if !text.ends_with('\n') {
        out.pop();
    }
    Some(out)
}

/// Extract `package.name` from `Cargo.toml` text — what the lockfile's own
/// `[[package]]` entry is keyed by.
fn parse_cargo_name(cargo_toml: &str) -> Option<String> {
    let doc: toml::Value = toml::from_str(cargo_toml).ok()?;
    Some(doc.get("package")?.get("name")?.as_str()?.to_string())
}

/// The crate's own version as `Cargo.lock` records it, under the
/// `[[package]]` entry named `name`. `None` when there is no such entry, or
/// when it cannot be told apart from another (see [`lock_own_version_line`]).
fn parse_cargo_lock_version(cargo_lock: &str, name: &str) -> Option<Semver> {
    let lines: Vec<&str> = cargo_lock.lines().collect();
    let line = lines[lock_own_version_line(&lines, name)?];
    Semver::parse(line.split('"').nth(1)?)
}

/// Rewrite the crate's own `version` in `Cargo.lock` text, leaving every
/// other byte — the lockfile's own top-level `version = 4`, every dependency
/// entry — exactly as it was. Line-based for the same reason as
/// [`replace_cargo_version`], and `None` under the same conditions as
/// [`parse_cargo_lock_version`].
///
/// This is the one lockfile change `cargo update --workspace` makes after a
/// version rewrite, done without cargo: no network, no toolchain, and no
/// chance of moving a dependency as a side effect.
fn replace_cargo_lock_version(cargo_lock: &str, name: &str, new: Semver) -> Option<String> {
    let lines: Vec<&str> = cargo_lock.lines().collect();
    let at = lock_own_version_line(&lines, name)?;
    let mut out = String::with_capacity(cargo_lock.len());
    for (i, line) in lines.iter().enumerate() {
        if i == at {
            let indent = &line[..line.len() - line.trim_start().len()];
            out.push_str(indent);
            out.push_str(&format!("version = \"{new}\""));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    if !cargo_lock.ends_with('\n') {
        out.pop();
    }
    Some(out)
}

/// Index of the `version = ...` line in the crate's own `[[package]]` entry.
///
/// "Own" is the entry named `name` with no `source` key: cargo records a
/// `source` for everything fetched from a registry or git, and none for a
/// workspace member, so a registry crate that happens to share the name is
/// never mistaken for this one. More than one match counts as no match —
/// guessing which entry to rewrite is how a real dependency would get
/// silently moved.
fn lock_own_version_line(lines: &[&str], name: &str) -> Option<usize> {
    let wanted = format!("name = \"{name}\"");
    let mut found = None;
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() != "[[package]]" {
            i += 1;
            continue;
        }
        let (mut named, mut sourced, mut version) = (false, false, None);
        i += 1;
        while i < lines.len() && !lines[i].trim_start().starts_with('[') {
            let trimmed = lines[i].trim();
            if trimmed == wanted {
                named = true;
            } else if is_key(trimmed, "source") {
                sourced = true;
            } else if version.is_none() && is_version_key(trimmed) {
                version = Some(i);
            }
            i += 1;
        }
        if named && !sourced {
            if found.is_some() {
                return None;
            }
            found = Some(version?);
        }
    }
    found
}

/// True for a `version = ...` assignment (allowing whitespace around `=`), the
/// same shape the workflows' `^version[[:space:]]*=` grep matches.
fn is_version_key(trimmed: &str) -> bool {
    is_key(trimmed, "version")
}

/// True for a `<key> = ...` assignment, allowing whitespace around `=`.
fn is_key(trimmed: &str, key: &str) -> bool {
    trimmed
        .strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

// ── Npm arm ──────────────────────────────────────────────────────────────────

/// Extract `version` from `package.json` text. Validated through
/// `serde_json` first, so malformed JSON reads as unreadable rather than
/// whatever the byte scanner happens to find.
fn parse_npm_version(package_json: &str) -> Option<Semver> {
    serde_json::from_str::<serde_json::Value>(package_json).ok()?;
    let range = json_string_at(package_json, &["version"])?;
    Semver::parse(&package_json[range])
}

/// Extract `name` from `package.json` text — what `merge_driver` and error
/// messages name the package by. The npm lockfile does not need it (its root
/// entry is positional), but the signature stays uniform with the Cargo arm.
fn parse_npm_name(package_json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(package_json).ok()?;
    let range = json_string_at(package_json, &["name"])?;
    Some(package_json[range].to_string())
}

/// Rewrite `version` in `package.json` text, splicing the new value into the
/// same byte range and leaving every other byte — indentation, key order, a
/// missing trailing newline — untouched. `None` when the key is absent.
fn replace_npm_version(text: &str, new: Semver) -> Option<String> {
    let range = json_string_at(text, &["version"])?;
    Some(splice(text, &[(range, new)]))
}

/// The root `version` in `package-lock.json` text. Present in every lockfile
/// version (1, 2 and 3).
fn parse_npm_lock_version(text: &str) -> Option<Semver> {
    serde_json::from_str::<serde_json::Value>(text).ok()?;
    let range = json_string_at(text, &["version"])?;
    Semver::parse(&text[range])
}

/// Rewrite the root `version` in `package-lock.json` text, and, when present
/// (lockfile v2/v3), `packages[""].version` alongside it. Returns the text
/// with only the root changed — not `None` — when there is no `packages[""]`
/// entry (v1), since that shape has nothing more to keep in sync.
fn replace_npm_lock_version(text: &str, new: Semver) -> Option<String> {
    let root = json_string_at(text, &["version"])?;
    let mut edits = vec![(root, new)];
    if let Some(own) = json_string_at(text, &["packages", "", "version"]) {
        edits.push((own, new));
    }
    Some(splice(text, &edits))
}

/// Replace each byte range in `edits` with the given version's text,
/// back-to-front so earlier ranges stay valid as later ones shift the string.
fn splice(text: &str, edits: &[(Range<usize>, Semver)]) -> String {
    let mut ordered: Vec<&(Range<usize>, Semver)> = edits.iter().collect();
    ordered.sort_by_key(|(r, _)| r.start);
    let mut out = String::with_capacity(text.len() + 8 * ordered.len());
    let mut cursor = 0;
    for (range, new) in ordered {
        out.push_str(&text[cursor..range.start]);
        out.push_str(&new.to_string());
        cursor = range.end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Byte range of the string value at `path` (e.g. `["version"]`, or
/// `["packages", "", "version"]` for a lockfile v2/v3 entry), excluding the
/// quotes. `None` when the path is absent or the value there is not a
/// string.
///
/// A small scanner, no allocation beyond the key stack: walk the bytes
/// tracking string state (honouring `\"`), container nesting, and for each
/// object level the key currently being valued. Array elements push `None`
/// onto that stack rather than a string, so a `"version"` nested inside
/// `"workspaces": [...]` (or any array) can never match a string-keyed path —
/// the path lengths might agree, but an array slot never equals `Some(key)`.
fn json_string_at(text: &str, path: &[&str]) -> Option<Range<usize>> {
    let bytes = text.as_bytes();
    let (_, found) = scan_json_value(bytes, 0, &[], path);
    found
}

fn skip_json_ws(bytes: &[u8], mut i: usize) -> usize {
    while matches!(bytes.get(i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        i += 1;
    }
    i
}

/// Scan one JSON value. `current` is the key-path leading to this value.
/// Returns the index just past the value, and the target's range if this
/// value (or one nested inside it) is the match.
fn scan_json_value(
    bytes: &[u8],
    i: usize,
    current: &[Option<String>],
    target: &[&str],
) -> (usize, Option<Range<usize>>) {
    let i = skip_json_ws(bytes, i);
    match bytes.get(i) {
        Some(b'"') => {
            let (end, range) = scan_json_string(bytes, i);
            let matched = json_path_matches(current, target).then_some(range);
            (end, matched)
        }
        Some(b'{') => scan_json_object(bytes, i, current, target),
        Some(b'[') => scan_json_array(bytes, i, current, target),
        Some(_) => (scan_json_scalar(bytes, i), None),
        None => (i, None),
    }
}

fn json_path_matches(current: &[Option<String>], target: &[&str]) -> bool {
    current.len() == target.len()
        && current
            .iter()
            .zip(target.iter())
            .all(|(c, t)| c.as_deref() == Some(*t))
}

fn scan_json_object(
    bytes: &[u8],
    i: usize,
    current: &[Option<String>],
    target: &[&str],
) -> (usize, Option<Range<usize>>) {
    let mut i = i + 1; // skip '{'
    let mut found = None;
    loop {
        i = skip_json_ws(bytes, i);
        match bytes.get(i) {
            Some(b'}') | None => {
                i = i.saturating_add(1);
                break;
            }
            Some(b',') => i += 1,
            Some(b'"') => {
                let (key_end, key_range) = scan_json_string(bytes, i);
                let key = String::from_utf8_lossy(&bytes[key_range]).into_owned();
                i = skip_json_ws(bytes, key_end);
                if bytes.get(i) != Some(&b':') {
                    break;
                }
                i += 1;
                let mut child = current.to_vec();
                child.push(Some(key));
                let (next_i, child_found) = scan_json_value(bytes, i, &child, target);
                if found.is_none() {
                    found = child_found;
                }
                i = next_i;
            }
            _ => break,
        }
    }
    (i, found)
}

fn scan_json_array(
    bytes: &[u8],
    i: usize,
    current: &[Option<String>],
    target: &[&str],
) -> (usize, Option<Range<usize>>) {
    let mut i = i + 1; // skip '['
    let mut found = None;
    let mut child = current.to_vec();
    child.push(None);
    loop {
        i = skip_json_ws(bytes, i);
        match bytes.get(i) {
            Some(b']') | None => {
                i = i.saturating_add(1);
                break;
            }
            Some(b',') => i += 1,
            _ => {
                let (next_i, child_found) = scan_json_value(bytes, i, &child, target);
                if found.is_none() {
                    found = child_found;
                }
                if next_i <= i {
                    break; // malformed input; avoid looping forever
                }
                i = next_i;
            }
        }
    }
    (i, found)
}

/// `i` points at the opening `"`. Returns the index just past the closing
/// quote, and the range of the content between the quotes (escapes included
/// verbatim, not decoded — every path and value this module compares against
/// is plain ASCII with no escapes).
fn scan_json_string(bytes: &[u8], i: usize) -> (usize, Range<usize>) {
    let start = i + 1;
    let mut j = start;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            b'"' => break,
            _ => j += 1,
        }
    }
    let end = j.min(bytes.len());
    (end + 1, start..end)
}

/// Skip a number, `true`, `false`, or `null` — anything that isn't a string,
/// object or array — up to the next structural byte.
fn scan_json_scalar(bytes: &[u8], i: usize) -> usize {
    let mut j = i;
    while j < bytes.len() && !matches!(bytes[j], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
    {
        j += 1;
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_roll_version_sorts_below_the_release_it_marks() {
        let dev = Semver::parse("0.2.4-roll9").expect("roll version parses");
        let release = Semver::parse("0.2.4").expect("release parses");

        assert_eq!(dev.marker, Marker::Roll(9));
        assert!(dev.has_marker());
        assert!(!release.has_marker());

        // The whole reason `Marker`'s declaration order matters: the derived
        // `Ord` would otherwise let a roll's dev version promote over the
        // release it was branched from.
        assert!(dev < release, "{dev} !< {release}");
        assert!(release > dev);
        // And the numbers still dominate the marker.
        assert!(Semver::parse("0.2.5-roll9").unwrap() > release);
        assert!(dev < Semver::parse("0.2.4-roll10").unwrap());
    }

    #[test]
    fn a_dev_version_sorts_below_the_release_but_above_a_roll_of_the_same_numbers() {
        let roll = Semver::parse("0.2.4-roll9").unwrap();
        let dev = Semver::parse("0.2.4-dev").unwrap();
        let release = Semver::parse("0.2.4").unwrap();

        assert_eq!(dev.marker, Marker::Dev);
        assert!(dev.has_marker());
        assert!(roll < dev, "{roll} !< {dev}");
        assert!(dev < release, "{dev} !< {release}");
        // Numbers still dominate every marker kind.
        assert!(Semver::parse("0.2.5-dev").unwrap() > release);
        assert!(Semver::parse("0.2.3-dev").unwrap() < release);
    }

    #[test]
    fn a_roll_version_round_trips_and_releases() {
        let dev = Semver::parse("1.10.3-roll42").unwrap();
        assert_eq!(dev.to_string(), "1.10.3-roll42");
        assert_eq!(dev.release().to_string(), "1.10.3");
        assert_eq!(Semver::parse("1.10.3").unwrap().as_roll(42), dev);

        // A bump on a roll branch keeps the marker, raising the base that
        // graduation will carry onto rolling's `-dev`.
        assert_eq!(dev.bump(BumpLevel::Patch).to_string(), "1.10.4-roll42");
        assert_eq!(dev.bump(BumpLevel::Minor).to_string(), "1.11.0-roll42");
        assert_eq!(dev.bump(BumpLevel::Major).to_string(), "2.0.0-roll42");
    }

    #[test]
    fn a_dev_version_round_trips_and_releases() {
        let dev = Semver::parse("1.10.3-dev").unwrap();
        assert_eq!(dev.to_string(), "1.10.3-dev");
        assert_eq!(dev.release().to_string(), "1.10.3");
        assert_eq!(Semver::parse("1.10.3").unwrap().as_rolling_dev(), dev);
        assert_eq!(dev.bump(BumpLevel::Patch).to_string(), "1.10.4-dev");
    }

    #[test]
    fn with_marker_swaps_the_marker_only() {
        let roll = Semver::parse("0.2.4-roll9").unwrap();
        assert_eq!(roll.with_marker(Marker::Dev).to_string(), "0.2.4-dev");
        assert_eq!(roll.with_marker(Marker::None).to_string(), "0.2.4");
        assert_eq!(roll.with_marker(Marker::Roll(7)).to_string(), "0.2.4-roll7");
    }

    #[test]
    fn suffixes_that_are_not_markers_are_still_rejected() {
        // Unchanged behaviour: anything this crate cannot represent exactly is
        // refused rather than silently dropped, since dropping it could let a
        // lower version read as higher.
        for raw in [
            "0.2.4-beta",
            "0.2.4-roll",
            "0.2.4-rollx",
            "0.2.4-1",
            "0.2.4-devel",
            "0.2.4+build",
        ] {
            assert_eq!(Semver::parse(raw), None, "{raw} should not parse");
        }
    }

    #[test]
    fn parses_and_orders_versions() {
        assert_eq!(
            Semver::parse("0.1.2"),
            Some(Semver {
                major: 0,
                minor: 1,
                patch: 2,
                marker: Marker::None,
            })
        );
        assert!(Semver::parse("0.1.3").unwrap() > Semver::parse("0.1.2").unwrap());
        assert!(Semver::parse("0.2.0").unwrap() > Semver::parse("0.1.99").unwrap());
        assert!(Semver::parse("1.0.0").unwrap() > Semver::parse("0.99.99").unwrap());
    }

    #[test]
    fn rejects_malformed_versions() {
        assert_eq!(Semver::parse("0.1"), None);
        assert_eq!(Semver::parse("0.1.2.3"), None);
        assert_eq!(Semver::parse("0.1.2-rc1"), None);
        assert_eq!(Semver::parse("not-a-version"), None);
    }

    #[test]
    fn bump_zeroes_lower_fields() {
        let v = Semver::parse("1.2.3").unwrap();
        assert_eq!(v.bump(BumpLevel::Patch).to_string(), "1.2.4");
        assert_eq!(v.bump(BumpLevel::Minor).to_string(), "1.3.0");
        assert_eq!(v.bump(BumpLevel::Major).to_string(), "2.0.0");
    }

    #[test]
    fn tag_matches_ci_format() {
        assert_eq!(Semver::parse("0.1.3").unwrap().tag(), "v0.1.3");
        assert_eq!(Semver::parse("0.2.6-dev").unwrap().tag(), "v0.2.6-dev");
    }

    #[test]
    fn reads_package_version_not_dependency_version() {
        let manifest = r#"
[package]
name = "roll-flow"
version = "0.1.2"

[dependencies]
clap = { version = "4.6.1" }

[dependencies.serde]
version = "1.0.228"
"#;
        assert_eq!(parse_cargo_version(manifest), Semver::parse("0.1.2"));
    }

    #[test]
    fn rewrite_preserves_comments_and_layout() {
        let manifest = "[package]\n\
                        name = \"roll-flow\"  # the binary\n\
                        version = \"0.1.2\"\n\
                        edition = \"2021\"\n\
                        \n\
                        [dependencies]\n\
                        toml = \"1.1.2\"\n";
        let out = replace_cargo_version(manifest, Semver::parse("0.1.3").unwrap()).unwrap();
        assert!(out.contains("version = \"0.1.3\""));
        assert!(out.contains("name = \"roll-flow\"  # the binary"));
        assert!(out.contains("edition = \"2021\""));
        // The dependency version must be untouched.
        assert!(out.contains("toml = \"1.1.2\""));
        assert_eq!(parse_cargo_version(&out), Semver::parse("0.1.3"));
    }

    #[test]
    fn rewrite_ignores_version_keys_outside_package() {
        let manifest = "[dependencies]\n\
                        version = \"9.9.9\"\n\
                        \n\
                        [package]\n\
                        version = \"0.1.2\"\n";
        let out = replace_cargo_version(manifest, Semver::parse("0.2.0").unwrap()).unwrap();
        assert!(out.contains("version = \"9.9.9\""));
        assert!(out.contains("version = \"0.2.0\""));
    }

    #[test]
    fn rewrite_reports_missing_version_key() {
        let manifest = "[package]\nname = \"x\"\n";
        assert_eq!(
            replace_cargo_version(manifest, Semver::parse("1.0.0").unwrap()),
            None
        );
    }

    #[test]
    fn not_applicable_is_satisfied() {
        assert!(VersionCheck::not_applicable().is_satisfied());
    }

    const LOCK: &str = "# generated\nversion = 4\n\n\
        [[package]]\nname = \"roll-flow\"\nversion = \"0.2.7-roll35\"\n\
        dependencies = [\n \"serde\",\n]\n\n\
        [[package]]\nname = \"serde\"\nversion = \"1.0.228\"\n\
        source = \"registry+https://github.com/rust-lang/crates.io-index\"\n";

    #[test]
    fn the_lockfile_s_own_entry_is_read_and_rewritten_alone() {
        assert_eq!(
            parse_cargo_lock_version(LOCK, "roll-flow"),
            Semver::parse("0.2.7-roll35")
        );
        let next = Semver::parse("0.2.8-roll35").unwrap();
        let out = replace_cargo_lock_version(LOCK, "roll-flow", next).unwrap();
        assert_eq!(out, LOCK.replace("0.2.7-roll35", "0.2.8-roll35"));
        // The lockfile format's own `version = 4` and the dependency are left
        // exactly as they were.
        assert!(out.contains("version = 4\n"));
        assert!(out.contains("version = \"1.0.228\""));
    }

    #[test]
    fn a_registry_entry_is_never_the_crate_s_own() {
        // Named like a dependency that carries a `source`: not ours.
        assert_eq!(parse_cargo_lock_version(LOCK, "serde"), None);
        let v = Semver::parse("9.9.9").unwrap();
        assert_eq!(replace_cargo_lock_version(LOCK, "serde", v), None);
        assert_eq!(parse_cargo_lock_version(LOCK, "absent"), None);
    }

    #[test]
    fn an_ambiguous_own_entry_is_left_alone() {
        let twice = format!("{LOCK}\n[[package]]\nname = \"roll-flow\"\nversion = \"0.1.0\"\n");
        assert_eq!(parse_cargo_lock_version(&twice, "roll-flow"), None);
    }

    #[test]
    fn the_package_name_is_read_from_the_manifest() {
        let toml = "[package]\nname = \"roll-flow\"\nversion = \"0.2.7\"\n";
        assert_eq!(parse_cargo_name(toml).as_deref(), Some("roll-flow"));
        assert_eq!(parse_cargo_name("[workspace]\n"), None);
    }

    // ── ManifestKind ─────────────────────────────────────────────────────────

    #[test]
    fn for_path_recognises_either_kind_at_any_depth() {
        assert_eq!(
            ManifestKind::for_path(Path::new("Cargo.toml")),
            Some((ManifestKind::Cargo, Role::Manifest))
        );
        assert_eq!(
            ManifestKind::for_path(Path::new("crates/x/Cargo.lock")),
            Some((ManifestKind::Cargo, Role::Lockfile))
        );
        assert_eq!(
            ManifestKind::for_path(Path::new("package.json")),
            Some((ManifestKind::Npm, Role::Manifest))
        );
        assert_eq!(
            ManifestKind::for_path(Path::new("packages/app/package-lock.json")),
            Some((ManifestKind::Npm, Role::Lockfile))
        );
        assert_eq!(ManifestKind::for_path(Path::new("README.md")), None);
    }

    #[test]
    fn a_manifest_knows_its_paths_from_where_it_sits() {
        let keeper = Manifest::from_path("keeper/Cargo.toml").unwrap();
        assert_eq!(keeper.kind, ManifestKind::Cargo);
        assert_eq!(keeper.dir(), "keeper");
        assert_eq!(keeper.manifest(), "keeper/Cargo.toml");
        assert_eq!(keeper.lockfile(), "keeper/Cargo.lock");
        assert_eq!(
            Manifest::from_path("./package.json"),
            Some(Manifest::root(ManifestKind::Npm))
        );
        assert_eq!(
            Manifest::root(ManifestKind::Npm).lockfile(),
            "package-lock.json"
        );
        // A lockfile, or anything else, is not a manifest.
        assert_eq!(Manifest::from_path("keeper/Cargo.lock"), None);
        assert_eq!(Manifest::from_path("README.md"), None);
    }

    #[test]
    fn manifests_order_root_first_then_cargo_before_npm() {
        let at = |p: &str| Manifest::from_path(p).unwrap();
        let sorted = ordered(vec![
            at("keeper/Cargo.toml"),
            at("a/b/package.json"),
            at("package.json"),
            at("Cargo.toml"),
            at("keeper/Cargo.toml"),
            at("app/package.json"),
        ]);
        let paths: Vec<String> = sorted.iter().map(Manifest::manifest).collect();
        assert_eq!(
            paths,
            [
                "Cargo.toml",
                "package.json",
                "keeper/Cargo.toml",
                "app/package.json",
                "a/b/package.json",
            ]
        );
    }

    #[test]
    fn manifest_kind_dispatches_version_and_name_by_kind() {
        let cargo = "[package]\nname = \"c\"\nversion = \"0.1.0\"\n";
        let npm = r#"{"name": "n", "version": "0.1.0"}"#;
        assert_eq!(
            ManifestKind::Cargo.parse_version(cargo),
            Semver::parse("0.1.0")
        );
        assert_eq!(ManifestKind::Npm.parse_version(npm), Semver::parse("0.1.0"));
        assert_eq!(ManifestKind::Cargo.parse_name(cargo).as_deref(), Some("c"));
        assert_eq!(ManifestKind::Npm.parse_name(npm).as_deref(), Some("n"));
    }

    // ── json_string_at / npm manifest ────────────────────────────────────────

    #[test]
    fn json_string_at_finds_the_root_key_despite_nested_namesakes() {
        let text = r#"{
  "name": "x",
  "engines": { "node": ">=18" },
  "publishConfig": { "version": "9.9.9" },
  "version": "1.2.3"
}"#;
        let range = json_string_at(text, &["version"]).expect("root version found");
        assert_eq!(&text[range], "1.2.3");
    }

    #[test]
    fn json_string_at_ignores_an_array_element_of_the_same_name() {
        let text = r#"{"workspaces": ["version"], "version": "1.0.0"}"#;
        let range = json_string_at(text, &["version"]).unwrap();
        assert_eq!(&text[range], "1.0.0");
    }

    #[test]
    fn json_string_at_survives_an_escaped_quote_elsewhere() {
        let text = r#"{"description": "a \"quoted\" word", "version": "2.0.0"}"#;
        let range = json_string_at(text, &["version"]).unwrap();
        assert_eq!(&text[range], "2.0.0");
    }

    #[test]
    fn json_string_at_misses_an_absent_key() {
        assert_eq!(json_string_at(r#"{"name": "x"}"#, &["version"]), None);
    }

    #[test]
    fn json_string_at_finds_a_nested_lockfile_path() {
        let text = r#"{"version": "1.0.0", "packages": {"": {"name": "x", "version": "1.0.0"}}}"#;
        let range = json_string_at(text, &["packages", "", "version"]).unwrap();
        assert_eq!(&text[range], "1.0.0");
    }

    #[test]
    fn npm_version_is_read_from_the_root_not_a_nested_object() {
        let manifest = "{\n    \"name\": \"fixture\",\n    \"publishConfig\": {\n        \"version\": \"9.9.9\"\n    },\n    \"version\": \"0.1.2\"\n}";
        assert_eq!(parse_npm_version(manifest), Semver::parse("0.1.2"));
    }

    #[test]
    fn npm_rewrite_preserves_four_space_indent_and_missing_trailing_newline() {
        let manifest =
            "{\n    \"name\": \"fixture\",\n    \"version\": \"0.1.2\",\n    \"private\": true\n}";
        let out = replace_npm_version(manifest, Semver::parse("0.1.3").unwrap()).unwrap();
        assert_eq!(
            out,
            "{\n    \"name\": \"fixture\",\n    \"version\": \"0.1.3\",\n    \"private\": true\n}"
        );
        assert!(!out.ends_with('\n'));
    }

    #[test]
    fn npm_rewrite_reports_a_missing_version_key() {
        assert_eq!(
            replace_npm_version("{\"name\": \"x\"}", Semver::parse("1.0.0").unwrap()),
            None
        );
    }

    #[test]
    fn a_private_package_with_no_version_reads_as_unreadable() {
        assert_eq!(parse_npm_version(r#"{"name": "x", "private": true}"#), None);
    }

    #[test]
    fn malformed_npm_json_is_unreadable_rather_than_scanned() {
        assert_eq!(parse_npm_version("{\"version\": \"1.0.0\" "), None);
    }

    const NPM_LOCK_V3: &str = r#"{
  "name": "fixture",
  "version": "0.2.7-roll35",
  "lockfileVersion": 3,
  "packages": {
    "": {
      "name": "fixture",
      "version": "0.2.7-roll35"
    },
    "node_modules/leftpad": {
      "version": "1.0.0"
    }
  }
}"#;

    #[test]
    fn npm_lock_both_entries_move_together() {
        assert_eq!(
            parse_npm_lock_version(NPM_LOCK_V3),
            Semver::parse("0.2.7-roll35")
        );
        let next = Semver::parse("0.2.8-roll35").unwrap();
        let out = replace_npm_lock_version(NPM_LOCK_V3, next).unwrap();
        assert_eq!(out, NPM_LOCK_V3.replace("0.2.7-roll35", "0.2.8-roll35"));
        // The dependency's own version is left untouched.
        assert!(out.contains("\"node_modules/leftpad\""));
        assert!(out.contains("\"version\": \"1.0.0\""));
    }

    #[test]
    fn npm_lock_v1_without_a_packages_table_still_rewrites_the_root() {
        let v1 = r#"{"name": "fixture", "version": "0.1.0", "lockfileVersion": 1}"#;
        let out = replace_npm_lock_version(v1, Semver::parse("0.2.0").unwrap()).unwrap();
        assert_eq!(
            out,
            r#"{"name": "fixture", "version": "0.2.0", "lockfileVersion": 1}"#
        );
    }
}
