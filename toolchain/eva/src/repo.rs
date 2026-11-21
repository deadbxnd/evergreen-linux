//! Repository configuration, index loading, fetching, and signature checks.
//!
//! An index is a TOML file (`index.toml`) accompanied by a detached ed25519
//! signature (`index.toml.sig`, hex encoded). Signatures are verified against
//! every key in `<root>/etc/eva/keys/*.pub` unless verification is disabled.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::{Signature, VerifyingKey};
use evergreen_common::{Branch, RepoId};
use serde::{Deserialize, Serialize};

use crate::config::Paths;
use crate::model::{
    normalize, Arch, Candidate, Condition, Conflict, DepKind, Dependency, FileEntry, Hooks,
    OptionalDep, PackageId, PackageMeta, Provide, SymlinkEntry,
};

/// One configured repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    pub name: String,
    pub url: String,
    #[serde(default = "default_branch")]
    pub branch: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_branch() -> String {
    "stable".to_string()
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(default)]
struct RepoConfigFile {
    repo: Vec<RepoConfig>,
}

/// Load repository configuration from `<root>/etc/eva/repos.toml`.
pub fn load_config(path: &Path) -> Result<Vec<RepoConfig>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    let file: RepoConfigFile = toml::from_str(&text)?;
    Ok(file.repo)
}

/// Write repository configuration atomically.
pub fn save_config(path: &Path, repos: &[RepoConfig]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = RepoConfigFile {
        repo: repos.to_vec(),
    };
    let text = toml::to_string_pretty(&file)?;
    let temp = path.with_extension("toml.tmp");
    std::fs::write(&temp, text)?;
    std::fs::rename(temp, path)?;
    Ok(())
}

/// In-memory repository index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoIndex {
    pub metadata: IndexMetadata,
    pub packages: Vec<IndexPackage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMetadata {
    pub name: String,
    #[serde(default = "default_branch")]
    pub branch: String,
    #[serde(default)]
    pub generated: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexPackage {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub release: u32,
    pub arch: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub license: Vec<String>,
    #[serde(default)]
    pub maintainer: String,
    #[serde(default)]
    pub checksum: String,
    #[serde(default)]
    pub size_download: u64,
    #[serde(default)]
    pub size_installed: u64,
    #[serde(default)]
    pub archive: String,
    #[serde(default)]
    pub dependencies: BTreeMap<String, DepValue>,
    #[serde(default)]
    pub recommends: BTreeMap<String, DepValue>,
    #[serde(default)]
    pub suggests: BTreeMap<String, DepValue>,
    #[serde(default)]
    pub provides: BTreeMap<String, String>,
    #[serde(default)]
    pub conflicts: BTreeMap<String, String>,
    #[serde(default)]
    pub replaces: Vec<String>,
    #[serde(default)]
    pub files: Vec<IndexFileEntry>,
    #[serde(default)]
    pub symlinks: Vec<SymlinkEntry>,
    #[serde(default)]
    pub hooks: Hooks,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexFileEntry {
    pub path: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default = "default_mode")]
    pub mode: u32,
}

