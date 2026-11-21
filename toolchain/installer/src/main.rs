//! `evergreen-installer` — graphical and text installer entry point.

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use evergreen_installer::{
    config::InstallConfig,
    disk::{DryRunRunner, SystemRunner},
    install, ui,
};

#[derive(Debug, Parser)]
#[command(
    name = "evergreen-installer",
    version,
    about = "Evergreen Linux installer"
)]
struct Cli {
    /// Installer configuration file.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Non-interactive mode (requires --config).
    #[arg(long, global = true)]
    noninteractive: bool,

    /// Do not run privileged commands; record them instead.
    #[arg(long, global = true)]
    dry_run: bool,

    /// Use the optional GTK4 front end.
    #[arg(long, global = true)]
    gui: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate a configuration file and exit.
    Validate,
    /// Print the installation plan without executing it.
    Plan,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("evergreen-installer: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    if cli.version_requested() {
        println!("evergreen-installer {}", evergreen_installer::VERSION);
        return Ok(());
    }

    let mut config = match &cli.config {
        Some(path) => InstallConfig::load(path)?,
        None => InstallConfig::default(),
    };
    config.dry_run = cli.dry_run || config.dry_run;

    if cli.gui {
        #[cfg(feature = "gtk")]
        {
            return evergreen_installer::gui::run_gui(config);
        }
        #[cfg(not(feature = "gtk"))]
        bail!("GTK4 front end is not compiled; rebuild with --features gtk or use --tui");
    }

    match cli.command {
        Some(Command::Validate) => {
            config.validate()?;
            println!("configuration is valid");
            return Ok(());
        }
        Some(Command::Plan) => {
            let plan = evergreen_installer::disk::plan(&config)?;
            for command in &plan.commands {
                println!("{}", command.display());
            }
            return Ok(());
        }
        None => {}
    }

    if cli.noninteractive && cli.config.is_none() {
        bail!("--noninteractive requires --config");
    }
    if !cli.noninteractive {
        config = ui::run_tui(config)?;
        config.validate()?;
        ui::confirm(&config)?;
    }

    if config.dry_run {
        let runner = DryRunRunner::default();
        let report = install(&config, &runner)?;
        print_report(&report.steps, report.commands.len());
        return Ok(());
    }

    let runner = SystemRunner;
    let report = install(&config, &runner)?;
    print_report(&report.steps, report.commands.len());
    Ok(())
}

fn print_report(steps: &[String], command_count: usize) {
    println!("Installation completed:");
    for step in steps {
        println!("  - {step}");
    }
    println!("{command_count} commands recorded/executed");
}

impl Cli {
    fn version_requested(&self) -> bool {
        std::env::args().any(|arg| arg == "--version" || arg == "-V")
    }
}
