//! Core data model for `eva`.
//!
//! Types here are shared by the repository index, the installed database, the
//! solver, and the transaction planner. Version constraints are parsed from
//! the human-readable strings used in `META.toml` and index files.

use std::fmt;
use std::str::FromStr;

use anyhow::anyhow;
use evergreen_common::{RepoId, Version};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

/// Normalize a package name to lowercase, trimming surrounding whitespace.
pub fn normalize(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Target architecture of a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Arch {
    X86_64,
    Aarch64,
    NoArch,
}

impl Arch {
    pub fn parse(input: &str) -> anyhow::Result<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "x86_64" | "amd64" => Ok(Self::X86_64),
            "aarch64" | "arm64" => Ok(Self::Aarch64),
            "noarch" | "any" | "all" => Ok(Self::NoArch),
            other => Err(anyhow!("unknown architecture {other:?}")),
        }
    }

    /// Architecture of the machine running eva.
    pub fn host() -> Self {
        match std::env::consts::ARCH {
            "x86_64" => Self::X86_64,
            "aarch64" => Self::Aarch64,
            _ => Self::NoArch,
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::X86_64 => f.write_str("x86_64"),
            Self::Aarch64 => f.write_str("aarch64"),
            Self::NoArch => f.write_str("noarch"),
        }
    }
}

/// Uniquely identifies one package build in one repository.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PackageId {
    pub name: String,
    pub version: Version,
    pub arch: Arch,
    pub repo: RepoId,
}

impl PackageId {
    pub fn key(&self) -> String {
        format!(
            "{}={}-{}@{}",
            self.name, self.version, self.arch, self.repo.0
        )
    }
}

/// A version constraint such as `>=1.2`, `=2.0`, `1.2.*`, or `^1.4`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VersionConstraint {
    Any,
    Eq(Version),
    Ne(Version),
    Gt(Version),
    Ge(Version),
    Lt(Version),
    Le(Version),
    Caret(Version),
    Tilde(Version),
    Wildcard(String),
    All(Vec<VersionConstraint>),
    AnyOf(Vec<VersionConstraint>),
}

impl VersionConstraint {
    pub fn any() -> Self {
        Self::Any
    }

    pub fn matches(&self, version: &Version) -> bool {
        match self {
            Self::Any => true,
            Self::Eq(wanted) => version == wanted || wildcard_matches(&wanted.to_string(), version),
            Self::Ne(wanted) => !Self::Eq(wanted.clone()).matches(version),
            Self::Gt(wanted) => version > wanted,
            Self::Ge(wanted) => version >= wanted,
            Self::Lt(wanted) => version < wanted,
            Self::Le(wanted) => version <= wanted,
            Self::Caret(base) => version >= base && version < &caret_upper_bound(base),
            Self::Tilde(base) => version >= base && version < &tilde_upper_bound(base),
            Self::Wildcard(pattern) => wildcard_matches(pattern, version),
            Self::All(parts) => parts.iter().all(|part| part.matches(version)),
            Self::AnyOf(parts) => parts.iter().any(|part| part.matches(version)),
        }
    }

    /// Constraint requiring an upgrade relative to `version`.
    pub fn at_least(version: &Version) -> Self {
        Self::Ge(version.clone())
    }

    pub fn is_any(&self) -> bool {
        matches!(self, Self::Any)
    }
}

impl fmt::Display for VersionConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => f.write_str("*"),
            Self::Eq(v) => write!(f, "={v}"),
            Self::Ne(v) => write!(f, "!={v}"),
            Self::Gt(v) => write!(f, ">{v}"),
            Self::Ge(v) => write!(f, ">={v}"),
            Self::Lt(v) => write!(f, "<{v}"),
            Self::Le(v) => write!(f, "<={v}"),
            Self::Caret(v) => write!(f, "^{v}"),
            Self::Tilde(v) => write!(f, "~{v}"),
            Self::Wildcard(pattern) => f.write_str(pattern),
            Self::All(parts) => {
                let text: Vec<String> = parts.iter().map(ToString::to_string).collect();
                f.write_str(&text.join(","))
            }
            Self::AnyOf(parts) => {
                let text: Vec<String> = parts.iter().map(ToString::to_string).collect();
                f.write_str(&text.join("|"))
            }
        }
    }
}

