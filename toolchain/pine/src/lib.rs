//! `pine` — the Evergreen Linux init system (PID 1).
//!
//! See `docs/design/pine-internals.md` for the full architecture.

pub mod cgroup;
pub mod config;
pub mod control;
pub mod dbus;
pub mod graph;
pub mod log;
pub mod manager;
pub mod mount;
pub mod process;
pub mod shutdown;
pub mod unit;

pub use config::{Config, Paths};
pub use manager::{Command, Manager, ManagerHandle, Response, UnitStatus};
pub use unit::{ServiceType, Unit, UnitState};

/// Version of the `pine` binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Units are named like `sshd.service` or `graphical.target`.
pub type UnitName = String;
