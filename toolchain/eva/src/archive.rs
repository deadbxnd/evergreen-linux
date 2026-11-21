//! `.eva` package archive creation, inspection, and extraction.
//!
//! An archive is a zstd-compressed tar with a top-level `META.toml` and a
//! `payload/` tree whose paths are relative to the system root.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::symlink;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::{Archive, Builder, Header};

use crate::config::Paths;
use crate::model::{
    Candidate, Conflict, Dependency, FileEntry, OptionalDep, PackageId, PackageMeta, Provide,
    SymlinkEntry,
};
use crate::repo::{fetch, RepoConfig};

/// TOML-friendly view of a candidate. `None` values are omitted because TOML
/// has no null representation.
#[derive(Debug, Serialize, Deserialize)]
struct MetaDocument {
    id: PackageId,
    meta: PackageMeta,
    #[serde(default)]
    deps: Vec<Dependency>,
    #[serde(default)]
    opt_deps: Vec<OptionalDep>,
    #[serde(default)]
    recommends: Vec<Dependency>,
    #[serde(default)]
    suggests: Vec<Dependency>,
    #[serde(default)]
    conflicts: Vec<Conflict>,
    #[serde(default)]
    provides: Vec<ProvideDocument>,
    #[serde(default)]
    replaces: Vec<String>,
    #[serde(default)]
    files: Vec<FileEntry>,
    #[serde(default)]
    symlinks: Vec<SymlinkEntry>,
    #[serde(default)]
    hooks: HookDocument,
    #[serde(default)]
    archive: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProvideDocument {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<evergreen_common::Version>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HookDocument {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pre_install: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    post_install: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pre_remove: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    post_remove: Option<String>,
}

impl From<&Candidate> for MetaDocument {
    fn from(candidate: &Candidate) -> Self {
        Self {
            id: candidate.id.clone(),
            meta: candidate.meta.clone(),
            deps: candidate.deps.clone(),
            opt_deps: candidate.opt_deps.clone(),
            recommends: candidate.recommends.clone(),
            suggests: candidate.suggests.clone(),
            conflicts: candidate.conflicts.clone(),
            provides: candidate
                .provides
                .iter()
                .map(|provide| ProvideDocument {
                    name: provide.name.clone(),
                    version: provide.version.clone(),
                })
                .collect(),
            replaces: candidate.replaces.clone(),
            files: candidate.files.clone(),
            symlinks: candidate.symlinks.clone(),
            hooks: HookDocument {
                pre_install: candidate.hooks.pre_install.clone(),
                post_install: candidate.hooks.post_install.clone(),
                pre_remove: candidate.hooks.pre_remove.clone(),
                post_remove: candidate.hooks.post_remove.clone(),
            },
            archive: candidate.archive.clone(),
        }
    }
}

impl From<MetaDocument> for Candidate {
    fn from(document: MetaDocument) -> Self {
        Self {
            id: document.id,
            meta: document.meta,
            deps: document.deps,
            opt_deps: document.opt_deps,
            recommends: document.recommends,
            suggests: document.suggests,
            conflicts: document.conflicts,
            provides: document
                .provides
                .into_iter()
                .map(|provide| Provide {
                    name: provide.name,
                    version: provide.version,
                })
                .collect(),
            replaces: document.replaces,
            files: document.files,
            symlinks: document.symlinks,
            hooks: crate::model::Hooks {
                pre_install: document.hooks.pre_install,
                post_install: document.hooks.post_install,
                pre_remove: document.hooks.pre_remove,
                post_remove: document.hooks.post_remove,
            },
            archive: document.archive,
        }
    }
}

/// Compute the SHA-256 of a file.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Write a `.eva` package from a staging directory.
pub fn write_package(path: &Path, candidate: &Candidate, payload_dir: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(path)?;
    let encoder = zstd::stream::write::Encoder::new(file, 3)?;
    let mut builder = Builder::new(encoder);

    let meta = toml::to_string_pretty(&MetaDocument::from(candidate))?;
    let mut header = Header::new_gnu();
    header.set_size(meta.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, "META.toml", meta.as_bytes())?;

    let mut files = Vec::new();
    collect_files(payload_dir, payload_dir, &mut files)?;
    files.sort();
    for relative in files {
        let absolute = payload_dir.join(&relative);
        builder.append_path_with_name(&absolute, format!("payload/{relative}"))?;
    }

    let mut links = Vec::new();
    collect_symlinks(payload_dir, payload_dir, &mut links)?;
    links.sort();
    for (relative, target) in links {
        let mut header = Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_link_name(&target)?;
        header.set_size(0);
        header.set_mode(0o777);
        header.set_cksum();
        builder.append_data(&mut header, format!("payload/{relative}"), std::io::empty())?;
    }

    let encoder = builder.into_inner()?;
    encoder.finish()?;
    Ok(())
}

fn collect_files(base: &Path, dir: &Path, files: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            collect_files(base, &path, files)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            files.push(relative);
        }
    }
    Ok(())
}

