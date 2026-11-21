//! Shared types and helpers used across the Evergreen toolchain.

pub mod recipe;
pub mod repo;
pub mod version;

pub use recipe::{BuildSpec, Dependency, PackageMeta, Recipe};
pub use repo::{Branch, Repo, RepoId};
pub use version::{Version, VersionParseError};

/// Human-readable distribution name.
pub const DISTRO_NAME: &str = "Evergreen Linux";

/// Distribution version (CalVer, e.g. `2026.11`).
pub const DISTRO_VERSION: &str = env!("CARGO_PKG_VERSION");
