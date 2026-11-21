//! `eva` — the Evergreen Linux package manager.
//!
//! The resolver, repository index format, transaction journal, and CLI are
//! described in `docs/design/eva-resolver.md`.

pub mod archive;
pub mod build;
pub mod config;
pub mod db;
pub mod model;
pub mod publish;
pub mod repo;
pub mod resolver;
pub mod solver;
pub mod transaction;

pub use build::{build_package, load_recipe, BuildRecipe, BuildResult};
pub use config::{EvaConfig, Paths};
pub use db::{InstalledDb, VerifyKind, VerifyProblem};
pub use model::{
    normalize, Arch, Candidate, Condition, Conflict, DepKind, Dependency, FileEntry, Hooks,
    InstallReason, InstalledPackage, PackageId, PackageMeta, PackageSpec, Provide, Request,
    UpgradeMode, VersionConstraint,
};
pub use repo::{RepoConfig, RepoIndex};
pub use solver::{Action, ConflictExplanation, ResolveError, Resolver, TransactionPlan};
pub use transaction::{Lock, Transaction, TransactionError};

/// Version of the `eva` binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default local package cache.
pub const CACHE_DIR: &str = "/var/cache/eva";

/// Default database directory.
pub const DB_DIR: &str = "/var/lib/eva";
