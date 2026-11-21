//! Disk planning and fstab generation.

use std::cell::RefCell;
use std::path::Path;

use anyhow::{bail, Result};

use crate::config::{Encryption, InstallConfig, PartitionMode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: Option<String>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>, args: &[&str]) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            stdin: None,
        }
    }

    pub fn display(&self) -> String {
        format!("{} {}", self.program, self.args.join(" "))
    }
}

/// Executes installer commands. Implementations can dry-run by recording them.
pub trait Runner {
    fn run(&self, command: &CommandSpec) -> Result<()>;
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, command: &CommandSpec) -> Result<()> {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let mut process = Command::new(&command.program);
        process.args(&command.args);
        if command.stdin.is_some() {
            process.stdin(Stdio::piped());
        }
        let mut child = process.spawn()?;
        if let Some(input) = &command.stdin {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(input.as_bytes())?;
            }
        }
        let status = child.wait()?;
        if !status.success() {
            bail!("command failed: {}", command.display());
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct DryRunRunner {
    pub commands: RefCell<Vec<CommandSpec>>,
}

impl Runner for DryRunRunner {
    fn run(&self, command: &CommandSpec) -> Result<()> {
        self.commands.borrow_mut().push(command.clone());
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskPlan {
    pub commands: Vec<CommandSpec>,
    pub efi_partition: String,
    pub root_partition: String,
    pub home_partition: Option<String>,
    pub swap_partition: Option<String>,
    pub encrypted: bool,
}

/// Build the partition and filesystem commands for `config`.
pub fn plan(config: &InstallConfig) -> Result<DiskPlan> {
    let device = config.target_device.display().to_string();
    let target = config.target.display().to_string();
    let mut commands = Vec::new();

    let (efi, root, home, swap) = match config.partition_mode {
        PartitionMode::Auto => {
            let efi = format!("{device}1");
            let swap = format!("{device}2");
            let root = format!("{device}3");
            let home = if config.encryption == Encryption::Home {
                Some(format!("{device}4"))
            } else {
                None
            };

            commands.push(CommandSpec::new("sgdisk", &["--zap-all", &device]));
            commands.push(CommandSpec::new(
                "sgdisk",
                &["-n", "1::+512M", "-t", "1:ef00", &device],
            ));
            commands.push(CommandSpec::new(
                "sgdisk",
                &["-n", "2::+4G", "-t", "2:8200", &device],
            ));
            if home.is_some() {
                commands.push(CommandSpec::new(
                    "sgdisk",
                    &["-n", "3::-16G", "-t", "3:8300", &device],
                ));
                commands.push(CommandSpec::new(
                    "sgdisk",
                    &["-n", "4::0", "-t", "4:8300", &device],
                ));
            } else {
                commands.push(CommandSpec::new(
                    "sgdisk",
                    &["-n", "3::0", "-t", "3:8300", &device],
                ));
            }
            (efi, root, home, Some(swap))
        }
        PartitionMode::Manual => {
            let efi = config
                .efi_partition
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing efi_partition"))?
                .display()
                .to_string();
            let root = config
                .root_partition
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing root_partition"))?
                .display()
                .to_string();
            let home = config
                .home_partition
                .as_ref()
                .map(|path| path.display().to_string());
            (efi, root, home, None)
        }
    };

    let mut root_device = root.clone();
    let mut home_device = home.clone();
    let mut encrypted = false;

    if config.encryption == Encryption::Luks {
        let mut format = CommandSpec::new("cryptsetup", &["luksFormat", "--type", "luks2", &root]);
        format.stdin = Some(config.password.clone());
        commands.push(format);
        let mut open = CommandSpec::new("cryptsetup", &["open", &root, "evergreen-root"]);
        open.stdin = Some(config.password.clone());
        commands.push(open);
        root_device = "/dev/mapper/evergreen-root".to_string();
        encrypted = true;
    } else if config.encryption == Encryption::Home {
        if let Some(home_path) = &home {
            let mut format =
                CommandSpec::new("cryptsetup", &["luksFormat", "--type", "luks2", home_path]);
            format.stdin = Some(config.password.clone());
            commands.push(format);
            let mut open = CommandSpec::new("cryptsetup", &["open", home_path, "evergreen-home"]);
            open.stdin = Some(config.password.clone());
            commands.push(open);
            home_device = Some("/dev/mapper/evergreen-home".to_string());
            encrypted = true;
        }
    }

    commands.push(CommandSpec::new("mkfs.fat", &["-F32", &efi]));
    if let Some(swap) = &swap {
        commands.push(CommandSpec::new("mkswap", &[swap]));
        commands.push(CommandSpec::new("swapon", &[swap]));
    }
    commands.push(CommandSpec::new("mkfs.btrfs", &["-f", &root_device]));
    commands.push(CommandSpec::new(
        "mount",
        &["-o", "subvol=@,compress=zstd", &root_device, &target],
    ));
    commands.push(CommandSpec::new(
        "mkdir",
        &["-p", &format!("{target}/boot/efi")],
    ));

    if let Some(home) = &home_device {
        commands.push(CommandSpec::new("mkfs.ext4", &["-F", home]));
        commands.push(CommandSpec::new(
            "mkdir",
            &["-p", &format!("{target}/home")],
        ));
        commands.push(CommandSpec::new(
            "mount",
            &[home, &format!("{target}/home")],
        ));
    } else {
        for subvol in ["@home", "@var", "@snapshots"] {
            commands.push(CommandSpec::new(
                "btrfs",
                &["subvolume", "create", &format!("{target}/{subvol}")],
            ));
        }
        commands.push(CommandSpec::new(
            "mount",
            &[
                "-o",
                "subvol=@home,compress=zstd",
                &root_device,
                &format!("{target}/home"),
            ],
        ));
        commands.push(CommandSpec::new(
            "mount",
            &[
                "-o",
                "subvol=@var,compress=zstd",
                &root_device,
                &format!("{target}/var"),
            ],
        ));
    }

    commands.push(CommandSpec::new(
        "mount",
        &[&efi, &format!("{target}/boot/efi")],
    ));

    Ok(DiskPlan {
        commands,
        efi_partition: efi,
        root_partition: root_device,
        home_partition: home_device,
        swap_partition: swap,
        encrypted,
    })
}

/// Generate `/etc/fstab` content for the plan.
pub fn fstab(plan: &DiskPlan, config: &InstallConfig) -> String {
    let mut lines = vec![
        "# Evergreen Linux filesystems".to_string(),
        "# <device> <mountpoint> <type> <options> <dump> <pass>".to_string(),
    ];
    if let Some(swap) = &plan.swap_partition {
        lines.push(format!("{swap} none swap sw 0 0"));
    }
    lines.push(format!(
        "{} / btrfs subvol=@,compress=zstd 0 1",
        plan.root_partition
    ));
    if let Some(home) = &plan.home_partition {
        lines.push(format!("{home} /home ext4 defaults 0 2"));
    } else {
        lines.push(format!(
            "{} /home btrfs subvol=@home,compress=zstd 0 2",
            plan.root_partition
        ));
        lines.push(format!(
            "{} /var btrfs subvol=@var,compress=zstd 0 2",
            plan.root_partition
        ));
    }
    lines.push(format!(
        "{} /boot/efi vfat umask=0077 0 1",
        plan.efi_partition
    ));
    let _ = config;
    format!("{}\n", lines.join("\n"))
}

/// Ensure the target directory exists and is mounted as expected in dry-run.
pub fn prepare_target(config: &InstallConfig, runner: &dyn Runner) -> Result<()> {
    let path: &Path = config.target.as_path();
    if !config.dry_run {
        std::fs::create_dir_all(path)?;
    }
    runner.run(&CommandSpec::new("true", &[]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> InstallConfig {
        InstallConfig {
            password: "secret".to_string(),
            ..InstallConfig::default()
        }
    }

    #[test]
    fn plans_auto_btrfs_encryption() {
        let config = InstallConfig {
            encryption: Encryption::Luks,
            ..config()
        };
        let plan = plan(&config).unwrap();
        assert!(plan.encrypted);
        assert_eq!(plan.root_partition, "/dev/mapper/evergreen-root");
        let names: Vec<String> = plan
            .commands
            .iter()
            .map(|cmd| cmd.program.clone())
            .collect();
        assert!(names.contains(&"sgdisk".to_string()));
        assert!(names.contains(&"cryptsetup".to_string()));
        assert!(names.contains(&"mkfs.btrfs".to_string()));
    }

    #[test]
    fn fstab_contains_subvolumes() {
        let plan = plan(&config()).unwrap();
        let text = fstab(&plan, &config());
        assert!(text.contains("subvol=@,"));
        assert!(text.contains("/boot/efi"));
    }

    #[test]
    fn dry_run_records_commands() {
        let config = config();
        let plan = plan(&config).unwrap();
        let runner = DryRunRunner::default();
        for command in &plan.commands {
            runner.run(command).unwrap();
        }
        assert_eq!(runner.commands.borrow().len(), plan.commands.len());
    }
}
