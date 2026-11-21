//! Rootfs installation and post-install configuration.

use std::path::Path;

use anyhow::{Context, Result};

use crate::config::{Edition, InstallConfig};
use crate::disk::{self, CommandSpec, Runner};

#[derive(Debug, Clone)]
pub struct InstallReport {
    pub steps: Vec<String>,
    pub commands: Vec<CommandSpec>,
}

/// Perform a full installation into `config.target`.
pub fn install(config: &InstallConfig, runner: &dyn Runner) -> Result<InstallReport> {
    config.validate()?;
    let mut report = InstallReport {
        steps: Vec::new(),
        commands: Vec::new(),
    };

    let plan = disk::plan(config)?;
    for command in &plan.commands {
        runner.run(command)?;
        report.commands.push(command.clone());
    }
    report
        .steps
        .push("partitioned and formatted target disk".to_string());

    install_rootfs(config, runner, &mut report)?;
    write_configuration(config, &mut report)?;
    configure_system(config, runner, &mut report)?;
    install_bootloader(config, runner, &mut report)?;
    create_snapshot(config, runner, &mut report)?;

    Ok(report)
}

fn install_rootfs(
    config: &InstallConfig,
    runner: &dyn Runner,
    report: &mut InstallReport,
) -> Result<()> {
    let target = &config.target;
    if !config.dry_run {
        std::fs::create_dir_all(target)?;
    }

    if config.source.is_dir() {
        copy_tree(&config.source, target)?;
    } else {
        let command = CommandSpec::new(
            "unsquashfs",
            &[
                "-f",
                "-d",
                &target.display().to_string(),
                &config.source.display().to_string(),
            ],
        );
        runner.run(&command)?;
        report.commands.push(command);
    }
    report.steps.push("installed rootfs".to_string());
    Ok(())
}

fn write_configuration(config: &InstallConfig, report: &mut InstallReport) -> Result<()> {
    let etc = config.target.join("etc");
    std::fs::create_dir_all(&etc)?;
    std::fs::create_dir_all(etc.join("pine"))?;

    let plan = disk::plan(config)?;
    write(&etc.join("fstab"), &disk::fstab(&plan, config))?;
    write(&etc.join("hostname"), &format!("{}\n", config.hostname))?;
    write(
        &etc.join("locale.conf"),
        &format!("LANG={}\n", config.locale),
    )?;
    write(
        &etc.join("locale.gen"),
        &format!("{}.UTF-8 UTF-8\n", config.language),
    )?;
    write(
        &etc.join("vconsole.conf"),
        &format!("KEYMAP={}\n", config.keymap),
    )?;
    write(
        &etc.join("pine/pine.conf"),
        &format!(
            "[Manager]\nDefaultTarget = \"{}\"\nLogTarget = \"journal\"\n",
            default_target(config.edition)
        ),
    )?;

    let localtime = etc.join("localtime");
    if localtime.exists() || localtime.symlink_metadata().is_ok() {
        std::fs::remove_file(&localtime)?;
    }
    std::os::unix::fs::symlink(
        format!("/usr/share/zoneinfo/{}", config.timezone),
        localtime,
    )?;

    report
        .steps
        .push("wrote fstab, hostname, locale, keymap, timezone and pine config".to_string());
    Ok(())
}

