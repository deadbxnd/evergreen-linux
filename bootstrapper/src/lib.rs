//! `evergreen-bootstrap` — builds the minimal Evergreen base rootfs from pinned
//! upstream sources, then assembles it into images and ISOs.
//!
//! See `docs/design/evergreen-bootstrapper.md` for the full design.

pub mod build;
pub mod config;
pub mod recipe;
pub mod stage1;
pub mod stages;

pub use config::BootstrapConfig;
pub use stages::{Stage, StageStatus};

/// Version of the bootstrapper.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default working directory for sources, sysroot, and state.
pub const DEFAULT_WORKDIR: &str = "/opt/evergreen";
