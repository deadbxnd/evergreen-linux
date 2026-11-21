//! `evergreen-build` — recipe loading and package/ISO orchestration.

pub mod iso;
pub mod publish;
pub mod recipe;
pub mod rootfs;

pub use iso::{build_iso, IsoOptions, IsoReport};
pub use publish::{publish, validate_recipes, PublishOptions, PublishReport, RecipeReport};
pub use recipe::{load_recipe, RecipeError};
pub use rootfs::{assemble, AssembleOptions, AssembleReport, Edition};

/// Version of the build tool.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
