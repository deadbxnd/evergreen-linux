//! Stage 0 build execution: fetch, verify, extract, compile, install, and state.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{Arch, BootstrapConfig, Libc};
use crate::recipe::{build_order, load_recipes, Stage0Recipe};
use crate::stages::StageStatus;

/// Result of a single preflight check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightCheck {
    pub name: String,
    pub ok: bool,
    pub message: String,
}

/// Check host tools needed for a stage 0 build.
pub fn preflight() -> Vec<PreflightCheck> {
    let tools = [
        ("sh", "shell for recipe commands"),
        ("gcc", "C compiler"),
        ("make", "build driver"),
        ("tar", "source archive extraction"),
        ("xz", "xz-compressed sources"),
        ("zstd", "stage 0 tarball compression"),
        ("cpio", "initramfs generation"),
        ("gzip", "initramfs compression"),
        ("rustc", "Rust toolchain (stage 1)"),
        ("cargo", "Rust package manager (stage 1)"),
        ("mksquashfs", "live ISO squashfs"),
        ("grub-mkrescue", "bootable ISO creation"),
        ("xorriso", "ISO image writer"),
        ("mformat", "mtools for GRUB EFI image"),
        ("mcopy", "mtools for GRUB EFI image"),
        ("cmake", "CMake-based package builds"),
        ("meson", "Meson-based package builds"),
        ("ninja", "Ninja build tool"),
        ("autoconf", "Autotools-based package builds"),
        ("zic", "timezone database compilation"),
    ];

    tools
        .iter()
        .map(|(tool, purpose)| {
            let found = Command::new("sh")
                .arg("-c")
                .arg(format!("command -v {tool} >/dev/null 2>&1"))
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            PreflightCheck {
                name: (*tool).to_string(),
                ok: found,
                message: if found {
                    purpose.to_string()
                } else {
                    format!("missing {tool}: required for {purpose}")
                },
            }
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PackageState {
    version: String,
    source_sha256: String,
    status: StageStatus,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    finished_at: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct BuildState {
    packages: BTreeMap<String, PackageState>,
}

/// Layout of the stage 0 work directory.
#[derive(Debug, Clone)]
pub struct Stage0Paths {
    pub root: PathBuf,
    pub sources: PathBuf,
    pub build: PathBuf,
    pub sysroot: PathBuf,
    pub state: PathBuf,
    pub artifacts: PathBuf,
}

impl Stage0Paths {
    pub fn new(config: &BootstrapConfig) -> Self {
        let root = config.workdir.clone();
        Self {
            sources: root.join("src"),
            build: root.join("build"),
            sysroot: root.join("stage0"),
            state: root.join("state"),
            artifacts: if config.artifacts.is_absolute() {
                config.artifacts.clone()
            } else {
                root.join(&config.artifacts)
            },
            root,
        }
    }

    fn state_file(&self, libc: Libc, arch: Arch) -> PathBuf {
        self.state
            .join(format!("stage0-{}-{}.json", arch.as_str(), libc.as_str()))
    }
}

/// Summary of a stage 0 run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage0Report {
    pub built: Vec<String>,
    pub skipped: Vec<String>,
    pub tarball: PathBuf,
}

/// Build the complete stage 0 sysroot from recipes.
pub fn build_stage0(
    config: &BootstrapConfig,
    recipes_dir: &Path,
    resume: bool,
) -> Result<Stage0Report> {
    let paths = Stage0Paths::new(config);
    for directory in [
        &paths.sources,
        &paths.build,
        &paths.sysroot,
        &paths.state,
        &paths.artifacts,
    ] {
        std::fs::create_dir_all(directory)?;
    }

    let recipes = load_recipes(recipes_dir, config.libc, config.arch)?;
    let ordered = build_order(&recipes)?;
    let state_file = paths.state_file(config.libc, config.arch);
    let mut state: BuildState = if resume && state_file.exists() {
        serde_json::from_str(&std::fs::read_to_string(&state_file)?)?
    } else {
        BuildState::default()
    };

    let mut built = Vec::new();
    let mut skipped = Vec::new();

    for recipe in &ordered {
        let name = recipe.name();
        if let Some(previous) = state.packages.get(name) {
            if previous.status == StageStatus::Done
                && previous.version == recipe.version()
                && previous.source_sha256 == recipe.source.sha256
            {
                skipped.push(name.to_string());
                continue;
            }
        }

        state.packages.insert(
            name.to_string(),
            PackageState {
                version: recipe.version().to_string(),
                source_sha256: recipe.source.sha256.clone(),
                status: StageStatus::Building,
                error: None,
                finished_at: None,
            },
        );
        save_state(&state_file, &state)?;

        let result = build_one(recipe, &paths, config)
            .with_context(|| format!("building stage 0 package {name}"));
        match result {
            Ok(()) => {
                if let Some(entry) = state.packages.get_mut(name) {
                    entry.status = StageStatus::Done;
                    entry.finished_at = Some(chrono::Utc::now().to_rfc3339());
                }
                built.push(name.to_string());
            }
            Err(err) => {
                let message = format!("{err:#}");
                if let Some(entry) = state.packages.get_mut(name) {
                    entry.status = StageStatus::Failed;
                    entry.error = Some(message.clone());
                }
                save_state(&state_file, &state)?;
                return Err(anyhow!("{message}"));
            }
        }
        save_state(&state_file, &state)?;
    }

    let version = env!("CARGO_PKG_VERSION");
    let tarball = paths.artifacts.join(format!(
        "evergreen-stage0-{}-{}-{}.tar.zst",
        version,
        config.arch.as_str(),
        config.libc.as_str()
    ));
    create_stage0_tarball(&paths.sysroot, &tarball)?;

    Ok(Stage0Report {
        built,
        skipped,
        tarball,
    })
}

fn save_state(path: &Path, state: &BuildState) -> Result<()> {
    let text = serde_json::to_string_pretty(state)?;
    std::fs::write(path, text)?;
    Ok(())
}

/// Load the recorded state for reporting.
pub fn load_state(config: &BootstrapConfig) -> Result<BTreeMap<String, StageStatus>> {
    let paths = Stage0Paths::new(config);
    let file = paths.state_file(config.libc, config.arch);
    if !file.exists() {
        return Ok(BTreeMap::new());
    }
    let state: BuildState = serde_json::from_str(&std::fs::read_to_string(file)?)?;
    Ok(state
        .packages
        .into_iter()
        .map(|(name, package)| (name, package.status))
        .collect())
}

/// Remove all stage 0 build state and outputs for this configuration.
pub fn clean_stage0(config: &BootstrapConfig) -> Result<()> {
    let paths = Stage0Paths::new(config);
    for directory in [&paths.sources, &paths.build, &paths.sysroot, &paths.state] {
        if directory.exists() {
            std::fs::remove_dir_all(directory)?;
        }
    }
    let _ = std::fs::remove_file(paths.state_file(config.libc, config.arch));
    Ok(())
}

fn build_one(recipe: &Stage0Recipe, paths: &Stage0Paths, config: &BootstrapConfig) -> Result<()> {
    let location = recipe.source_location()?;
    let archive = fetch_source(&location, paths, recipe)?;
    verify_sha256(&archive, &recipe.source.sha256)?;
    let source_dir = extract_source(&archive, recipe, paths)?;
    if !recipe.bootstrap.sysroot {
        return Ok(());
    }

    let workdir = &paths.root;
    let sysroot = &paths.sysroot;
    let prefix = "/usr";

    let steps = [
        ("configure", recipe.configure_command()),
        ("build", recipe.build_command()),
        ("install", recipe.install_command()),
    ];

    for (step, command) in steps {
        if command.trim().is_empty() {
            continue;
        }
        let command = substitute(&command, config, workdir, sysroot, &source_dir, prefix);
        let status = Command::new("/bin/sh")
            .arg("-c")
            .arg(&command)
            .current_dir(&source_dir)
            .env("SYSROOT", sysroot)
            .env("DESTDIR", sysroot)
            .env("SRC", &source_dir)
            .env("WORKDIR", workdir)
            .env("PREFIX", prefix)
            .env("JOBS", config.jobs().to_string())
            .status()
            .with_context(|| format!("running {step} for {}", recipe.name()))?;
        if !status.success() {
            bail!("{} step failed for {} (exit {status})", step, recipe.name());
        }
    }
    Ok(())
}

fn substitute(
    command: &str,
    config: &BootstrapConfig,
    workdir: &Path,
    sysroot: &Path,
    source: &Path,
    prefix: &str,
) -> String {
    command
        .replace("{jobs}", &config.jobs().to_string())
        .replace("{sysroot}", &sysroot.display().to_string())
        .replace("{workdir}", &workdir.display().to_string())
        .replace("{source}", &source.display().to_string())
        .replace("{prefix}", prefix)
}

fn archive_name(recipe: &Stage0Recipe, location: &str) -> String {
    let filename = Path::new(location)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("{}-{}.tar.gz", recipe.name(), recipe.version()));
    format!("{}-{}", recipe.name(), filename)
}

fn fetch_source(location: &str, paths: &Stage0Paths, recipe: &Stage0Recipe) -> Result<PathBuf> {
    let destination = paths.sources.join(archive_name(recipe, location));
    if destination.exists() {
        return Ok(destination);
    }

    let bytes = if let Some(path) = location.strip_prefix("file://") {
        std::fs::read(path)?
    } else if !location.contains("://") {
        std::fs::read(location)?
    } else if location.starts_with("http://") || location.starts_with("https://") {
        let response = ureq::get(location)
            .call()
            .map_err(|err| anyhow!("downloading {location}: {err}"))?;
        use std::io::Read;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading response from {location}"))?;
        bytes
    } else {
        bail!("unsupported source URL scheme: {location}");
    };

    std::fs::write(&destination, bytes)?;
    Ok(destination)
}

fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hex::encode(hasher.finalize());
    if actual != expected.trim().to_ascii_lowercase() {
        bail!(
            "source checksum mismatch for {}: expected {expected}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

fn extract_source(archive: &Path, recipe: &Stage0Recipe, paths: &Stage0Paths) -> Result<PathBuf> {
    let directory = paths.build.join(recipe.name());
    let source_dir = directory.join("src");
    if source_dir.exists() {
        std::fs::remove_dir_all(&source_dir)?;
    }
    std::fs::create_dir_all(&source_dir)?;

    let name = archive
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if !name.contains(".tar")
        && !name.ends_with(".tgz")
        && !name.ends_with(".txz")
        && !name.ends_with(".tzst")
    {
        let destination = source_dir.join(name);
        std::fs::copy(archive, &destination)?;
        return Ok(source_dir);
    }
    let flag = if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        "-xzf"
    } else if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        "-xJf"
    } else if name.ends_with(".tar.zst") || name.ends_with(".tzst") {
        "--zstd -xf"
    } else {
        "-xf"
    };

    let status = Command::new("tar")
        .args(flag.split_whitespace())
        .arg(archive)
        .arg("-C")
        .arg(&source_dir)
        .arg(format!(
            "--strip-components={}",
            recipe.source.strip_components
        ))
        .status()
        .with_context(|| format!("extracting {}", archive.display()))?;
    if !status.success() {
        bail!("failed to extract {}", archive.display());
    }
    Ok(source_dir)
}

fn create_stage0_tarball(sysroot: &Path, output: &Path) -> Result<()> {
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let status = Command::new("tar")
        .arg("--zstd")
        .arg("-cf")
        .arg(output)
        .arg("-C")
        .arg(sysroot)
        .arg(".")
        .status()
        .context("creating stage 0 tarball")?;
    if !status.success() {
        bail!("failed to create stage 0 tarball {}", output.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_placeholders() {
        let config = BootstrapConfig::default();
        let command = substitute(
            "make -j{jobs} SYSROOT={sysroot} SRC={source} P={prefix}",
            &config,
            Path::new("/work"),
            Path::new("/sysroot"),
            Path::new("/src"),
            "/usr",
        );
        assert!(command.contains(&format!("-j{}", config.jobs())));
        assert!(command.contains("SYSROOT=/sysroot"));
        assert!(command.contains("SRC=/src"));
        assert!(command.contains("P=/usr"));
    }
}
