//! Repository index generation, signing and key management.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use rand_core::OsRng;

use crate::archive::{read_meta, sha256_file};
use crate::model::{Condition, DepKind, Dependency};
use crate::repo::{DepValue, IndexFileEntry, IndexMetadata, IndexPackage, RepoIndex};

/// Build a repository index from every `.eva` archive in `package_dir`.
pub fn build_index(package_dir: &Path, name: &str, branch: &str) -> Result<RepoIndex> {
    if !package_dir.is_dir() {
        bail!("package directory {} does not exist", package_dir.display());
    }

    let mut packages = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(package_dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("eva"))
        .collect();
    entries.sort();

    for path in entries {
        let candidate = read_meta(&path)
            .with_context(|| format!("reading metadata from {}", path.display()))?;
        let checksum = sha256_file(&path)?;
        let size_download = std::fs::metadata(&path)?.len();
        let archive = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();

        packages.push(index_package(candidate, checksum, size_download, archive));
    }

    packages.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));
    Ok(RepoIndex {
        metadata: IndexMetadata {
            name: name.to_string(),
            branch: branch.to_string(),
            generated: Some(chrono::Utc::now().to_rfc3339()),
        },
        packages,
    })
}

fn index_package(
    candidate: crate::model::Candidate,
    checksum: String,
    size_download: u64,
    archive: String,
) -> IndexPackage {
    let mut dependencies = std::collections::BTreeMap::new();
    for dep in &candidate.deps {
        dependencies.insert(dep.name.clone(), dep_to_index(dep));
    }
    for optional in &candidate.opt_deps {
        let mut value = dep_to_index(&optional.dependency);
        if let DepValue::Detailed { condition, .. } = &mut value {
            *condition = condition_to_string(&optional.condition);
        }
        dependencies.insert(optional.dependency.name.clone(), value);
    }

    let recommends = candidate
        .recommends
        .iter()
        .map(|dep| (dep.name.clone(), dep_to_index(dep)))
        .collect();
    let suggests = candidate
        .suggests
        .iter()
        .map(|dep| (dep.name.clone(), dep_to_index(dep)))
        .collect();
    let provides = candidate
        .provides
        .iter()
        .map(|provide| {
            (
                provide.name.clone(),
                provide
                    .version
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "*".to_string()),
            )
        })
        .collect();
    let conflicts = candidate
        .conflicts
        .iter()
        .map(|conflict| (conflict.name.clone(), conflict.constraint.to_string()))
        .collect();
    let files = candidate
        .files
        .iter()
        .map(|file| IndexFileEntry {
            path: file.path.clone(),
            sha256: file.sha256.clone(),
            mode: file.mode,
        })
        .collect();

    IndexPackage {
        name: candidate.id.name.clone(),
        version: candidate.id.version.to_string(),
        release: candidate.id.version.release,
        arch: candidate.id.arch.to_string(),
        description: candidate.meta.description.clone(),
        url: candidate.meta.url.clone(),
        license: candidate.meta.license.clone(),
        maintainer: candidate.meta.maintainer.clone(),
        checksum,
        size_download,
        size_installed: candidate.meta.size_installed,
        archive,
        dependencies,
        recommends,
        suggests,
        provides,
        conflicts,
        replaces: candidate.replaces.clone(),
        files,
        symlinks: candidate.symlinks.clone(),
        hooks: candidate.hooks.clone(),
    }
}

fn dep_to_index(dep: &Dependency) -> DepValue {
    DepValue::Detailed {
        constraint: dep.constraint.to_string(),
        optional: dep.kind == DepKind::Optional,
        condition: None,
        kind: Some(match dep.kind {
            DepKind::Required => "required".to_string(),
            DepKind::Optional => "optional".to_string(),
            DepKind::Recommend => "recommend".to_string(),
            DepKind::Suggest => "suggest".to_string(),
        }),
    }
}

fn condition_to_string(condition: &Condition) -> Option<String> {
    match condition {
        Condition::Always => None,
        Condition::Feature(name) => Some(format!("feature:{name}")),
        Condition::PackagePresent(name) => Some(format!("package:{name}")),
    }
}

