//! `evergreen-bootstrap` — command-line entry point.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use evergreen_bootstrap::build::{build_stage0, clean_stage0, load_state, preflight, Stage0Paths};
use evergreen_bootstrap::config::{Arch, BootstrapConfig, Libc};
use evergreen_bootstrap::stage1::{build_stage1, clean_stage1, load_state as load_stage1_state};
use evergreen_bootstrap::VERSION;
use evergreen_build::rootfs::{assemble as assemble_rootfs, AssembleOptions, Edition};
use evergreen_build::{build_iso, IsoOptions};

#[derive(Debug, Parser)]
#[command(
    name = "evergreen-bootstrap",
    version,
    about = "Evergreen Linux bootstrapper"
)]
struct Cli {
    /// Path to bootstrapper.toml.
    #[arg(long, default_value = "bootstrapper.toml")]
    config: PathBuf,

    /// C library edition (overrides config).
    #[arg(long, global = true)]
    libc: Option<String>,

    /// Target architecture (overrides config).
    #[arg(long, global = true)]
    arch: Option<String>,

    /// Build work directory (overrides config).
    #[arg(long, global = true)]
    workdir: Option<PathBuf>,

    /// Parallel jobs (overrides config).
    #[arg(long, global = true)]
    jobs: Option<usize>,

