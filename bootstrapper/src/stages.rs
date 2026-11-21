//! Bootstrap stages and build state.

use serde::{Deserialize, Serialize};

/// The three bootstrap stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Minimal base rootfs built from pinned upstream sources.
    Stage0,
    /// Evergreen components built as `.eva` packages on top of stage0.
    Stage1,
    /// Assemble, smoke-test, and publish artifacts.
    Assemble,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Stage0 => "stage0",
            Stage::Stage1 => "stage1",
            Stage::Assemble => "assemble",
        }
    }
}

/// Status of a stage or package within a stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StageStatus {
    #[default]
    Pending,
    Building,
    Done,
    Failed,
}

impl std::fmt::Display for StageStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Pending => "Pending",
            Self::Building => "Building",
            Self::Done => "Done",
            Self::Failed => "Failed",
        };
        f.write_str(text)
    }
}