/// Write `index.toml` and an optional detached ed25519 signature atomically.
pub fn write_signed_index(
    directory: &Path,
    index: &RepoIndex,
    signing_key: Option<&Path>,
) -> Result<(PathBuf, Option<PathBuf>)> {
    std::fs::create_dir_all(directory)?;
    let text = toml::to_string_pretty(index)?;
    let index_path = directory.join("index.toml");
    let temp = directory.join("index.toml.tmp");
    std::fs::write(&temp, text.as_bytes())?;
    std::fs::rename(&temp, &index_path)?;

    let Some(key_path) = signing_key else {
        return Ok((index_path, None));
    };

    let key_hex = std::fs::read_to_string(key_path)
        .with_context(|| format!("reading signing key {}", key_path.display()))?;
    let key_hex: String = key_hex
        .lines()
        .find(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .unwrap_or_default()
        .trim()
        .to_string();
    let bytes = hex::decode(&key_hex).context("signing key must be hex")?;
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("signing key must be 32 bytes"))?;
    let key = SigningKey::from_bytes(&array);
    let signature = key.sign(text.as_bytes());
    let signature_path = directory.join("index.toml.sig");
    let temp_sig = directory.join("index.toml.sig.tmp");
    std::fs::write(&temp_sig, hex::encode(signature.to_bytes()))?;
    std::fs::rename(temp_sig, &signature_path)?;

    Ok((index_path, Some(signature_path)))
}

/// Generate a new ed25519 keypair. The secret key is written with mode 0600.
pub fn generate_key_pair(directory: &Path, label: &str) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(directory)?;
    let signing = SigningKey::generate(&mut OsRng);
    let secret_hex = hex::encode(signing.to_bytes());
    let public_hex = hex::encode(signing.verifying_key().to_bytes());

    let secret_path = directory.join(format!("{label}.key"));
    let public_path = directory.join(format!("{label}.pub"));
    std::fs::write(&public_path, format!("{public_hex}\n"))?;
    std::fs::write(&secret_path, format!("{secret_hex}\n"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok((secret_path, public_path))
}

/// Human-readable fingerprint for a public or secret key hex string.
pub fn fingerprint(key: &str) -> Result<String> {
    let text = key
        .lines()
        .find(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .unwrap_or_default()
        .trim();
    let bytes = hex::decode(text).context("key must be hex")?;
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("key must be 32 bytes"))?;
    let verifying = VerifyingKey::from_bytes(&array).context("invalid ed25519 key")?;
    Ok(hex::encode(verifying.to_bytes())[..16].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::write_package;
    use crate::model::{Arch, Candidate, PackageId, PackageMeta};
    use evergreen_common::RepoId;

    fn candidate() -> Candidate {
        Candidate {
            id: PackageId {
                name: "demo".to_string(),
                version: "1.0".parse().unwrap(),
                arch: Arch::X86_64,
                repo: RepoId(0),
            },
            meta: PackageMeta {
                description: "Demo".to_string(),
                size_installed: 42,
                ..PackageMeta::default()
            },
            deps: vec![Dependency {
                name: "base".to_string(),
                constraint: "*".parse().unwrap(),
                kind: DepKind::Required,
            }],
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
    fn builds_index_from_packages() {
        let dir = std::env::temp_dir().join(format!(
            "eva-index-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let payload = dir.join("payload");
        let packages = dir.join("packages");
        std::fs::create_dir_all(&payload).unwrap();
        std::fs::create_dir_all(&packages).unwrap();
        std::fs::write(payload.join("demo.txt"), b"demo").unwrap();
        write_package(&packages.join("demo-1.0.eva"), &candidate(), &payload).unwrap();

        let index = build_index(&packages, "core", "stable").unwrap();
        assert_eq!(index.packages.len(), 1);
        assert_eq!(index.packages[0].name, "demo");
        assert_eq!(index.packages[0].checksum.len(), 64);
        assert!(index.packages[0].dependencies.contains_key("base"));

        let (secret, public) = generate_key_pair(&dir, "test").unwrap();
        let (index_path, sig_path) = write_signed_index(&packages, &index, Some(&secret)).unwrap();
        assert!(index_path.exists());
        let signature = std::fs::read_to_string(sig_path.unwrap()).unwrap();
        let paths = crate::config::Paths::for_root(&dir);
        crate::repo::add_key(&paths, &public.to_string_lossy()).unwrap();
        let data = std::fs::read(&index_path).unwrap();
        crate::repo::verify_signature(&data, &signature, &paths).unwrap();

        std::fs::remove_dir_all(dir).ok();
    }
}