impl FromStr for VersionConstraint {
    type Err = anyhow::Error;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        parse_constraint(input).map_err(anyhow::Error::msg)
    }
}

impl Serialize for VersionConstraint {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for VersionConstraint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_constraint(&text).map_err(de::Error::custom)
    }
}

fn parse_constraint(input: &str) -> Result<VersionConstraint, String> {
    let text = input.trim();
    if text.is_empty() || text == "*" {
        return Ok(VersionConstraint::Any);
    }

    let alternatives: Vec<&str> = text.split('|').map(str::trim).collect();
    if alternatives.len() > 1 {
        let parts = alternatives
            .iter()
            .map(|part| parse_constraint(part))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(VersionConstraint::AnyOf(parts));
    }

    let conjunction: Vec<&str> = text
        .split([',', ' '])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if conjunction.len() > 1 {
        let parts = conjunction
            .iter()
            .map(|part| parse_constraint(part))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(VersionConstraint::All(parts));
    }

    let token = conjunction[0];
    let (op, value) = if let Some(value) = token.strip_prefix("!=") {
        ("!=", value)
    } else if let Some(value) = token.strip_prefix(">=") {
        (">=", value)
    } else if let Some(value) = token.strip_prefix("<=") {
        ("<=", value)
    } else if let Some(value) = token.strip_prefix("==") {
        ("=", value)
    } else if let Some(value) = token.strip_prefix('=') {
        ("=", value)
    } else if let Some(value) = token.strip_prefix('>') {
        (">", value)
    } else if let Some(value) = token.strip_prefix('<') {
        ("<", value)
    } else if let Some(value) = token.strip_prefix('^') {
        ("^", value)
    } else if let Some(value) = token.strip_prefix('~') {
        ("~", value)
    } else {
        ("=", token)
    };

    if value.ends_with(".*") || value.ends_with('*') {
        let pattern = value.trim_end_matches('*').trim_end_matches('.');
        return Ok(VersionConstraint::Wildcard(format!("{pattern}.*")));
    }

    let version = Version::parse(value).map_err(|err| err.to_string())?;
    Ok(match op {
        "=" => VersionConstraint::Eq(version),
        "!=" => VersionConstraint::Ne(version),
        ">" => VersionConstraint::Gt(version),
        ">=" => VersionConstraint::Ge(version),
        "<" => VersionConstraint::Lt(version),
        "<=" => VersionConstraint::Le(version),
        "^" => VersionConstraint::Caret(version),
        "~" => VersionConstraint::Tilde(version),
        other => return Err(format!("unknown constraint operator {other:?}")),
    })
}

fn wildcard_matches(pattern: &str, version: &Version) -> bool {
    let text = version.to_string();
    let prefix = pattern.trim_end_matches('*');
    if pattern.ends_with('*') {
        text.starts_with(prefix)
    } else {
        text == prefix
    }
}

fn first_number(version: &Version) -> u64 {
    version
        .to_string()
        .split('.')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn second_number(version: &Version) -> Option<u64> {
    version
        .to_string()
        .split('.')
        .nth(1)
        .and_then(|s| s.parse().ok())
}

fn caret_upper_bound(base: &Version) -> Version {
    let major = first_number(base);
    if major == 0 {
        let minor = second_number(base).unwrap_or(0);
        Version::parse(&format!("0.{}", minor + 1)).unwrap_or_else(|_| base.clone())
    } else {
        Version::parse(&format!("{}.0", major + 1)).unwrap_or_else(|_| base.clone())
    }
}

fn tilde_upper_bound(base: &Version) -> Version {
    let major = first_number(base);
    let minor = second_number(base).unwrap_or(0);
    Version::parse(&format!("{major}.{}", minor + 1)).unwrap_or_else(|_| base.clone())
}

/// Dependency kind, from required to advisory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DepKind {
    Required,
    Optional,
    Recommend,
    Suggest,
}

