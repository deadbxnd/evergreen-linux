//! `evergreen-installer` library: configuration, disk planning and installation.

pub mod config;
pub mod disk;
#[cfg(feature = "gtk")]
pub mod gui;
pub mod system;
pub mod ui;

pub use config::{Edition, Encryption, InstallConfig, PartitionMode};
pub use disk::{CommandSpec, DiskPlan, DryRunRunner, Runner, SystemRunner};
pub use system::{install, InstallReport};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
