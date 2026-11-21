//! ISO assembly: live squashfs + GRUB EFI/BIOS images.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::rootfs::Edition;

#[derive(Debug, Clone)]
pub struct IsoOptions {
    pub rootfs: PathBuf,
    pub output: PathBuf,
    pub edition: Edition,
    /// Kernel image. Defaults to `<rootfs>/boot/vmlinuz`.
    pub kernel: Option<PathBuf>,
    /// Initramfs image. Defaults to `<rootfs>/boot/initramfs`.
    pub initramfs: Option<PathBuf>,
    /// Optional PATH prefix used by tests to provide fake build tools.
    pub tool_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsoReport {
    pub iso: PathBuf,
    pub squashfs: PathBuf,
}

/// Build a bootable live ISO from an assembled rootfs.
pub fn build_iso(options: &IsoOptions) -> Result<IsoReport> {
    for tool in ["mksquashfs", "grub-mkrescue"] {
        if !tool_available(tool, options.tool_path.as_deref()) {
            bail!("required ISO tool {tool:?} is not installed");
        }
    }

    let kernel = options
        .kernel
        .clone()
        .unwrap_or_else(|| find_boot_image(&options.rootfs, "vmlinuz"));
    if !kernel.exists() {
        bail!("kernel image {} does not exist", kernel.display());
    }
    let initramfs = options
        .initramfs
        .clone()
        .unwrap_or_else(|| find_boot_image(&options.rootfs, "initramfs"));
    if !initramfs.exists() {
        bail!("initramfs {} does not exist", initramfs.display());
    }
    if !options.rootfs.exists() {
        bail!("rootfs {} does not exist", options.rootfs.display());
    }

    let staging = std::env::temp_dir().join(format!(
        "evergreen-iso-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&staging);
    for directory in ["live", "boot/grub", "EFI/BOOT"] {
        std::fs::create_dir_all(staging.join(directory))?;
    }

    let squashfs = staging.join("live/filesystem.squashfs");
    run_tool(
        "mksquashfs",
        &[
            options.rootfs.to_string_lossy().as_ref(),
            squashfs.to_string_lossy().as_ref(),
            "-noappend",
            "-comp",
            "zstd",
        ],
        options.tool_path.as_deref(),
    )?;
    if !squashfs.exists() {
        bail!("mksquashfs did not create {}", squashfs.display());
    }

    std::fs::copy(&kernel, staging.join("live/vmlinuz"))?;
    std::fs::copy(&initramfs, staging.join("live/initramfs"))?;

    let grub_cfg = format!(
        "set default=0\nset timeout=5\n\nmenuentry \"Evergreen Linux ({edition})\" {{\n    linux /live/vmlinuz boot=live quiet splash\n    initrd /live/initramfs\n}}\n",
        edition = options.edition
    );
    std::fs::write(staging.join("boot/grub/grub.cfg"), &grub_cfg)?;
    std::fs::write(staging.join("EFI/BOOT/grub.cfg"), &grub_cfg)?;

    if let Some(parent) = options.output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let volume_id = format!("EVERGREEN_{}", options.edition.as_str().to_uppercase());
    run_tool(
        "grub-mkrescue",
        &[
            "-o",
            options.output.to_string_lossy().as_ref(),
            "-volid",
            &volume_id,
            staging.to_string_lossy().as_ref(),
        ],
        options.tool_path.as_deref(),
    )?;
    if !options.output.exists() {
        bail!("grub-mkrescue did not create {}", options.output.display());
    }

    let report = IsoReport {
        iso: options.output.clone(),
        squashfs,
    };
    let _ = std::fs::remove_dir_all(&staging);
    Ok(report)
}

/// Prefer an exact `/boot/<name>` symlink, then versioned files such as
/// `/boot/vmlinuz-6.18.64`.
fn find_boot_image(rootfs: &Path, name: &str) -> PathBuf {
    let exact = rootfs.join("boot").join(name);
    if exact.exists() {
        return exact;
    }
    let boot = rootfs.join("boot");
    if let Ok(entries) = std::fs::read_dir(&boot) {
        let mut matches: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|text| text.starts_with(&format!("{name}-")))
            })
            .collect();
        matches.sort();
        if let Some(latest) = matches.pop() {
            return latest;
        }
    }
    exact
}

fn tool_available(tool: &str, tool_path: Option<&Path>) -> bool {
    let lookup = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool} >/dev/null 2>&1"))
        .envs(path_environment(tool_path))
        .status();
    matches!(lookup, Ok(status) if status.success())
}

fn run_tool(tool: &str, args: &[&str], tool_path: Option<&Path>) -> Result<()> {
    let status = Command::new(tool)
        .args(args)
        .envs(path_environment(tool_path))
        .status()
        .with_context(|| format!("running {tool}"))?;
    if !status.success() {
        bail!("{tool} failed with exit status {status}");
    }
    Ok(())
}

fn path_environment(tool_path: Option<&Path>) -> Vec<(&'static str, String)> {
    let existing = std::env::var("PATH").unwrap_or_default();
    let path = match tool_path {
        Some(directory) => format!("{}:{existing}", directory.display()),
        None => existing,
    };
    vec![("PATH", path)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_iso_with_fake_tools() {
        let dir = std::env::temp_dir().join(format!(
            "build-iso-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let rootfs = dir.join("rootfs");
        std::fs::create_dir_all(rootfs.join("boot")).unwrap();
        std::fs::write(rootfs.join("boot/vmlinuz"), b"kernel\n").unwrap();
        std::fs::write(rootfs.join("boot/initramfs"), b"initramfs\n").unwrap();

        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            bin.join("mksquashfs"),
            "#!/bin/sh\nmkdir -p \"$(dirname \"$2\")\"\nprintf squashfs > \"$2\"\n",
        )
        .unwrap();
        std::fs::write(
            bin.join("grub-mkrescue"),
            "#!/bin/sh\nprintf iso > \"$2\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for tool in ["mksquashfs", "grub-mkrescue"] {
                std::fs::set_permissions(bin.join(tool), std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
        }

        let report = build_iso(&IsoOptions {
            rootfs,
            output: dir.join("out.iso"),
            edition: Edition::Desktop,
            kernel: None,
            initramfs: None,
            tool_path: Some(bin),
        })
        .unwrap();
        assert_eq!(std::fs::read(&report.iso).unwrap(), b"iso");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn reports_missing_kernel() {
        let dir = std::env::temp_dir().join(format!(
            "build-iso-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let result = build_iso(&IsoOptions {
            rootfs: dir.clone(),
            output: dir.join("out.iso"),
            edition: Edition::Minimal,
            kernel: None,
            initramfs: None,
            tool_path: None,
        });
        assert!(result.is_err());
        std::fs::remove_dir_all(dir).ok();
    }
}