/// A dependency on a real or virtual package.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Dependency {
    pub name: String,
    pub constraint: VersionConstraint,
    pub kind: DepKind,
}

impl Dependency {
    pub fn required(name: impl Into<String>, constraint: VersionConstraint) -> Self {
        Self {
            name: normalize(&name.into()),
            constraint,
            kind: DepKind::Required,
        }
    }
}

/// Condition controlling an optional dependency.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Condition {
    Always,
    Feature(String),
    PackagePresent(String),
}

impl Condition {
    pub fn parse(input: &str) -> Self {
        let text = input.trim();
        if let Some(feature) = text.strip_prefix("feature:") {
            Self::Feature(feature.trim().to_string())
        } else if let Some(package) = text.strip_prefix("package:") {
            Self::PackagePresent(normalize(package.trim()))
        } else {
            Self::Always
        }
    }
}

/// A conditional optional dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionalDep {
    pub dependency: Dependency,
    pub condition: Condition,
}

/// A package incompatibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub name: String,
    pub constraint: VersionConstraint,
}

/// A virtual package provided by a real package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provide {
    pub name: String,
    pub version: Option<Version>,
}

/// A file owned by a package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Path relative to the system root, e.g. `usr/bin/eva`.
    pub path: String,
    pub sha256: String,
    #[serde(default = "default_mode")]
    pub mode: u32,
}

fn default_mode() -> u32 {
    0o644
}

/// A symbolic link owned by a package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymlinkEntry {
    /// Link path relative to the system root.
    pub path: String,
    /// Link target (relative to the link's directory).
    pub target: String,
    #[serde(default = "default_mode")]
    pub mode: u32,
}

/// Maintenance scripts shipped inside a `.eva` archive.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hooks {
    pub pre_install: Option<String>,
    pub post_install: Option<String>,
    pub pre_remove: Option<String>,
    pub post_remove: Option<String>,
}

/// Descriptive metadata for a candidate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageMeta {
    pub description: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub license: Vec<String>,
    #[serde(default)]
    pub maintainer: String,
    #[serde(default)]
    pub size_installed: u64,
    #[serde(default)]
    pub size_download: u64,
    #[serde(default)]
    pub checksum: String,
}

/// One installable package version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub id: PackageId,
    pub meta: PackageMeta,
    #[serde(default)]
    pub deps: Vec<Dependency>,
    #[serde(default)]
    pub opt_deps: Vec<OptionalDep>,
    #[serde(default)]
    pub recommends: Vec<Dependency>,
    #[serde(default)]
    pub suggests: Vec<Dependency>,
    #[serde(default)]
    pub conflicts: Vec<Conflict>,
    #[serde(default)]
    pub provides: Vec<Provide>,
    #[serde(default)]
    pub replaces: Vec<String>,
    #[serde(default)]
    pub files: Vec<FileEntry>,
    #[serde(default)]
    pub symlinks: Vec<SymlinkEntry>,
    #[serde(default)]
    pub hooks: Hooks,
    /// Relative filename inside the repository (derived when absent).
    #[serde(default)]
    pub archive: String,
}

impl Candidate {
    pub fn name(&self) -> &str {
        &self.id.name
    }

    pub fn version(&self) -> &Version {
        &self.id.version
    }

    /// Package names this candidate can satisfy, including virtual provides.
    pub fn satisfied_names(&self) -> Vec<String> {
        let mut names = vec![self.id.name.clone()];
        names.extend(self.provides.iter().map(|provide| normalize(&provide.name)));
        names.sort();
        names.dedup();
        names
    }

    /// Version advertised for a name this candidate satisfies.
    pub fn provided_version(&self, name: &str) -> Option<&Version> {
        let name = normalize(name);
        if name == self.id.name {
            return Some(&self.id.version);
        }
        match self
            .provides
            .iter()
            .find(|provide| normalize(&provide.name) == name)
        {
            Some(provide) => Some(provide.version.as_ref().unwrap_or(&self.id.version)),
            None => None,
        }
    }

