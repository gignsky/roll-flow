//! Crate-version policy: read, compare, and bump the `version` in `Cargo.toml`.
//!
//! This mirrors, by construction, the three CI workflows that own the same
//! policy today, so `rf` and GitHub Actions can never disagree:
//!
//! - `.github/workflows/version-bump-check.yml` — a promotion must raise the
//!   version strictly above the branch it targets.
//! - `.github/workflows/tag-on-main.yml` — a version change on stable gets an
//!   annotated `vX.Y.Z` tag, created idempotently.
//! - `.github/workflows/release-check.yml` — the tag must match `Cargo.toml`.
//!
//! Pure logic only: nothing here prints or prompts. Repos without a
//! `Cargo.toml` (the dotfiles repo roll-flow was built for) yield
//! `VersionStatus::NotApplicable` and every caller silently skips.

use std::fmt;
use std::path::Path;

use crate::core::git;
use crate::error::RfError;

/// The file the crate version is read from and written to.
pub const VERSION_FILE: &str = "Cargo.toml";

// ── Semver ──────────────────────────────────────────────────────────────────

/// A three-field version. Derived `Ord` compares major, then minor, then patch,
/// which is exactly the ordering `sort -V` gives the workflows for the
/// `X.Y.Z` values this project uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Semver {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Semver {
    /// Parse `X.Y.Z`. Any pre-release/build suffix on the patch field is
    /// rejected rather than silently dropped — this project has never used one,
    /// and quietly ignoring it could let a lower version read as higher.
    pub fn parse(raw: &str) -> Option<Semver> {
        let mut parts = raw.trim().split('.');
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
        })
    }

    /// The next version at `level`, zeroing the fields below it.
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
            },
            BumpLevel::Major => Semver {
                major: self.major + 1,
                minor: 0,
                patch: 0,
            },
        }
    }

    /// The release tag for this version, e.g. `v0.1.3`. Matches the `v$crate`
    /// form `tag-on-main.yml` builds.
    pub fn tag(self) -> String {
        format!("v{self}")
    }
}

impl fmt::Display for Semver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
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
        // A manifest added by this very branch is a bump from nothing.
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
    fn parses_and_orders_versions() {
        assert_eq!(
            Semver::parse("0.1.2"),
            Some(Semver {
                major: 0,
                minor: 1,
                patch: 2
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
