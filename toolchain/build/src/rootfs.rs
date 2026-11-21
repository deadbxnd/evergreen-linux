//! Assemble a base rootfs from a stage 0 tarball and `.eva` packages.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use eva::archive::{extract_package, sha256_file};
use eva::config::Paths;
use eva::db::InstalledDb;
use eva::model::{InstallReason, InstalledPackage};

/// Product edition assembled from the same base packages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edition {
    Desktop,
    Minimal,
}

impl Edition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Minimal => "minimal",
        }
    }

    pub fn default_target(self) -> &'static str {
        match self {
            Self::Desktop => "graphical.target",
            Self::Minimal => "multi-user.target",
        }
    }
}

impl FromStr for Edition {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "desktop" => Ok(Self::Desktop),
            "minimal" | "server" => Ok(Self::Minimal),
            other => Err(format!("unknown edition {other:?}")),
        }
    }
}

impl std::fmt::Display for Edition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct AssembleOptions {
    pub stage0_tarball: PathBuf,
    pub package_dirs: Vec<PathBuf>,
    pub output: PathBuf,
    pub edition: Edition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembleReport {
    pub rootfs: PathBuf,
    pub packages: Vec<String>,
}

/// Assemble a rootfs. Any existing output directory is replaced.
pub fn assemble(options: &AssembleOptions) -> Result<AssembleReport> {
    if !options.stage0_tarball.exists() {
        bail!(
            "stage 0 tarball {} does not exist",
            options.stage0_tarball.display()
        );
    }
    if options.output.exists() {
        std::fs::remove_dir_all(&options.output)?;
    }
    std::fs::create_dir_all(&options.output)?;

    extract_stage0(&options.stage0_tarball, &options.output)?;

    let archives = collect_packages(&options.package_dirs)?;
    let mut packages = Vec::new();
    let paths = Paths::for_root(&options.output);
    let db = InstalledDb::open_at(&paths)?;

    for archive in &archives {
        let name = archive
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        let checksum = sha256_file(archive)?;
        let mut candidate = extract_package(archive, &options.output, "")?;
        candidate.meta.checksum = checksum;
        candidate.archive = name;

        db.insert(&InstalledPackage {
            candidate: candidate.clone(),
            reason: InstallReason::Explicit,
            installed_at: chrono::Utc::now().to_rfc3339(),
        })
        .with_context(|| format!("registering {}", candidate.id.name))?;
        packages.push(candidate.id.name.clone());
    }

    generate_initramfs(&options.output);
    write_base_configuration(&options.output, options.edition)?;
    ensure_pine_units(&options.output)?;

    Ok(AssembleReport {
        rootfs: options.output.clone(),
        packages,
    })
}

fn extract_stage0(tarball: &Path, output: &Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("--zstd")
        .arg("-xf")
        .arg(tarball)
        .arg("-C")
        .arg(output)
        .status()
        .with_context(|| format!("extracting {}", tarball.display()))?;
    if !status.success() {
        bail!("failed to extract stage 0 tarball {}", tarball.display());
    }
    Ok(())
}

fn collect_packages(directories: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut seen = BTreeSet::new();
    let mut archives = Vec::new();
    for directory in directories {
        if !directory.exists() {
            continue;
        }
        let entries = std::fs::read_dir(directory)
            .with_context(|| format!("reading {}", directory.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("eva") {
                continue;
            }
            let key = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string();
            if seen.insert(key) {
                archives.push(path);
            }
        }
    }
    archives.sort();
    Ok(archives)
}

fn write_base_configuration(rootfs: &Path, edition: Edition) -> Result<()> {
    let etc = rootfs.join("etc");
    std::fs::create_dir_all(&etc)?;
    for directory in ["boot", "dev", "proc", "sys", "run", "var/log", "var/lib"] {
        std::fs::create_dir_all(rootfs.join(directory))?;
    }

    write_if_missing(&etc.join("hostname"), "evergreen\n")?;
    write_if_missing(
        &etc.join("fstab"),
        "# Evergreen Linux static filesystems\n# Use the installer to configure persistent mounts.\n",
    )?;

    let pine = etc.join("pine");
    std::fs::create_dir_all(&pine)?;
    write_if_missing(
        &pine.join("pine.conf"),
        &format!(
            "[Manager]\nDefaultTarget = \"{}\"\nLogTarget = \"journal\"\n",
            edition.default_target()
        ),
    )?;

    let init = rootfs.join("init");
    if !init.exists() && rootfs.join("usr/sbin/pine").exists() {
        std::os::unix::fs::symlink("usr/sbin/pine", init)?;
    }
    Ok(())
}

fn write_if_missing(path: &Path, contents: &str) -> Result<()> {
    if !path.exists() {
        std::fs::write(path, contents)?;
    }
    Ok(())
}

/// Build `/boot/initramfs` from the installed pine-init binary and the newest
/// installed kernel's modules. Skipped when either is missing or `cpio`/`gzip`
/// are unavailable.
fn generate_initramfs(rootfs: &Path) {
    let modules_root = rootfs.join("usr/lib/modules");
    let Ok(entries) = std::fs::read_dir(&modules_root) else {
        return;
    };
    let mut versions: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .collect();
    versions.sort();
    let Some(version) = versions.pop() else {
        return;
    };

    let pine_init = rootfs.join("usr/lib/pine-init/init");
    if !pine_init.exists() {
        return;
    }
    if !tool_available("cpio") || !tool_available("gzip") {
        eprintln!("evergreen-build: cpio/gzip unavailable; skipping initramfs generation");
        return;
    }

    let tree = rootfs.join(".initramfs-tmp");
    let _ = std::fs::remove_dir_all(&tree);
    for directory in [
        "bin",
        "sbin",
        "dev",
        "proc",
        "sys",
        "run",
        "etc",
        "usr/lib/pine-init",
    ] {
        let _ = std::fs::create_dir_all(tree.join(directory));
    }
    let module_target = tree.join("lib/modules").join(&version);
    if copy_tree(&modules_root.join(&version), &module_target).is_err() {
        let _ = std::fs::remove_dir_all(&tree);
        return;
    }
    if std::fs::copy(&pine_init, tree.join("init")).is_err() {
        let _ = std::fs::remove_dir_all(&tree);
        return;
    }

    let boot = rootfs.join("boot");
    let _ = std::fs::create_dir_all(&boot);
    let cpio_path = boot.join("initramfs.cpio");
    let initramfs = boot.join("initramfs");

    let cpio_status = Command::new("sh")
        .arg("-c")
        .arg("find . -print0 | cpio --null -o -H newc -F \"$1\"")
        .arg("sh")
        .arg(&cpio_path)
        .current_dir(&tree)
        .status();
    if !matches!(cpio_status, Ok(status) if status.success()) {
        let _ = std::fs::remove_dir_all(&tree);
        return;
    }

    let gzip = std::fs::File::create(&initramfs).and_then(|output| {
        let mut child = Command::new("gzip")
            .arg("-9")
            .arg("-c")
            .arg(&cpio_path)
            .stdout(output)
            .spawn()?;
        child.wait()
    });
    let _ = std::fs::remove_file(&cpio_path);
    let _ = std::fs::remove_dir_all(&tree);

    if matches!(gzip, Ok(status) if status.success()) {
        let vmlinuz = boot.join("vmlinuz");
        let versioned = boot.join(format!("vmlinuz-{version}"));
        if !vmlinuz.exists() && versioned.exists() {
            let _ = std::os::unix::fs::symlink(format!("vmlinuz-{version}"), vmlinuz);
        }
    }
}

fn tool_available(tool: &str) -> bool {
    matches!(
        Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {tool} >/dev/null 2>&1"))
            .status(),
        Ok(status) if status.success()
    )
}

fn copy_tree(source: &Path, destination: &Path) -> std::io::Result<()> {
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

fn ensure_pine_units(rootfs: &Path) -> Result<()> {
    let units = rootfs.join("usr/lib/pine/services");
    std::fs::create_dir_all(&units)?;

    let definitions = [
        ("sysinit.target", "[Unit]\nDescription = \"System initialization\"\n"),
        (
            "basic.target",
            "[Unit]\nDescription = \"Basic system\"\nWants = [\"sysinit.target\"]\n",
        ),
        (
            "multi-user.target",
            "[Unit]\nDescription = \"Multi-user system\"\nWants = [\"basic.target\"]\n",
        ),
        (
            "graphical.target",
            "[Unit]\nDescription = \"Graphical interface\"\nWants = [\"multi-user.target\"]\n",
        ),
        ("rescue.target", "[Unit]\nDescription = \"Rescue mode\"\n"),
        ("poweroff.target", "[Unit]\nDescription = \"Power off\"\n"),
        ("reboot.target", "[Unit]\nDescription = \"Reboot\"\n"),
        (
            "getty@.service",
            "[Unit]\nDescription = \"Virtual console getty on %I\"\n\n[Service]\nType = \"simple\"\nExecStart = \"/usr/sbin/agetty --noclear tty%I 38400 linux\"\nRestart = \"on-failure\"\n\n[Install]\nWantedBy = [\"multi-user.target\"]\n",
        ),
    ];
    for (name, contents) in definitions {
        let path = units.join(name);
        if !path.exists() {
            std::fs::write(path, contents)?;
        }
    }

    let services = [
        (
            "dbus",
            "/usr/bin/dbus-daemon --system --nofork --nopidfile",
            "sysinit.target",
        ),
        (
            "polkit",
            "/usr/lib/polkit-1/polkitd --no-debug",
            "dbus.service",
        ),
        ("elogind", "/usr/lib/elogind/elogind", "dbus.service"),
        (
            "networkmanager",
            "/usr/sbin/NetworkManager --no-daemon",
            "dbus.service",
        ),
        (
            "wpa_supplicant",
            "/usr/sbin/wpa_supplicant -u -s",
            "network.target",
        ),
        ("pipewire", "/usr/bin/pipewire", "dbus.service"),
        ("wireplumber", "/usr/bin/wireplumber", "pipewire.service"),
        (
            "pipewire-pulse",
            "/usr/bin/pipewire-pulse",
            "pipewire.service",
        ),
        ("bluez", "/usr/sbin/bluetoothd -n", "dbus.service"),
        ("cups", "/usr/sbin/cupsd -f", "dbus.service"),
        ("upower", "/usr/lib/upower/upowerd", "dbus.service"),
        ("chronyd", "/usr/sbin/chronyd -d", "network.target"),
        (
            "thermald",
            "/usr/sbin/thermald --no-daemon",
            "sysinit.target",
        ),
        ("tlp", "/usr/sbin/tlp start", "sysinit.target"),
        (
            "nftables",
            "/usr/sbin/nft -f /etc/nftables.conf",
            "network.target",
        ),
        ("sshd", "/usr/sbin/sshd -D", "network.target"),
        ("gdm", "/usr/sbin/gdm", "dbus.service"),
        ("sddm", "/usr/bin/sddm", "dbus.service"),
        ("lightdm", "/usr/sbin/lightdm", "dbus.service"),
        ("ly", "/usr/bin/ly", "dbus.service"),
        ("greetd", "/usr/bin/greetd", "dbus.service"),
    ];
    let multiuser = units.join("multi-user.target.wants");
    let graphical = units.join("graphical.target.wants");
    std::fs::create_dir_all(&multiuser)?;
    std::fs::create_dir_all(&graphical)?;

    for (name, exec_start, after) in services {
        let unit = format!(
            "[Unit]\nDescription = \"{name} service\"\nAfter = [\"{after}\"]\n\n[Service]\nType = \"simple\"\nExecStart = \"{exec_start}\"\nRestart = \"on-failure\"\n\n[Install]\nWantedBy = [\"multi-user.target\"]\n"
        );
        let path = units.join(format!("{name}.service"));
        if !path.exists() {
            std::fs::write(path, unit)?;
        }
        let display_manager = matches!(name, "gdm" | "sddm" | "lightdm" | "ly" | "greetd");
        let wants = if display_manager {
            &graphical
        } else {
            &multiuser
        };
        let link = wants.join(format!("{name}.service"));
        if std::fs::symlink_metadata(&link).is_err() {
            let _ = std::os::unix::fs::symlink(format!("../{name}.service"), link);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eva::archive::write_package;
    use eva::model::{Arch, Candidate, PackageId, PackageMeta};
    use evergreen_common::{RepoId, Version};

    fn candidate(name: &str) -> Candidate {
        Candidate {
            id: PackageId {
                name: name.to_string(),
                version: Version::parse("1.0").unwrap(),
                arch: Arch::X86_64,
                repo: RepoId(0),
            },
            meta: PackageMeta::default(),
            deps: Vec::new(),
            opt_deps: Vec::new(),
            recommends: Vec::new(),
            suggests: Vec::new(),
            conflicts: Vec::new(),
            provides: Vec::new(),
            replaces: Vec::new(),
            files: Vec::new(),
            symlinks: Vec::new(),
            hooks: Default::default(),
            archive: String::new(),
        }
    }

    #[test]
    fn assembles_rootfs_from_tarball_and_packages() {
        let dir = std::env::temp_dir().join(format!(
            "build-assemble-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("base/usr/bin")).unwrap();
        std::fs::create_dir_all(dir.join("base/boot")).unwrap();
        std::fs::write(dir.join("base/usr/bin/sh"), b"#!").unwrap();
        std::fs::create_dir_all(dir.join("base/usr/lib/modules/6.18.64")).unwrap();
        std::fs::write(
            dir.join("base/usr/lib/modules/6.18.64/modules.order"),
            b"kernel/fs/btrfs/btrfs.ko\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("base/usr/lib/pine-init")).unwrap();
        std::fs::write(dir.join("base/usr/lib/pine-init/init"), b"#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("base/boot/vmlinuz-6.18.64"), b"kernel\n").unwrap();
        let tarball = dir.join("stage0.tar.zst");
        assert!(Command::new("tar")
            .arg("--zstd")
            .arg("-cf")
            .arg(&tarball)
            .arg("-C")
            .arg(dir.join("base"))
            .arg(".")
            .status()
            .unwrap()
            .success());

        let payload = dir.join("payload/usr/bin");
        std::fs::create_dir_all(&payload).unwrap();
        std::fs::write(payload.join("demo"), b"binary\n").unwrap();
        let packages = dir.join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        let package = packages.join("demo-1.0.eva");
        write_package(&package, &candidate("demo"), &dir.join("payload")).unwrap();

        let report = assemble(&AssembleOptions {
            stage0_tarball: tarball,
            package_dirs: vec![packages],
            output: dir.join("rootfs"),
            edition: Edition::Minimal,
        })
        .unwrap();

        assert_eq!(report.packages, vec!["demo"]);
        assert!(report.rootfs.join("usr/bin/sh").exists());
        assert!(report.rootfs.join("usr/bin/demo").exists());
        assert_eq!(
            std::fs::read_to_string(report.rootfs.join("etc/pine/pine.conf")).unwrap(),
            "[Manager]\nDefaultTarget = \"multi-user.target\"\nLogTarget = \"journal\"\n"
        );
        assert!(report
            .rootfs
            .join("usr/lib/pine/services/multi-user.target")
            .exists());
        assert!(report
            .rootfs
            .join("usr/lib/pine/services/dbus.service")
            .exists());
        assert!(report
            .rootfs
            .join("usr/lib/pine/services/multi-user.target.wants/dbus.service")
            .symlink_metadata()
            .is_ok());
        assert!(report.rootfs.join("boot/initramfs").exists());
        assert_eq!(
            std::fs::read_link(report.rootfs.join("boot/vmlinuz")).unwrap(),
            PathBuf::from("vmlinuz-6.18.64")
        );
        std::fs::remove_dir_all(dir).ok();
    }
}