fn collect_symlinks(base: &Path, dir: &Path, links: &mut Vec<(String, String)>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            let relative = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let target = std::fs::read_link(&path)?.to_string_lossy().into_owned();
            links.push((relative, target));
        } else if metadata.is_dir() {
            collect_symlinks(base, &path, links)?;
        }
    }
    Ok(())
}

/// Read the metadata from a package without extracting its payload.
pub fn read_meta(path: &Path) -> Result<Candidate> {
    let file = File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut archive = Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.path()?.to_string_lossy() == "META.toml" {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            let document: MetaDocument = toml::from_str(&text)?;
            return Ok(document.into());
        }
    }
    bail!("package {} has no META.toml", path.display())
}

/// Verify the package checksum, then extract it into `destination`.
///
/// Returns the metadata from `META.toml`. Extracted paths are validated to
/// stay inside `destination`; symlinks and hardlinks are rejected.
pub fn extract_package(
    path: &Path,
    destination: &Path,
    expected_sha256: &str,
) -> Result<Candidate> {
    if !expected_sha256.is_empty() {
        let actual = sha256_file(path)?;
        if actual != expected_sha256 {
            bail!("package checksum mismatch: expected {expected_sha256}, got {actual}");
        }
    }

    std::fs::create_dir_all(destination)?;
    let file = File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut archive = Archive::new(decoder);
    let mut metadata = None;

    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.to_string_lossy().replace('\\', "/");

        if entry_path == "META.toml" {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            let document: MetaDocument = toml::from_str(&text)?;
            metadata = Some(document.into());
            continue;
        }

        let relative = match entry_path.strip_prefix("payload/") {
            Some(relative) if !relative.is_empty() => relative,
            _ => continue,
        };
        let relative_path = Path::new(relative);
        if !safe_relative(relative_path) {
            bail!("unsafe path in package: {relative}");
        }
        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() {
            let link_target = entry
                .link_name()?
                .map(|name| name.to_string_lossy().into_owned())
                .ok_or_else(|| anyhow!("symlink {relative} has no target"))?;
            if !safe_link_target(relative_path, &link_target) {
                bail!("unsafe symlink target in package: {relative} -> {link_target}");
            }
            let link_path = destination.join(relative_path);
            if let Some(parent) = link_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if std::fs::symlink_metadata(&link_path).is_ok() {
                std::fs::remove_file(&link_path)?;
            }
            symlink(&link_target, &link_path)?;
            continue;
        }
        if !entry_type.is_file() {
            // Directory, PAX/GNU metadata, and other non-file entries carry no
            // payload; skipping them is safe after the path check above.
            continue;
        }

        let target = destination.join(relative_path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output = File::create(&target)?;
        std::io::copy(&mut entry, &mut output)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = entry.header().mode()? & 0o7777;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))?;
        }
    }

    metadata.ok_or_else(|| anyhow!("package {} has no META.toml", path.display()))
}

fn safe_relative(path: &Path) -> bool {
    if path.as_os_str().is_empty() {
        return false;
    }
    path.components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

/// Symlink targets must stay inside the system root. Absolute targets would
/// escape the staging root and point at the host filesystem; `..` is allowed
/// only while it does not climb above the root.
fn safe_link_target(link_path: &Path, target: &str) -> bool {
    let path = Path::new(target);
    if path.is_absolute() || target.is_empty() {
        return false;
    }

    let mut depth: i32 = link_path
        .parent()
        .map(|parent| {
            parent
                .components()
                .filter(|component| matches!(component, Component::Normal(_)))
                .count() as i32
        })
        .unwrap_or(0);

    for component in path.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Recompute file checksums for the extracted payload and attach them to the
/// candidate metadata, keeping it consistent with the index.
pub fn with_payload_checksums(mut candidate: Candidate, payload_dir: &Path) -> Result<Candidate> {
    candidate.files.clear();
    let mut paths = Vec::new();
    collect_files(payload_dir, payload_dir, &mut paths)?;
    paths.sort();
    candidate.symlinks.clear();
    let mut links = Vec::new();
    collect_symlinks(payload_dir, payload_dir, &mut links)?;
    links.sort();
    for (relative, target) in links {
        if relative == "META.toml" || relative.starts_with("META.toml/") {
            continue;
        }
        candidate.symlinks.push(SymlinkEntry {
            path: relative,
            target,
            mode: 0o777,
        });
    }

    for relative in paths {
        if relative == "META.toml" || relative.starts_with("META.toml/") {
            continue;
        }
        let absolute = payload_dir.join(&relative);
        let metadata = std::fs::metadata(&absolute)?;
        let mode = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o7777
            }
            #[cfg(not(unix))]
            {
                0o644
            }
        };
        candidate.files.push(crate::model::FileEntry {
            path: relative,
            sha256: sha256_file(&absolute)?,
            mode,
        });
    }
    Ok(candidate)
}

