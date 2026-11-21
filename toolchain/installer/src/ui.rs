//! Text-based installer UI.

use std::io::{self, Write};

use anyhow::{bail, Result};

use crate::config::{InstallConfig, PartitionMode};

/// Run the interactive questionnaire, starting from `base`.
pub fn run_tui(base: InstallConfig) -> Result<InstallConfig> {
    let mut config = base;
    println!("Evergreen Linux installer (text mode)");
    println!("Press Enter to accept the value in brackets.\n");

    config.language = prompt("Language", &config.language)?;
    config.locale = prompt("Locale", &config.locale)?;
    config.timezone = prompt("Timezone", &config.timezone)?;
    config.keymap = prompt("Keyboard layout", &config.keymap)?;
    config.edition = prompt_enum("Edition", config.edition, &["desktop", "minimal"])?.parse()?;
    config.partition_mode =
        prompt_enum("Partitioning", config.partition_mode, &["auto", "manual"])?.parse()?;
    config.encryption = prompt_enum(
        "Encryption (none/luks/home)",
        config.encryption,
        &["none", "luks", "home"],
    )?
    .parse()?;
    config.target_device = prompt_path("Target disk", &config.target_device)?;

    if config.partition_mode == PartitionMode::Manual {
        config.efi_partition = Some(prompt_path("EFI partition", &config.target_device)?);
        config.root_partition = Some(prompt_path("Root partition", &config.target_device)?);
        config.home_partition = Some(prompt_path(
            "Home partition (optional)",
            &config.target_device,
        )?);
    }

    config.hostname = prompt("Hostname", &config.hostname)?;
    config.username = prompt("Username", &config.username)?;
    config.password = prompt_secret("User password")?;
    config.root_password = prompt_secret("Root password (leave empty to lock root)")?;
    config.source = prompt_path("Rootfs source", &config.source)?;
    config.target = prompt_path("Mount target", &config.target)?;

    Ok(config)
}

fn prompt(label: &str, default: &str) -> Result<String> {
    print!("{label} [{default}]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();
    Ok(if input.is_empty() {
        default.to_string()
    } else {
        input.to_string()
    })
}

fn prompt_secret(label: &str) -> Result<String> {
    print!("{label}: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

fn prompt_path(label: &str, default: &std::path::Path) -> Result<std::path::PathBuf> {
    let text = prompt(label, &default.display().to_string())?;
    Ok(std::path::PathBuf::from(text))
}

fn prompt_enum<T: std::fmt::Display>(label: &str, default: T, _choices: &[&str]) -> Result<String> {
    let value = prompt(label, &default.to_string())?;
    Ok(value)
}

/// Validate a config before performing any destructive action.
pub fn confirm(config: &InstallConfig) -> Result<bool> {
    println!("\nSummary:");
    println!("  edition:      {}", config.edition.as_str());
    println!("  target disk:  {}", config.target_device.display());
    println!("  partitioning: {:?}", config.partition_mode);
    println!("  encryption:   {:?}", config.encryption);
    println!("  timezone:     {}", config.timezone);
    println!("  user:         {}", config.username);
    println!("  hostname:     {}", config.hostname);
    let answer = prompt("Proceed with installation?", "no")?;
    if !matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") {
        bail!("installation aborted");
    }
    Ok(true)
}
