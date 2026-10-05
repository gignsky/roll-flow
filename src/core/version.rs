//! Crate-version policy: read, compare, and bump the `version` in `Cargo.toml`.
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
//! - `.github/workflows/release-check.yml` — the tag must match `Cargo.toml`.
//!
//! Pure logic only: nothing here prints or prompts. Repos without a
//! `Cargo.toml` (the dotfiles repo roll-flow was built for) yield
//! `VersionStatus::NotApplicable` and every caller silently skips.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use crate::core::git;
use crate::error::RfError;

/// The file the crate version is read from and written to.
pub const VERSION_FILE: &str = "Cargo.toml";

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
    /// A `Cargo.toml` exists but its version could not be read or parsed.
    Unreadable,
    /// Source still carries a dev marker — a roll's `-roll<N>`, or rolling's
    /// own `-dev`. Its own status rather than folded into `Unchanged`,
    /// because the fix is different: strip the marker (what `rf graduate`
    /// does for a roll, and what a *final* `rf promote` does for rolling)
    /// rather than raise the numbers.
    DevVersion,
    /// No `Cargo.toml` on either side, or the gate is disabled in config.
    NotApplicable,
}

/// The two versions being compared plus the verdict.
#[derive(Debug, Clone)]
pub struct VersionCheck {
    pub head: Option<Semver>,
    pub base: Option<Semver>,
    pub status: VersionStatus,
}

impl VersionCheck {
    pub fn not_applicable() -> VersionCheck {
        VersionCheck {
            head: None,
            base: None,
            status: VersionStatus::NotApplicable,
        }
    }

    /// True when the check neither blocks nor has anything to report.
    pub fn is_satisfied(&self) -> bool {
        matches!(
            self.status,
            VersionStatus::Ok | VersionStatus::NotApplicable
        )
    }
}

/// Extract `package.version` from `Cargo.toml` text.
///
/// Parsed with the `toml` crate rather than a regex so section nesting is
/// honoured — a `version = ...` under `[dependencies.foo]` can never be
/// mistaken for the package version.
pub fn parse_version(cargo_toml: &str) -> Option<Semver> {
    let doc: toml::Value = toml::from_str(cargo_toml).ok()?;
    let raw = doc.get("package")?.get("version")?.as_str()?;
    Semver::parse(raw)
}