    /// Verbose output.
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Verify host toolchain and prerequisites.
    Preflight,
    /// Build the minimal stage 0 rootfs from pinned sources.
    Stage0 {
        /// Directory containing stage 0 TOML recipes.
        #[arg(long)]
        recipes: Option<PathBuf>,
        /// Skip packages recorded as successfully built.
        #[arg(long)]
        resume: bool,
    },
    /// Build Stage 1 Evergreen components as .eva packages.
    Stage1 {
        /// Directory containing stage 1 TOML recipes.
        #[arg(long)]
        recipes: Option<PathBuf>,
        /// Skip packages recorded as successfully built.
        #[arg(long)]
        resume: bool,
    },
    /// Assemble a base image from stage 0 and stage 1 packages.
    Assemble {
        /// Stage 0 tarball (defaults to the last build's artifact).
        #[arg(long)]
        stage0: Option<PathBuf>,
        /// Directory of .eva packages (repeatable).
        #[arg(long = "packages")]
        package_dirs: Vec<PathBuf>,
        /// Assembled rootfs output directory.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Product edition.
        #[arg(long, default_value = "desktop")]
        edition: String,
    },
    /// Build a live ISO from an assembled rootfs.
    Iso {
        /// Rootfs to package (defaults to the assembled edition rootfs).
        #[arg(long)]
        rootfs: Option<PathBuf>,
        /// ISO output path.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Product edition.
        #[arg(long, default_value = "desktop")]
        edition: String,
        /// Kernel image override.
        #[arg(long)]
        kernel: Option<PathBuf>,
        /// Initramfs image override.
        #[arg(long)]
        initramfs: Option<PathBuf>,
    },
    /// Run preflight, stage 0, and stage 1 in sequence.
    All,
    /// Publish artifacts to the configured mirror.
    Publish,
    /// Remove build state and outputs.
    Clean,
    /// Show build progress.
    Status,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("evergreen-bootstrap: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let mut config = BootstrapConfig::load(&cli.config)?;

    if let Some(libc) = &cli.libc {
        config.libc = Libc::parse(libc)?;
    }
    if let Some(arch) = &cli.arch {
        config.arch = Arch::parse(arch)?;
    }
    if let Some(workdir) = &cli.workdir {
        config.workdir = workdir.clone();
    }
    if let Some(jobs) = cli.jobs {
        config.jobs = jobs;
    }

    match cli.command {
        Command::Preflight => {
            let checks = preflight();
            let mut failed = false;
            for check in &checks {
                if check.ok {
                    println!("ok       {:<10} {}", check.name, check.message);
                } else {
                    println!("MISSING  {:<10} {}", check.name, check.message);
                    failed = true;
                }
            }
            if failed {
                bail!("host is missing required build tools");
            }
        }
        Command::Stage0 { recipes, resume } => {
            let recipes = recipes.unwrap_or_else(default_recipes_dir);
            let report = build_stage0(&config, &recipes, resume)?;
            println!(
                "stage 0 complete: built {}, resumed {}",
                report.built.len(),
                report.skipped.len()
            );
            println!("tarball: {}", report.tarball.display());
        }
        Command::Stage1 { recipes, resume } => {
            let recipes = recipes.unwrap_or_else(default_stage1_recipes_dir);
            let report = build_stage1(&config, &recipes, resume)?;
            println!(
                "stage 1 complete: built {}, resumed {}, packages {}",
                report.built.len(),
                report.skipped.len(),
                report.packages.len()
            );
            for package in &report.packages {
                println!("  {}", package.display());
            }
        }
        Command::Assemble {
            stage0,
            package_dirs,
            out,
            edition,
        } => {
            let edition: Edition = edition.parse().map_err(anyhow::Error::msg)?;
            let stage0 = stage0.unwrap_or_else(|| default_stage0_tarball(&config));
            let package_dirs = if package_dirs.is_empty() {
                vec![default_package_dir(&config)]
            } else {
                package_dirs
            };
            let out = out.unwrap_or_else(|| default_rootfs(&config, edition));
            let report = assemble_rootfs(&AssembleOptions {
                stage0_tarball: stage0,
                package_dirs,
                output: out,
                edition,
            })?;
            println!(
                "assembled {} ({} packages)",
                report.rootfs.display(),
                report.packages.len()
            );
        }
        Command::Iso {
            rootfs,
            out,
            edition,
            kernel,
            initramfs,
        } => {
            let edition: Edition = edition.parse().map_err(anyhow::Error::msg)?;
            let rootfs = rootfs.unwrap_or_else(|| default_rootfs(&config, edition));
            let out = out.unwrap_or_else(|| default_iso(&config, edition));
            let report = build_iso(&IsoOptions {
                rootfs,
                output: out,
                edition,
                kernel,
                initramfs,
                tool_path: None,
            })?;
            println!("built {}", report.iso.display());
        }
        Command::All => {
            let checks = preflight();
            let missing: Vec<_> = checks.iter().filter(|check| !check.ok).collect();
            if !missing.is_empty() {
                bail!(
                    "host is missing required build tools: {}",
                    missing
                        .iter()
                        .map(|check| check.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            let recipes = default_recipes_dir();
            let report = build_stage0(&config, &recipes, true)?;
            println!("stage 0 complete: {}", report.tarball.display());

            let stage1_recipes = default_stage1_recipes_dir();
            let stage1 = build_stage1(&config, &stage1_recipes, true)?;
            println!(
                "stage 1 complete: {} packages in {}",
                stage1.packages.len(),
                config
                    .workdir
                    .join("packages")
                    .join(config.arch.as_str())
                    .display()
            );
        }
        Command::Publish => bail!("publishing is planned after artifacts exist"),
        Command::Clean => {
            clean_stage0(&config)?;
            clean_stage1(&config)?;
            println!("stage 0 and stage 1 build state removed");
        }
        Command::Status => {
            let paths = Stage0Paths::new(&config);
            println!("workdir: {}", paths.root.display());

            let states = load_state(&config)?;
            println!("[stage 0]");
            if states.is_empty() {
                println!("  not built");
            }
            for (name, status) in states {
                println!("  {name:<24} {status:?}");
            }

            let stage1_states = load_stage1_state(&config)?;
            println!("[stage 1]");
            if stage1_states.is_empty() {
                println!("  not built");
            }
            for (name, status) in stage1_states {
                println!("  {name:<24} {status:?}");
            }
        }
    }

    let _ = (cli.verbose, VERSION);
    Ok(ExitCode::SUCCESS)
}

fn default_recipes_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("recipes/stage0")
}

fn default_stage1_recipes_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages")
}

fn default_stage0_tarball(config: &BootstrapConfig) -> PathBuf {
    if let Some(location) = &config.stage0_tarball {
        return PathBuf::from(location.trim_start_matches("file://"));
    }
    Stage0Paths::new(config).artifacts.join(format!(
        "evergreen-stage0-{}-{}-{}.tar.zst",
        VERSION,
        config.arch.as_str(),
        config.libc.as_str()
    ))
}

fn default_package_dir(config: &BootstrapConfig) -> PathBuf {
    config.workdir.join("packages").join(config.arch.as_str())
}

fn default_rootfs(config: &BootstrapConfig, edition: Edition) -> PathBuf {
    config.workdir.join(format!("rootfs-{edition}"))
}

fn default_iso(config: &BootstrapConfig, edition: Edition) -> PathBuf {
    Stage0Paths::new(config).artifacts.join(format!(
        "evergreen-{edition}-{}-{}-{}.iso",
        VERSION,
        config.arch.as_str(),
        config.libc.as_str()
    ))
}