fn configure_system(
    config: &InstallConfig,
    runner: &dyn Runner,
    report: &mut InstallReport,
) -> Result<()> {
    let target = config.target.display().to_string();
    let commands = vec![
        CommandSpec::new("chroot", &[&target, "locale-gen"]),
        CommandSpec::new(
            "chroot",
            &[
                &target,
                "useradd",
                "-m",
                "-G",
                "wheel",
                "-s",
                "/usr/bin/fish",
                &config.username,
            ],
        ),
        CommandSpec::new("chroot", &[&target, "passwd", "-l", "root"]),
    ];
    for command in commands {
        runner.run(&command)?;
        report.commands.push(command);
    }

    let mut user_password = CommandSpec::new("chroot", &[&target, "chpasswd"]);
    user_password.stdin = Some(format!("{}:{}\n", config.username, config.password));
    runner.run(&user_password)?;
    report.commands.push(user_password);

    if !config.root_password.is_empty() {
        let mut root_password = CommandSpec::new("chroot", &[&target, "passwd", "root"]);
        root_password.stdin = Some(format!(
            "{}\n{}\n",
            config.root_password, config.root_password
        ));
        runner.run(&root_password)?;
        report.commands.push(root_password);
    }

    report
        .steps
        .push("configured locale and created admin user".to_string());
    Ok(())
}

fn install_bootloader(
    config: &InstallConfig,
    runner: &dyn Runner,
    report: &mut InstallReport,
) -> Result<()> {
    let target = config.target.display().to_string();
    let efi = format!("{target}/boot/efi");
    let commands = vec![
        CommandSpec::new(
            "chroot",
            &[
                &target,
                "grub-install",
                "--target=x86_64-efi",
                "--efi-directory=/boot/efi",
                "--bootloader-id=evergreen",
            ],
        ),
        CommandSpec::new(
            "chroot",
            &[&target, "grub-mkconfig", "-o", "/boot/grub/grub.cfg"],
        ),
    ];
    let _ = efi;
    for command in commands {
        runner.run(&command)?;
        report.commands.push(command);
    }
    report.steps.push("installed GRUB bootloader".to_string());
    Ok(())
}

fn create_snapshot(
    config: &InstallConfig,
    runner: &dyn Runner,
    report: &mut InstallReport,
) -> Result<()> {
    let snapshot = format!(
        "{}/@snapshots/@pre-install-{}",
        config.target.display(),
        chrono::Utc::now().format("%Y%m%d%H%M%S")
    );
    let command = CommandSpec::new(
        "btrfs",
        &[
            "subvolume",
            "snapshot",
            "-r",
            &config.target.display().to_string(),
            &snapshot,
        ],
    );
    runner.run(&command)?;
    report.commands.push(command);
    report
        .steps
        .push("created initial btrfs snapshot".to_string());
    Ok(())
}

fn default_target(edition: Edition) -> &'static str {
    match edition {
        Edition::Desktop => "graphical.target",
        Edition::Minimal => "multi-user.target",
    }
}

fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    if source == destination {
        return Ok(());
    }
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let target = destination.join(&name);
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

pub fn target_exists(path: &Path) -> bool {
    path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::DryRunRunner;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "installer-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn dry_run_install_writes_configuration() {
        let dir = temp_dir();
        let source = dir.join("source");
        let target = dir.join("target");
        std::fs::create_dir_all(source.join("usr/bin")).unwrap();
        std::fs::write(source.join("usr/bin/pine"), b"binary").unwrap();

        let config = InstallConfig {
            source: source.clone(),
            target: target.clone(),
            timezone: "UTC".to_string(),
            hostname: "testbox".to_string(),
            username: "alice".to_string(),
            password: "secret".to_string(),
            root_password: "rootsecret".to_string(),
            dry_run: true,
            ..InstallConfig::default()
        };

        let runner = DryRunRunner::default();
        let report = install(&config, &runner).unwrap();
        assert!(report.steps.len() >= 5);
        assert!(!report.commands.is_empty());
        assert!(target.join("usr/bin/pine").exists());
        assert_eq!(
            std::fs::read_to_string(target.join("etc/hostname")).unwrap(),
            "testbox\n"
        );
        assert!(std::fs::read_to_string(target.join("etc/fstab"))
            .unwrap()
            .contains("/boot/efi"));
        assert!(std::fs::read_to_string(target.join("etc/pine/pine.conf"))
            .unwrap()
            .contains("graphical.target"));
        assert!(target.join("etc/localtime").symlink_metadata().is_ok());
        std::fs::remove_dir_all(dir).ok();
    }
}