fn default_mode() -> u32 {
    0o644
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DepValue {
    Constraint(String),
    Detailed {
        #[serde(default)]
        constraint: String,
        #[serde(default)]
        optional: bool,
        #[serde(default)]
        condition: Option<String>,
        #[serde(default)]
        kind: Option<String>,
    },
}

impl DepValue {
    pub(crate) fn into_dependency(self, name: &str, default_kind: DepKind) -> Result<Dependency> {
        let (constraint, optional, _condition, kind) = match self {
            Self::Constraint(value) => (value, false, None, None),
            Self::Detailed {
                constraint,
                optional,
                condition,
                kind,
            } => (constraint, optional, condition, kind),
        };
        let kind = if optional {
            DepKind::Optional
        } else {
            match kind.as_deref().unwrap_or("required") {
                "recommend" => DepKind::Recommend,
                "suggest" => DepKind::Suggest,
                "optional" => DepKind::Optional,
                _ => default_kind,
            }
        };
        Ok(Dependency {
            name: normalize(name),
            constraint: constraint.parse()?,
            kind,
        })
    }
}

/// Parse an index from TOML text.
pub fn parse_index(text: &str) -> Result<RepoIndex> {
    Ok(toml::from_str(text)?)
}

/// Convert an index entry into a solver candidate.
pub fn candidate_from_index(
    entry: &IndexPackage,
    _repo: &RepoConfig,
    id: RepoId,
) -> Result<Candidate> {
    let version: evergreen_common::Version = entry.version.parse()?;
    let arch = Arch::parse(&entry.arch)?;

    let mut deps = Vec::new();
    let mut opt_deps = Vec::new();
    for (name, value) in &entry.dependencies {
        let dependency = value.clone().into_dependency(name, DepKind::Required)?;
        if dependency.kind == DepKind::Optional {
            let condition = match &value {
                DepValue::Detailed { condition, .. } => condition
                    .as_deref()
                    .map(Condition::parse)
                    .unwrap_or(Condition::Always),
                _ => Condition::Always,
            };
            opt_deps.push(OptionalDep {
                dependency,
                condition,
            });
        } else {
            deps.push(dependency);
        }
    }

    let recommends = entry
        .recommends
        .iter()
        .map(|(name, value)| value.clone().into_dependency(name, DepKind::Recommend))
        .collect::<Result<Vec<_>>>()?;
    let suggests = entry
        .suggests
        .iter()
        .map(|(name, value)| value.clone().into_dependency(name, DepKind::Suggest))
        .collect::<Result<Vec<_>>>()?;

    let provides = entry
        .provides
        .iter()
        .map(|(name, version)| {
            Ok(Provide {
                name: normalize(name),
                version: if version.is_empty() || version == "*" {
                    None
                } else {
                    Some(version.parse()?)
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let conflicts = entry
        .conflicts
        .iter()
        .map(|(name, constraint)| {
            Ok(Conflict {
                name: normalize(name),
                constraint: constraint.parse()?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let files = entry
        .files
        .iter()
        .map(|file| FileEntry {
            path: file.path.clone(),
            sha256: file.sha256.clone(),
            mode: file.mode,
        })
        .collect();

    Ok(Candidate {
        id: PackageId {
            name: normalize(&entry.name),
            version,
            arch,
            repo: id,
        },
        meta: PackageMeta {
            description: entry.description.clone(),
            url: entry.url.clone(),
            license: entry.license.clone(),
            maintainer: entry.maintainer.clone(),
            size_installed: entry.size_installed,
            size_download: entry.size_download,
            checksum: entry.checksum.clone(),
        },
        deps,
        opt_deps,
        recommends,
        suggests,
        conflicts,
        provides,
        replaces: entry.replaces.iter().map(|name| normalize(name)).collect(),
        files,
        symlinks: entry.symlinks.clone(),
        hooks: entry.hooks.clone(),
        archive: entry.archive.clone(),
    })
}

/// Load all enabled cached indexes and build the candidate pool.
///
/// Candidates are filtered by branch and architecture. When the same
/// `(name, version, arch)` appears in several repositories, the copy from the
/// highest-priority repository (lowest `priority` number) wins.
pub fn load_candidates(paths: &Paths, repos: &[RepoConfig]) -> Result<Vec<Candidate>> {
    let mut best: BTreeMap<(String, String, Arch), (i32, Candidate)> = BTreeMap::new();

    for (position, repo) in repos.iter().enumerate() {
        if !repo.enabled {
            continue;
        }
        let dir = paths.cache_dir().join(&repo.name);
        let index_path = dir.join("index.toml");
        if !index_path.exists() {
            continue;
        }
        let index = parse_index(&std::fs::read_to_string(&index_path)?)?;
        let branch_ok = repo.branch.eq_ignore_ascii_case(&index.metadata.branch)
            || repo.branch.eq_ignore_ascii_case("any");
        if !branch_ok {
            continue;
        }

        let id = RepoId(position as u32);
        for entry in &index.packages {
            let arch = Arch::parse(&entry.arch)?;
            if arch != Arch::NoArch && arch != Arch::host() {
                continue;
            }
            let candidate = candidate_from_index(entry, repo, id)?;
            let key = (
                candidate.id.name.clone(),
                candidate.id.version.to_string(),
                arch,
            );
            let insert = match best.get(&key) {
                Some((priority, _)) => repo.priority < *priority,
                None => true,
            };
            if insert {
                best.insert(key, (repo.priority, candidate));
            }
        }
    }

    Ok(best.into_values().map(|(_, candidate)| candidate).collect())
}

/// Download a URL into memory. Supports `file://`, plain paths, and HTTP(S).
pub fn fetch(url: &str) -> Result<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return Ok(std::fs::read(path)?);
    }
    if !url.contains("://") {
        return Ok(std::fs::read(url)?);
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        let response = ureq::get(url)
            .call()
            .map_err(|err| anyhow!(err.to_string()))?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut bytes)
            .context("reading HTTP response")?;
        return Ok(bytes);
    }
    bail!("unsupported repository URL scheme: {url}")
}

fn join_url(base: &str, file: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{file}")
    } else {
        format!("{base}/{file}")
    }
}

/// Synchronize one repository's index into the cache.
pub fn sync_repo(paths: &Paths, repo: &RepoConfig, verify: bool) -> Result<usize> {
    let bytes = fetch(&join_url(&repo.url, "index.toml"))
        .with_context(|| format!("fetching index for {}", repo.name))?;
    let index = parse_index(std::str::from_utf8(&bytes)?)?;

    if verify {
        let signature_bytes = fetch(&join_url(&repo.url, "index.toml.sig"))
            .with_context(|| format!("fetching signature for {}", repo.name))?;
        let signature_text = String::from_utf8(signature_bytes)?;
        verify_signature(&bytes, signature_text.trim(), paths)?;
    }

    let dir = paths.cache_dir().join(&repo.name);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("index.toml"), &bytes)?;
    Ok(index.packages.len())
}

/// Verify a detached hex ed25519 signature against all installed keys.
///
/// Verification succeeds only when at least one trusted key validates it; when
/// no keys are installed it fails (unless the caller disabled verification).
pub fn verify_signature(data: &[u8], signature_hex: &str, paths: &Paths) -> Result<()> {
    let signature_bytes = hex::decode(signature_hex.trim())?;
    let signature_array: [u8; 64] = signature_bytes
        .try_into()
        .map_err(|_| anyhow!("signature must be 64 bytes"))?;
    let signature = Signature::from_bytes(&signature_array);

    let key_dir = paths.key_dir();
    let mut verified = false;
    if let Ok(entries) = std::fs::read_dir(&key_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("pub") {
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(_) => continue,
            };
            let key_hex: String = text
                .lines()
                .find(|line| !line.trim().is_empty() && !line.starts_with('#'))
                .unwrap_or_default()
                .trim()
                .to_string();
            let bytes = match hex::decode(key_hex) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            let array: [u8; 32] = match bytes.try_into() {
                Ok(array) => array,
                Err(_) => continue,
            };
            if let Ok(key) = VerifyingKey::from_bytes(&array) {
                if key.verify_strict(data, &signature).is_ok() {
                    verified = true;
                    break;
                }
            }
        }
    }

    if !verified {
        bail!("repository signature could not be verified by any trusted key");
    }
    Ok(())
}

/// Add a public key from a file or raw hex string.
pub fn add_key(paths: &Paths, input: &str) -> Result<PathBuf> {
    let text = if Path::new(input).exists() {
        std::fs::read_to_string(input)?
    } else {
        input.to_string()
    };
    let hex_text: String = text
        .lines()
        .find(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .unwrap_or_default()
        .trim()
        .to_string();
    let bytes = hex::decode(&hex_text).context("public key must be 32 bytes of hex")?;
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow!("public key must be 32 bytes"))?;
    VerifyingKey::from_bytes(&array).context("invalid ed25519 public key")?;

    let dir = paths.key_dir();
    std::fs::create_dir_all(&dir)?;
    let destination = dir.join(format!("{hex_text}.pub"));
    std::fs::write(&destination, format!("{hex_text}\n"))?;
    Ok(destination)
}

/// List trusted key files with their fingerprints.
pub fn list_keys(paths: &Paths) -> Result<Vec<(PathBuf, String)>> {
    let mut keys = Vec::new();
    if let Ok(entries) = std::fs::read_dir(paths.key_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("pub") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let fingerprint: String = text
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .chars()
                .take(16)
                .collect();
            keys.push((path, fingerprint));
        }
    }
    keys.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(keys)
}

/// Branch from its textual name.
pub fn parse_branch(value: &str) -> Branch {
    if value.eq_ignore_ascii_case("edge") || value.eq_ignore_ascii_case("rolling") {
        Branch::Edge
    } else {
        Branch::Stable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_index_with_dependencies() {
        let text = r#"
[metadata]
name = "core"
branch = "stable"

[[packages]]
name = "demo"
version = "1.2"
arch = "x86_64"
description = "Demo package"
checksum = "abc"
size_installed = 100
size_download = 50
[packages.dependencies]
libc = ">=2.30"
media = { optional = true, condition = "feature:media" }
[packages.provides]
demo-virtual = "1.2"
[packages.conflicts]
demo-old = "*"
"#;
        let index = parse_index(text).unwrap();
        let repo = RepoConfig {
            name: "core".to_string(),
            url: "file:///tmp".to_string(),
            branch: "stable".to_string(),
            priority: 0,
            enabled: true,
        };
        let candidate = candidate_from_index(&index.packages[0], &repo, RepoId(0)).unwrap();
        assert_eq!(candidate.name(), "demo");
        assert_eq!(candidate.deps.len(), 1);
        assert_eq!(candidate.opt_deps.len(), 1);
        assert_eq!(
            candidate.opt_deps[0].condition,
            Condition::Feature("media".to_string())
        );
        assert_eq!(candidate.conflicts.len(), 1);
        assert_eq!(candidate.provides[0].name, "demo-virtual");
    }

    #[test]
    fn saves_and_loads_repo_config() {
        let dir = std::env::temp_dir().join(format!(
            "eva-repo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("repos.toml");
        let repos = vec![RepoConfig {
            name: "core".to_string(),
            url: "file:///tmp/core".to_string(),
            branch: "stable".to_string(),
            priority: 0,
            enabled: true,
        }];
        save_config(&path, &repos).unwrap();
        let loaded = load_config(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "core");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn verifies_signatures_and_rejects_tampering() {
        use ed25519_dalek::{Signature, Signer, SigningKey};

        let dir = std::env::temp_dir().join(format!(
            "eva-sign-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let paths = Paths::for_root(&dir);
        std::fs::create_dir_all(paths.key_dir()).unwrap();
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let public = signing.verifying_key().to_bytes();
        std::fs::write(
            paths.key_dir().join("test.pub"),
            format!("{}\n", hex::encode(public)),
        )
        .unwrap();

        let data = b"index contents";
        let signature: Signature = signing.sign(data);
        let hex_signature = hex::encode(signature.to_bytes());
        verify_signature(data, &hex_signature, &paths).unwrap();
        assert!(verify_signature(b"tampered", &hex_signature, &paths).is_err());
        std::fs::remove_dir_all(dir).ok();
    }
}