/// Read the crate version from the working tree.
pub fn read_version(repo: &Path) -> Result<Option<Semver>, RfError> {
    let path = repo.join(VERSION_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(parse_version(&text))
}

/// Read the crate version at each of `refs`, keyed by ref.
///
/// One `git cat-file --batch` for the whole set, because the caller is a table
/// that wants a version per row on every reload. Refs without a `Cargo.toml`,
/// or whose manifest carries a version this crate will not parse, are simply
/// absent from the map — the column renders those as a dash, and a repo with no
/// manifest at all yields an empty map rather than an error. Losing a version is
/// never worth failing a reload over.
pub fn versions_at(repo: &Path, refs: &[String]) -> HashMap<String, Semver> {
    let specs: Vec<String> = refs.iter().map(|r| format!("{r}:{VERSION_FILE}")).collect();
    let Ok(blobs) = git::show_files_at_refs(repo, &specs) else {
        return HashMap::new();
    };
    refs.iter()
        .zip(specs.iter())
        .filter_map(|(r, spec)| {
            let version = parse_version(blobs.get(spec)?)?;
            Some((r.clone(), version))
        })
        .collect()
}

/// Compare the version on `source_ref` to the one on `target_ref`, the same
/// comparison `version-bump-check.yml` makes between a PR head and its base.
pub fn check(repo: &Path, source_ref: &str, target_ref: &str) -> Result<VersionCheck, RfError> {
    let head_text = git::show_file_at_ref(repo, source_ref, VERSION_FILE)?;
    let base_text = git::show_file_at_ref(repo, target_ref, VERSION_FILE)?;

    // No manifest on either side: this repo doesn't version this way. Skip.
    if head_text.is_none() && base_text.is_none() {
        return Ok(VersionCheck::not_applicable());
    }

    let head = head_text.as_deref().and_then(parse_version);
    let base = base_text.as_deref().and_then(parse_version);

    let status = match (head, base) {
        // Checked ahead of the comparison: a dev version can be numerically
        // above its target and still must not promote — `0.2.5-roll9 > 0.2.4`
        // is true, and shipping a `-roll9` (or un-finalized `-dev`) version to
        // stable (and tagging it `v0.2.5-roll9`) is exactly what this gate
        // exists to stop.
        (Some(h), _) if h.has_marker() => VersionStatus::DevVersion,
        // A manifest added by this very branch is a bump from nothing.
        (Some(_), None) if base_text.is_none() => VersionStatus::Ok,
        (Some(h), Some(b)) if h > b => VersionStatus::Ok,
        (Some(h), Some(b)) if h == b => VersionStatus::Unchanged,
        (Some(_), Some(_)) => VersionStatus::Lower,
        _ => VersionStatus::Unreadable,
    };

    Ok(VersionCheck { head, base, status })
}

/// Like [`check`], but against an already-resolved `head` rather than reading
/// `source_ref` fresh.
///
/// A per-roll promotion step needs to gate on the *finalized* (marker-
/// stripped) version a graduation commit will carry once landed on stable,
/// not the raw `-dev` value still sitting in that commit's `Cargo.toml` —
/// `check` alone would report `DevVersion` on every single per-roll
/// promotion step otherwise, since the raw value never actually reaches
/// stable; finalizing is exactly what landing it does.
///
/// Kept as its own small function rather than `check` delegating to it: the
/// two differ in exactly when `NotApplicable` fires (`check`'s source-missing
/// case is about the raw file being absent, not a parse failure), and
/// collapsing that distinction to save a few lines is not worth risking here.
pub fn check_against(
    repo: &Path,
    head: Option<Semver>,
    target_ref: &str,
) -> Result<VersionCheck, RfError> {
    let base_text = git::show_file_at_ref(repo, target_ref, VERSION_FILE)?;
    if head.is_none() && base_text.is_none() {
        return Ok(VersionCheck::not_applicable());
    }
    let base = base_text.as_deref().and_then(parse_version);
    let status = match (head, base) {
        (Some(h), _) if h.has_marker() => VersionStatus::DevVersion,
        (Some(_), None) if base_text.is_none() => VersionStatus::Ok,
        (Some(h), Some(b)) if h > b => VersionStatus::Ok,
        (Some(h), Some(b)) if h == b => VersionStatus::Unchanged,
        (Some(_), Some(_)) => VersionStatus::Lower,
        _ => VersionStatus::Unreadable,
    };
    Ok(VersionCheck { head, base, status })
}

// ── Write ───────────────────────────────────────────────────────────────────

/// Rewrite the package version in the working tree's `Cargo.toml`.
///
/// Deliberately line-based rather than a `toml` serialize round-trip: that
/// would reorder keys and strip every comment in the manifest. We locate the
/// `[package]` table and replace the first `version = "..."` inside it.
pub fn write_version(repo: &Path, new: Semver) -> Result<(), RfError> {
    let path = repo.join(VERSION_FILE);
    let text = std::fs::read_to_string(&path)?;
    let rewritten = replace_package_version(&text, new).ok_or_else(|| {
        RfError::Git(format!(
            "could not find a `version` key in the [package] table of {}",
            path.display()
        ))
    })?;
    std::fs::write(&path, rewritten)?;
    Ok(())
}

/// Pure half of [`write_version`], so the formatting-preservation behaviour can
/// be unit-tested without touching a repo. Returns `None` if no version key was
/// found in `[package]`.
pub fn replace_package_version(text: &str, new: Semver) -> Option<String> {
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

/// True for a `version = ...` assignment (allowing whitespace around `=`), the
/// same shape the workflows' `^version[[:space:]]*=` grep matches.
fn is_version_key(trimmed: &str) -> bool {
    let rest = match trimmed.strip_prefix("version") {
        Some(rest) => rest,
        None => return false,
    };
    rest.trim_start().starts_with('=')
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
        assert_eq!(parse_version(manifest), Semver::parse("0.1.2"));
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
        let out = replace_package_version(manifest, Semver::parse("0.1.3").unwrap()).unwrap();
        assert!(out.contains("version = \"0.1.3\""));
        assert!(out.contains("name = \"roll-flow\"  # the binary"));
        assert!(out.contains("edition = \"2021\""));
        // The dependency version must be untouched.
        assert!(out.contains("toml = \"1.1.2\""));
        assert_eq!(parse_version(&out), Semver::parse("0.1.3"));
    }

    #[test]
    fn rewrite_ignores_version_keys_outside_package() {
        let manifest = "[dependencies]\n\
                        version = \"9.9.9\"\n\
                        \n\
                        [package]\n\
                        version = \"0.1.2\"\n";
        let out = replace_package_version(manifest, Semver::parse("0.2.0").unwrap()).unwrap();
        assert!(out.contains("version = \"9.9.9\""));
        assert!(out.contains("version = \"0.2.0\""));
    }

    #[test]
    fn rewrite_reports_missing_version_key() {
        let manifest = "[package]\nname = \"x\"\n";
        assert_eq!(
            replace_package_version(manifest, Semver::parse("1.0.0").unwrap()),
            None
        );
    }

    #[test]
    fn not_applicable_is_satisfied() {
        assert!(VersionCheck::not_applicable().is_satisfied());
    }
}
