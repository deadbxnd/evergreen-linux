//! Repository identity and metadata.

use serde::{Deserialize, Serialize};

/// Stable identifier for a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RepoId(pub u32);

/// Release branch a repository belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Branch {
    Stable,
    Edge,
}

/// A configured package repository.
#[derive(Debug, Clone)]
pub struct Repo {
    pub id: RepoId,
    pub name: String,
    pub url: String,
    pub branch: Branch,
    /// Lower values are preferred when versions tie.
    pub priority: i32,
}
