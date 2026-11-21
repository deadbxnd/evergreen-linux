//! Filesystem locations and small configuration files used by `eva`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use evergreen_common::Branch;
use serde::Deserialize;

use crate::model::{normalize, VersionConstraint};

/// Filesystem locations, rooted at `/` on a real system and at a temporary
/// directory in tests.
#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn system() -> Self {
        Self::for_root("/")
    }

    pub fn for_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve an absolute path against the configured root.
    pub fn resolve(&self, path: impl AsRef<Path>) -> PathBuf {
        let path = path.as_ref();
        if self.root == Path::new("/") {
            path.to_path_buf()
        } else if path.is_absolute() {
            self.root.join(path.strip_prefix("/").unwrap_or(path))
        } else {
            self.root.join(path)
        }
    }

    pub fn config_dir(&self) -> PathBuf {
        self.resolve("/etc/eva")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.resolve("/var/cache/eva")
    }

    pub fn db_dir(&self) -> PathBuf {
        self.resolve("/var/lib/eva")
    }

    pub fn journal_dir(&self) -> PathBuf {
        self.db_dir().join("journal")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.db_dir().join("lock")
    }

    pub fn installed_db(&self) -> PathBuf {
        self.db_dir().join("installed.db")
    }

    pub fn key_dir(&self) -> PathBuf {
        self.config_dir().join("keys")
    }

    pub fn repo_config(&self) -> PathBuf {
        self.config_dir().join("repos.toml")
    }

    pub fn eva_config(&self) -> PathBuf {
        self.config_dir().join("eva.conf")
    }

    pub fn features_file(&self) -> PathBuf {
        self.config_dir().join("features.toml")
    }

    pub fn pins_file(&self) -> PathBuf {
        self.config_dir().join("pins.toml")
    }

    pub fn holds_file(&self) -> PathBuf {
        self.config_dir().join("holds.toml")
    }
}

/// Global `eva.conf` settings.
#[derive(Debug, Clone)]
pub struct EvaConfig {
    pub branch: Branch,
    pub install_recommends: bool,
    pub verify_signatures: bool,
    pub keep_journals: usize,
}

impl Default for EvaConfig {
    fn default() -> Self {
        Self {
            branch: Branch::Stable,
            install_recommends: true,
            verify_signatures: true,
            keep_journals: 5,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawEvaConfig {
    branch: Option<String>,
    install_recommends: Option<bool>,
    verify_signatures: Option<bool>,
    keep_journals: Option<usize>,
}

impl EvaConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        let raw: RawEvaConfig = toml::from_str(&text)?;
        let mut config = Self::default();
        if let Some(branch) = raw.branch {
            config.branch = match branch.to_ascii_lowercase().as_str() {
                "edge" | "rolling" => Branch::Edge,
                _ => Branch::Stable,
            };
        }
        if let Some(value) = raw.install_recommends {
            config.install_recommends = value;
        }
        if let Some(value) = raw.verify_signatures {
            config.verify_signatures = value;
        }
        if let Some(value) = raw.keep_journals {
            config.keep_journals = value;
        }
        Ok(config)
    }
}

/// Load enabled feature flags from `/etc/eva/features.toml`.
pub fn load_features(path: &Path) -> anyhow::Result<BTreeMap<String, bool>> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct File {
        features: BTreeMap<String, bool>,
    }
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let file: File = toml::from_str(&std::fs::read_to_string(path)?)?;
    Ok(file.features)
}

/// Load version pins from `/etc/eva/pins.toml`.
pub fn load_pins(path: &Path) -> anyhow::Result<BTreeMap<String, VersionConstraint>> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct File {
        pins: BTreeMap<String, String>,
    }
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let file: File = toml::from_str(&std::fs::read_to_string(path)?)?;
    let mut pins = BTreeMap::new();
    for (name, value) in file.pins {
        pins.insert(normalize(&name), value.parse()?);
    }
    Ok(pins)
}

/// Load held packages from `/etc/eva/holds.toml`.
pub fn load_holds(path: &Path) -> anyhow::Result<BTreeSet<String>> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct File {
        holds: Vec<String>,
    }
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    let file: File = toml::from_str(&std::fs::read_to_string(path)?)?;
    Ok(file
        .holds
        .into_iter()
        .map(|name| normalize(&name))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_paths_under_root() {
        let paths = Paths::for_root("/tmp/eva-test");
        assert_eq!(
            paths.installed_db(),
            PathBuf::from("/tmp/eva-test/var/lib/eva/installed.db")
        );
        assert_eq!(
            paths.repo_config(),
            PathBuf::from("/tmp/eva-test/etc/eva/repos.toml")
        );
    }

    #[test]
    fn loads_features_and_holds() {
        let dir = std::env::temp_dir().join(format!(
            "eva-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let features = dir.join("features.toml");
        std::fs::write(&features, "[features]\nmedia = true\nnvidia = false\n").unwrap();
        assert_eq!(load_features(&features).unwrap().get("media"), Some(&true));

        let holds = dir.join("holds.toml");
        std::fs::write(&holds, "holds = [\"Kernel\", \"mesa\"]\n").unwrap();
        let holds = load_holds(&holds).unwrap();
        assert!(holds.contains("kernel"));
        std::fs::remove_dir_all(dir).ok();
    }
}