    pub fn archive_name(&self) -> String {
        if !self.archive.is_empty() {
            return self.archive.clone();
        }
        format!(
            "{}-{}-{}.{}.eva",
            self.id.name, self.id.version, 0, self.id.arch
        )
    }
}

/// Why a package is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstallReason {
    Explicit,
    Dependency,
}

/// An installed package record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledPackage {
    pub candidate: Candidate,
    pub reason: InstallReason,
    pub installed_at: String,
}

/// Upgrade mode for a resolve request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum UpgradeMode {
    #[default]
    None,
    All,
    Specific(Vec<String>),
}

/// A requested package and optional constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSpec {
    pub name: String,
    pub constraint: VersionConstraint,
}

impl PackageSpec {
    pub fn parse(input: &str) -> anyhow::Result<Self> {
        // Split on the first constraint operator, but keep `:` in versions.
        let mut index = None;
        for (position, ch) in input.char_indices() {
            if matches!(ch, '=' | '<' | '>' | '^' | '~' | '*') {
                index = Some(position);
                break;
            }
        }
        let (name, constraint) = match index {
            Some(position) => {
                let name = &input[..position];
                let constraint = &input[position..];
                (
                    name,
                    parse_constraint(constraint).map_err(anyhow::Error::msg)?,
                )
            }
            None => (input, VersionConstraint::Any),
        };
        if name.trim().is_empty() {
            return Err(anyhow!("empty package name"));
        }
        Ok(Self {
            name: normalize(name),
            constraint,
        })
    }
}

/// A complete resolve request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    pub install: Vec<PackageSpec>,
    pub remove: Vec<String>,
    pub upgrade: UpgradeMode,
    pub reinstall: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_constraints() {
        assert!(VersionConstraint::Any.matches(&Version::parse("1.0").unwrap()));
        assert!(parse_constraint(">=1.2")
            .unwrap()
            .matches(&Version::parse("1.3").unwrap()));
        assert!(!parse_constraint(">=1.2")
            .unwrap()
            .matches(&Version::parse("1.1").unwrap()));
        assert!(parse_constraint("1.2.*")
            .unwrap()
            .matches(&Version::parse("1.2.9").unwrap()));
        assert!(!parse_constraint("1.2.*")
            .unwrap()
            .matches(&Version::parse("1.3").unwrap()));
        assert!(parse_constraint("^1.2.0")
            .unwrap()
            .matches(&Version::parse("1.9.0").unwrap()));
        assert!(!parse_constraint("^1.2.0")
            .unwrap()
            .matches(&Version::parse("2.0.0").unwrap()));
        assert!(parse_constraint("~1.2")
            .unwrap()
            .matches(&Version::parse("1.2.8").unwrap()));
        assert!(!parse_constraint("~1.2")
            .unwrap()
            .matches(&Version::parse("1.3").unwrap()));
    }

    #[test]
    fn parses_compound_constraints() {
        let constraint = parse_constraint(">=1.0,<2.0").unwrap();
        assert!(constraint.matches(&Version::parse("1.5").unwrap()));
        assert!(!constraint.matches(&Version::parse("2.0").unwrap()));
        let either = parse_constraint("=1.0|=2.0").unwrap();
        assert!(either.matches(&Version::parse("2.0").unwrap()));
        assert!(!either.matches(&Version::parse("3.0").unwrap()));
    }

    #[test]
    fn parses_package_specs() {
        let spec = PackageSpec::parse("foo>=1.2").unwrap();
        assert_eq!(spec.name, "foo");
        assert!(spec.constraint.matches(&Version::parse("1.3").unwrap()));
        let spec = PackageSpec::parse("Bar").unwrap();
        assert_eq!(spec.name, "bar");
        assert!(spec.constraint.is_any());
    }

    #[test]
    fn round_trips_constraint_serde() {
        let constraint = parse_constraint(">=1.2,<2.0").unwrap();
        let json = serde_json::to_string(&constraint).unwrap();
        let parsed: VersionConstraint = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, constraint);
    }
}
