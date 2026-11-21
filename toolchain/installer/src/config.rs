//! Installer configuration parsing and validation.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edition {
    Desktop,
    Minimal,
}

impl std::fmt::Display for Edition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Edition {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "desktop" => Ok(Self::Desktop),
            "minimal" => Ok(Self::Minimal),
            other => anyhow::bail!("unknown edition {other:?}"),
        }
    }
}

impl Edition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Minimal => "minimal",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PartitionMode {
    Auto,
    Manual,
}

impl std::fmt::Display for PartitionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        })
    }
}

impl std::str::FromStr for PartitionMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "manual" => Ok(Self::Manual),
            other => anyhow::bail!("unknown partition mode {other:?}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Encryption {
    None,
    Luks,
    Home,
}

impl std::fmt::Display for Encryption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Luks => "luks",
            Self::Home => "home",
        })
    }
}

impl std::str::FromStr for Encryption {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "none" => Ok(Self::None),
            "luks" => Ok(Self::Luks),
            "home" => Ok(Self::Home),
            other => anyhow::bail!("unknown encryption mode {other:?}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InstallConfig {
    pub language: String,
    pub locale: String,
    pub timezone: String,
    pub keymap: String,
    pub edition: Edition,
    pub target_device: PathBuf,
    pub efi_partition: Option<PathBuf>,
    pub root_partition: Option<PathBuf>,
    pub home_partition: Option<PathBuf>,
    pub partition_mode: PartitionMode,
    pub encryption: Encryption,
    pub hostname: String,
    pub username: String,
    pub password: String,
    pub root_password: String,
    /// Directory or squashfs containing the rootfs to install.
    pub source: PathBuf,
    /// Where the target system is mounted during installation.
    pub target: PathBuf,
    pub dry_run: bool,
}

impl Default for InstallConfig {
    fn default() -> Self {
        Self {
            language: "en_US".to_string(),
            locale: "en_US.UTF-8".to_string(),
            timezone: "UTC".to_string(),
            keymap: "us".to_string(),
            edition: Edition::Desktop,
            target_device: PathBuf::from("/dev/sda"),
            efi_partition: None,
            root_partition: None,
            home_partition: None,
            partition_mode: PartitionMode::Auto,
            encryption: Encryption::None,
            hostname: "evergreen".to_string(),
            username: "user".to_string(),
            password: String::new(),
            root_password: String::new(),
            source: PathBuf::from("/run/evergreen/rootfs"),
            target: PathBuf::from("/mnt"),
            dry_run: false,
        }
    }
}

impl InstallConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.username.trim().is_empty() {
            bail!("username cannot be empty");
        }
        if self.hostname.trim().is_empty() {
            bail!("hostname cannot be empty");
        }
        if self.password.trim().is_empty() {
            bail!("user password cannot be empty");
        }
        if self.partition_mode == PartitionMode::Manual {
            if self.root_partition.is_none() {
                bail!("manual partitioning requires root_partition");
            }
            if self.efi_partition.is_none() {
                bail!("manual partitioning requires efi_partition");
            }
        }
        if self.encryption != Encryption::None && self.partition_mode == PartitionMode::Manual {
            bail!("encryption is only supported with automatic partitioning in 1.0");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_and_validates_config() {
        let text = r#"
language = "en_US"
locale = "en_US.UTF-8"
timezone = "Europe/Berlin"
keymap = "de"
edition = "desktop"
target_device = "/dev/nvme0n1"
partition_mode = "auto"
encryption = "luks"
hostname = "evergreen"
username = "alice"
password = "secret"
root_password = "rootsecret"
source = "/iso/rootfs"
target = "/mnt/target"
"#;
        let parsed: InstallConfig = toml::from_str(text).unwrap();
        parsed.validate().unwrap();
        assert_eq!(parsed.encryption, Encryption::Luks);
        assert_eq!(parsed.timezone, "Europe/Berlin");
    }

    #[test]
    fn rejects_missing_password() {
        let mut config = InstallConfig::default();
        assert!(config.validate().is_err());
        config.password = "x".to_string();
        config.validate().unwrap();
    }
}