/// Download (or reuse) a package archive in the local cache.
pub fn fetch_package(
    candidate: &Candidate,
    repos: &[RepoConfig],
    paths: &Paths,
) -> Result<PathBuf> {
    let name = candidate.archive_name();
    let directory = paths.cache_dir().join("packages");
    std::fs::create_dir_all(&directory)?;
    let destination = directory.join(&name);

    if destination.exists() {
        if candidate.meta.checksum.is_empty()
            || sha256_file(&destination)? == candidate.meta.checksum
        {
            return Ok(destination);
        }
        std::fs::remove_file(&destination)?;
    }

    let repo = repos
        .get(candidate.id.repo.0 as usize)
        .or_else(|| repos.first())
        .ok_or_else(|| anyhow!("no repository configured for {}", candidate.id.name))?;
    let base = repo.url.trim_end_matches('/');
    let url = format!("{base}/{name}");
    let bytes = fetch(&url).with_context(|| format!("downloading {name}"))?;
    let digest = hex::encode(Sha256::digest(&bytes));
    if !candidate.meta.checksum.is_empty() && digest != candidate.meta.checksum {
        bail!(
            "checksum mismatch for {name}: expected {}, got {}",
            candidate.meta.checksum,
            digest
        );
    }
    std::fs::write(&destination, bytes)?;
    Ok(destination)
}

/// Write a small staging file (`META.toml` copy) for inspection/debugging.
pub fn write_meta(path: &Path, candidate: &Candidate) -> Result<()> {
    let mut file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    writeln!(
        file,
        "{}",
        toml::to_string_pretty(&MetaDocument::from(candidate))?
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Arch, PackageId, PackageMeta};
    use evergreen_common::{RepoId, Version};

    fn candidate() -> Candidate {
        Candidate {
            id: PackageId {
                name: "demo".to_string(),
                version: Version::parse("1.0").unwrap(),
                arch: Arch::NoArch,
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
    fn writes_reads_and_extracts_packages() {
        let dir = std::env::temp_dir().join(format!(
            "eva-archive-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let payload = dir.join("payload");
        std::fs::create_dir_all(payload.join("usr/bin")).unwrap();
        std::fs::write(payload.join("usr/bin/demo"), b"#!/bin/sh\necho hi\n").unwrap();
        let package = dir.join("demo.eva");
        write_package(&package, &candidate(), &payload).unwrap();

        let meta = read_meta(&package).unwrap();
        assert_eq!(meta.id.name, "demo");

        let out = dir.join("out");
        let extracted = extract_package(&package, &out, "").unwrap();
        assert_eq!(extracted.id.name, "demo");
        assert!(out.join("usr/bin/demo").exists());

        let checksum = sha256_file(&package).unwrap();
        assert!(extract_package(&package, &out, "deadbeef").is_err());
        extract_package(&package, &out, &checksum).unwrap();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn round_trips_usr_merge_symlinks() {
        let dir = std::env::temp_dir().join(format!(
            "eva-symlink-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let payload = dir.join("payload");
        std::fs::create_dir_all(payload.join("usr/bin")).unwrap();
        std::fs::write(payload.join("usr/bin/demo"), b"demo\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("usr/bin", payload.join("bin")).unwrap();

        let package = dir.join("merge.eva");
        let candidate = with_payload_checksums(candidate(), &payload).unwrap();
        write_package(&package, &candidate, &payload).unwrap();
        let meta = read_meta(&package).unwrap();
        assert_eq!(meta.symlinks.len(), 1);
        assert_eq!(meta.symlinks[0].target, "usr/bin");

        let out = dir.join("out");
        extract_package(&package, &out, "").unwrap();
        assert_eq!(
            std::fs::read_link(out.join("bin")).unwrap(),
            PathBuf::from("usr/bin")
        );
        assert!(out.join("bin/demo").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rejects_unsafe_relative_paths() {
        assert!(!safe_relative(Path::new("../etc/passwd")));
        assert!(!safe_relative(Path::new("/etc/passwd")));
        assert!(safe_relative(Path::new("usr/bin/demo")));
    }
}
