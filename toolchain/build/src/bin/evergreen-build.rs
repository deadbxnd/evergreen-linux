//! `evergreen-build` — command-line entry point.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use evergreen_build::recipe::load_recipe;
use evergreen_build::{assemble, build_iso, AssembleOptions, Edition, IsoOptions};

#[derive(Debug, Parser)]
#[command(
    name = "evergreen-build",
    version,
    about = "Evergreen build orchestrator"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Parse and display a package recipe.
    Recipe { path: PathBuf },
    /// Assemble a rootfs from a stage 0 tarball and .eva packages.
    Assemble {
        #[arg(long)]
        stage0: PathBuf,
        #[arg(long = "packages")]
        package_dirs: Vec<PathBuf>,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value = "desktop")]
        edition: String,
    },
    /// Publish .eva packages into a signed repository directory.
    Publish {
        #[arg(long)]
        packages: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value = "core")]
        repo: String,
        #[arg(long, default_value = "stable")]
        branch: String,
        #[arg(long)]
        key: Option<PathBuf>,
    },
    /// Validate all Stage 1 recipes without building them.
    ValidateRecipes {
        #[arg(long, default_value = "packages")]
        root: PathBuf,
    },
    /// Build a live ISO from an assembled rootfs.
    Iso {
        #[arg(long)]
        rootfs: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value = "desktop")]
        edition: String,
        #[arg(long)]
        kernel: Option<PathBuf>,
        #[arg(long)]
        initramfs: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("evergreen-build: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Command::Recipe { path } => {
            let recipe = load_recipe(&path)?;
            println!(
                "{} {} (arch: {}, stage: {:?})",
                recipe.package.name,
                recipe.package.version,
                if recipe.package.arch.is_empty() {
                    "any"
                } else {
                    &recipe.package.arch
                },
                recipe.build.stage
            );
        }
        Command::Assemble {
            stage0,
            package_dirs,
            out,
            edition,
        } => {
            let edition: Edition = edition.parse().map_err(anyhow::Error::msg)?;
            let report = assemble(&AssembleOptions {
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
        Command::Publish {
            packages,
            output,
            repo,
            branch,
            key,
        } => {
            let report = evergreen_build::publish(&evergreen_build::PublishOptions {
                packages_dir: packages,
                output_dir: output,
                repo,
                branch,
                signing_key: key,
            })?;
            println!("published {} packages", report.packages.len());
            println!("index: {}", report.index.display());
            if let Some(signature) = report.signature {
                println!("signature: {}", signature.display());
            }
        }
        Command::ValidateRecipes { root } => {
            let report = evergreen_build::validate_recipes(&root)?;
            println!("{} recipes validated", report.recipes.len());
            for warning in &report.warnings {
                println!("warning: {warning}");
            }
            for error in &report.errors {
                eprintln!("error: {error}");
            }
            if !report.errors.is_empty() {
                bail!("recipe validation failed");
            }
        }
        Command::Iso {
            rootfs,
            out,
            edition,
            kernel,
            initramfs,
        } => {
            let edition: Edition = edition.parse().map_err(anyhow::Error::msg)?;
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
    }
    Ok(ExitCode::SUCCESS)
}
