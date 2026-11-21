//! Package and bootstrap recipe types.
//!
//! These mirror the TOML layout used by `eva` and `evergreen-bootstrap`.

use std::path::PathBuf;

/// A full package or bootstrap recipe.
#[derive(Debug, Clone, Default)]
pub struct Recipe {
    pub package: PackageMeta,
    pub dependencies: Vec<Dependency>,
    pub build: BuildSpec,
}

/// Identity and descriptive metadata for a package.
#[derive(Debug, Clone, Default)]
pub struct PackageMeta {
    pub name: String,
    pub version: String,
    pub release: u32,
    pub arch: String,
    pub description: String,
    pub license: String,
    pub maintainer: String,
}

/// A dependency on another package or virtual name.
#[derive(Debug, Clone, Default)]
pub struct Dependency {
    pub name: String,
    /// Version constraint such as `>=1.2` or `=2.0.1`.
    pub constraint: String,
    pub optional: bool,
}

/// Build instructions for a recipe.
#[derive(Debug, Clone, Default)]
pub struct BuildSpec {
    pub source: Option<String>,
    pub sha256: Option<String>,
    pub configure: Option<String>,
    pub build_deps: Vec<String>,
    pub patches: Vec<PathBuf>,
    /// Bootstrap stage (`0` or `1`); `None` for normal eva packages.
    pub stage: Option<u8>,
}
