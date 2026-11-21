//! Bootstrapper configuration (`bootstrapper.toml`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Which C library edition to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Libc {
    Glibc,
    Musl,
}

impl Libc {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Glibc => "glibc",
            Self::Musl => "musl",
        }
    }

    pub fn parse(input: &str) -> Result<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "glibc" => Ok(Self::Glibc),
            "musl" => Ok(Self::Musl),
            other => anyhow::bail!("unknown libc edition {other:?} (expected glibc or musl)"),
        }
    }
}

/// Target architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }

    pub fn parse(input: &str) -> Result<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "x86_64" | "amd64" => Ok(Self::X86_64),
            "aarch64" | "arm64" => Ok(Self::Aarch64),
            other => anyhow::bail!("unknown architecture {other:?}"),
        }
    }
}

/// Parsed bootstrap configuration.
#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    pub libc: Libc,
    pub arch: Arch,
    pub workdir: PathBuf,
    pub jobs: usize,
    pub stage0_tarball: Option<String>,
    pub source_mirrors: Vec<String>,
    pub artifacts: PathBuf,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            libc: Libc::Glibc,
            arch: Arch::X86_64,
            workdir: PathBuf::from(crate::DEFAULT_WORKDIR),
            jobs: 0,
            stage0_tarball: None,
            source_mirrors: Vec::new(),
            artifacts: PathBuf::from("artifacts"),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawConfig {
    bootstrap: RawBootstrap,
    mirrors: RawMirrors,
    output: RawOutput,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawBootstrap {
    libc: Option<String>,
    arch: Option<String>,
    workdir: Option<PathBuf>,
    jobs: Option<usize>,
    stage0_tarball: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawMirrors {
    source: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawOutput {
    artifacts: Option<PathBuf>,
}

impl BootstrapConfig {
    /// Load `bootstrapper.toml` if present, otherwise use defaults.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let raw: RawConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let mut config = Self::default();
        if let Some(libc) = raw.bootstrap.libc {
            config.libc = Libc::parse(&libc)?;
        }
        if let Some(arch) = raw.bootstrap.arch {
            config.arch = Arch::parse(&arch)?;
        }
        if let Some(workdir) = raw.bootstrap.workdir {
            config.workdir = workdir;
        }
        if let Some(jobs) = raw.bootstrap.jobs {
            config.jobs = jobs;
        }
        config.stage0_tarball = raw.bootstrap.stage0_tarball;
        config.source_mirrors = raw.mirrors.source;
        if let Some(artifacts) = raw.output.artifacts {
            config.artifacts = artifacts;
        }
        Ok(config)
    }

    /// Effective job count (falls back to available parallelism).
    pub fn jobs(&self) -> usize {
        if self.jobs == 0 {
            std::thread::available_parallelism()
                .map(|value| value.get())
                .unwrap_or(1)
        } else {
            self.jobs
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bootstrapper_toml() {
        let text = r#"
[bootstrap]
libc = "musl"
arch = "aarch64"
workdir = "/tmp/evergreen"
jobs = 4

[mirrors]
source = ["https://mirror.example/sources"]

[output]
artifacts = "/tmp/artifacts"
"#;
        let parsed: RawConfig = toml::from_str(text).unwrap();
        assert_eq!(parsed.bootstrap.libc.as_deref(), Some("musl"));
        assert_eq!(parsed.mirrors.source.len(), 1);
        assert_eq!(
            parsed.output.artifacts.unwrap(),
            PathBuf::from("/tmp/artifacts")
        );
    }

    #[test]
    fn loads_file() {
        let dir = std::env::temp_dir().join(format!(
            "bs-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bootstrapper.toml");
        std::fs::write(&path, "[bootstrap]\nlibc = \"musl\"\n").unwrap();
        let config = BootstrapConfig::load(&path).unwrap();
        assert_eq!(config.libc, Libc::Musl);
        assert!(config.jobs() > 0);
        std::fs::remove_dir_all(dir).ok();
    }
}
