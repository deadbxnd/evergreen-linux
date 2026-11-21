//! Source package builds (`eva build`).
//!
//! A recipe uses the same `[package]`, `[dependencies]`, `[provides]`,
//! `[conflicts]`, and `[files]` tables as a binary package, plus `[source]`
//! and `[build]` sections. Builds run in a staged directory; build
//! dependencies are resolved against the installed set before the build starts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use evergreen_common::RepoId;
use serde::{Deserialize, Serialize};

use crate::archive::{sha256_file, with_payload_checksums, write_package};
use crate::config::Paths;
use crate::model::{
    normalize, Arch, Candidate, Conflict, PackageId, PackageMeta, PackageSpec, Provide, Request,
    UpgradeMode,
};
use crate::repo::{fetch, DepValue};
use crate::solver::Resolver;

/// Parsed source recipe.
#[derive(Debug, Clone, Deserialize)]
pub struct BuildRecipe {
    pub package: RecipeHeader,
    #[serde(default)]
    pub source: SourceField,
    #[serde(default)]
    pub build: BuildSteps,
    #[serde(default)]
    pub dependencies: BTreeMap<String, DepValue>,
    #[serde(default)]
    pub recommends: BTreeMap<String, DepValue>,
    #[serde(default)]
    pub provides: BTreeMap<String, String>,
    #[serde(default)]
    pub conflicts: BTreeMap<String, String>,
    #[serde(default)]
    pub replaces: Vec<String>,
    #[serde(default)]
    pub hooks: RecipeHooks,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecipeHeader {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub release: u32,
    #[serde(default = "default_arch")]
    pub arch: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub license: LicenseField,
    #[serde(default)]
    pub maintainer: String,
}

fn default_arch() -> String {
    Arch::host().to_string()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum LicenseField {
    Text(String),
    List(Vec<String>),
}

impl Default for LicenseField {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

impl LicenseField {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::Text(text) => text
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect(),
            Self::List(list) => list,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
pub enum SourceField {
    #[default]
    Empty,
    Url(String),
    Detailed(SourceSpec),
}

impl SourceField {
    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Url(url) => Some(url),
            Self::Detailed(spec) => spec.url.as_deref(),
            Self::Empty => None,
        }
    }

    pub fn sha256(&self) -> Option<&str> {
        match self {
            Self::Detailed(spec) => spec.sha256.as_deref(),
            _ => None,
        }
    }

    pub fn set_url(&mut self, url: String) {
        match self {
            Self::Detailed(spec) => spec.url = Some(url),
            _ => {
                *self = Self::Detailed(SourceSpec {
                    url: Some(url),
                    ..SourceSpec::default()
                });
            }
        }
    }

    pub fn set_sha256(&mut self, sha256: String) {
        match self {
            Self::Detailed(spec) => spec.sha256 = Some(sha256),
            _ => {
                *self = Self::Detailed(SourceSpec {
                    sha256: Some(sha256),
                    ..SourceSpec::default()
                });
            }
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SourceSpec {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BuildSteps {
    #[serde(default)]
    pub depends: Vec<String>,
    /// Optional config file copied into the source directory as `.config`
    /// before the build runs (used by kernel recipes).
    #[serde(default)]
    pub config_file: Option<String>,
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub configure: Option<String>,
    #[serde(default = "default_prefix")]
    pub prefix: String,
    #[serde(default = "default_jobs")]
    pub jobs: u32,
}

fn default_prefix() -> String {
    "/usr".to_string()
}

fn default_jobs() -> u32 {
    std::thread::available_parallelism()
        .map(|value| value.get() as u32)
        .unwrap_or(1)
}

impl Default for BuildSteps {
    fn default() -> Self {
        Self {
            depends: Vec::new(),
            config_file: None,
            script: None,
            configure: None,
            prefix: default_prefix(),
            jobs: default_jobs(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RecipeHooks {
    #[serde(default)]
    pub pre_install: Option<String>,
    #[serde(default)]
    pub post_install: Option<String>,
    #[serde(default)]
    pub pre_remove: Option<String>,
    #[serde(default)]
    pub post_remove: Option<String>,
}

/// Load a recipe from disk.
pub fn load_recipe(path: &Path) -> Result<BuildRecipe> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading recipe {}", path.display()))?;
    Ok(toml::from_str(&text)?)
}

/// Result of a successful build.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildResult {
    pub package: Candidate,
    pub archive: PathBuf,
    pub checksum: String,
}

/// Build a source recipe into a `.eva` package.
pub fn build_package(
    paths: &Paths,
    recipe_path: &Path,
    recipe: &BuildRecipe,
    candidates: Vec<Candidate>,
    installed: Vec<crate::model::InstalledPackage>,
    refresh: bool,
) -> Result<BuildResult> {
    let mut recipe = recipe.clone();
    let name = normalize(&recipe.package.name);
    if name.is_empty() {
        bail!("recipe has empty package name");
    }

    if let Some(resolved) = crate::resolver::resolve(&recipe, paths, refresh)? {
        recipe.package.version = resolved.version;
        recipe.source.set_url(resolved.url);
        if !resolved.sha256.is_empty() {
            recipe.source.set_sha256(resolved.sha256);
        }
    }

    let version_string = recipe.package.version.clone();
    let version: evergreen_common::Version = version_string.parse()?;
    let arch = Arch::parse(&recipe.package.arch)?;

    let work = paths
        .cache_dir()
        .join("build")
        .join(format!("{name}-{version}"));
    let _ = std::fs::remove_dir_all(&work);
    let source_dir = work.join("src");
    let destdir = work.join("destdir");
    std::fs::create_dir_all(&source_dir)?;
    std::fs::create_dir_all(&destdir)?;

    check_build_dependencies(&recipe.build.depends, candidates, installed)?;
    fetch_source(&recipe, recipe_path, &work, &source_dir)?;
    if recipe
        .source
        .sha256()
        .filter(|value| !value.is_empty())
        .is_none()
    {
        if let Some(archive) = source_archive(&work) {
            let sha = sha256_file(&archive)?;
            recipe.source.set_sha256(sha.clone());
            crate::resolver::record_checksum(paths, &name, &sha)?;
        }
    }
    if let Some(config) = &recipe.build.config_file {
        let config = Path::new(config);
        let config = if config.is_absolute() {
            config.to_path_buf()
        } else {
            recipe_path.parent().unwrap_or(Path::new(".")).join(config)
        };
        std::fs::copy(&config, source_dir.join(".config"))
            .with_context(|| format!("copying build config {}", config.display()))?;
    }
    apply_patches(&recipe, recipe_path, &source_dir)?;
    run_build(&recipe, &source_dir, &destdir, &version_string)?;

    let mut deps = Vec::new();
    for (dep_name, value) in &recipe.dependencies {
        deps.push(
            value
                .clone()
                .into_dependency(dep_name, crate::model::DepKind::Required)?,
        );
    }
    let recommends = recipe
        .recommends
        .iter()
        .map(|(dep_name, value)| {
            value
                .clone()
                .into_dependency(dep_name, crate::model::DepKind::Recommend)
        })
        .collect::<Result<Vec<_>>>()?;
    let provides = recipe
        .provides
        .iter()
        .map(|(provide_name, version)| {
            Ok(Provide {
                name: normalize(provide_name),
                version: if version.is_empty() || version == "*" {
                    None
                } else {
                    Some(version.parse()?)
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let conflicts = recipe
        .conflicts
        .iter()
        .map(|(conflict_name, constraint)| {
            Ok(Conflict {
                name: normalize(conflict_name),
                constraint: constraint.parse()?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let hook_paths = install_hooks(&recipe, recipe_path, &destdir, &name)?;
    let mut candidate = Candidate {
        id: PackageId {
            name: name.clone(),
            version,
            arch,
            repo: RepoId(u32::MAX),
        },
        meta: PackageMeta {
            description: recipe.package.description.clone(),
            url: recipe.package.url.clone(),
            license: recipe.package.license.clone().into_vec(),
            maintainer: recipe.package.maintainer.clone(),
            ..PackageMeta::default()
        },
        deps,
        opt_deps: Vec::new(),
        recommends,
        suggests: Vec::new(),
        conflicts,
        provides,
        replaces: recipe.replaces.iter().map(|item| normalize(item)).collect(),
        files: Vec::new(),
        symlinks: Vec::new(),
        hooks: hook_paths,
        archive: String::new(),
    };
    candidate = with_payload_checksums(candidate, &destdir)?;
    candidate.meta.size_installed = candidate
        .files
        .iter()
        .map(|file| {
            std::fs::metadata(destdir.join(&file.path))
                .map(|meta| meta.len())
                .unwrap_or(0)
        })
        .sum();

    let archive = paths
        .cache_dir()
        .join("packages")
        .join(candidate.archive_name());
    // The checksum is recorded externally (index/database); leaving it empty
    // inside META.toml avoids a self-referential rewrite loop.
    write_package(&archive, &candidate, &destdir)?;
    let checksum = sha256_file(&archive)?;
    candidate.meta.checksum = checksum.clone();
    candidate.meta.size_download = std::fs::metadata(&archive)?.len();

    Ok(BuildResult {
        package: candidate,
        archive,
        checksum,
    })
}

fn check_build_dependencies(
    depends: &[String],
    candidates: Vec<Candidate>,
    installed: Vec<crate::model::InstalledPackage>,
) -> Result<()> {
    if depends.is_empty() {
        return Ok(());
    }
    let mut resolver = Resolver::new();
    resolver.add_candidates(candidates);
    resolver.set_installed(installed);
    resolver.config.install_recommends = false;
    let install: Vec<PackageSpec> = depends
        .iter()
        .map(|spec| PackageSpec::parse(spec))
        .collect::<Result<Vec<_>, _>>()?;
    resolver
        .resolve(&Request {
            install,
            upgrade: UpgradeMode::None,
            ..Request::default()
        })
        .map_err(|err| anyhow!("build dependencies are not satisfiable: {err}"))?;
    Ok(())
}

fn fetch_source(
    recipe: &BuildRecipe,
    recipe_path: &Path,
    work: &Path,
    source_dir: &Path,
) -> Result<()> {
    let (location, expected) = match &recipe.source {
        SourceField::Empty => return Ok(()),
        SourceField::Url(url) => (url.clone(), None),
        SourceField::Detailed(spec) => {
            let location = spec.url.clone().or_else(|| spec.path.clone());
            let Some(location) = location else {
                return Ok(());
            };
            (location, spec.sha256.clone())
        }
    };

    let mut local = PathBuf::from(&location);
    if !local.is_absolute() {
        let relative = recipe_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(&location);
        if relative.exists() {
            local = relative;
        }
    }

    if local.is_dir() {
        copy_tree(&local, source_dir)?;
        return Ok(());
    }

    let bytes = if local.exists() {
        std::fs::read(&local)?
    } else {
        fetch(&location)?
    };

    if let Some(expected) = expected.filter(|value| !value.is_empty()) {
        let actual = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes));
        if actual != expected {
            bail!("source checksum mismatch: expected {expected}, got {actual}");
        }
    }

    // Detect compressed tarballs by magic bytes: crates.io URLs end in
    // `/download` but still return a gzip-compressed tar archive.
    let archive_name = if bytes.starts_with(&[0x1f, 0x8b]) {
        "source.tar.gz"
    } else if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        "source.tar.zst"
    } else if bytes.starts_with(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]) {
        "source.tar.xz"
    } else {
        ""
    };

    if archive_name.is_empty() {
        let filename = local.file_name().unwrap_or_default();
        std::fs::write(source_dir.join(filename), bytes)?;
    } else {
        let archive_path = work.join(archive_name);
        std::fs::write(&archive_path, &bytes)?;
        extract_source_archive(&archive_path, source_dir)?;
    }
    Ok(())
}

fn extract_source_archive(path: &Path, destination: &Path) -> Result<()> {
    let name = path.to_string_lossy();
    std::fs::create_dir_all(destination)?;

    if name.ends_with(".tar.zst") {
        let file = std::fs::File::open(path)?;
        let decoder = zstd::stream::read::Decoder::new(file)?;
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(destination)?;
    } else if name.ends_with(".tar.xz") {
        let status = Command::new("tar")
            .arg("-xJf")
            .arg(path)
            .current_dir(destination)
            .status()?;
        if !status.success() {
            bail!("failed to extract xz source archive");
        }
    } else {
        let file = std::fs::File::open(path)?;
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(destination)?;
    }
    Ok(())
}

fn apply_patches(recipe: &BuildRecipe, recipe_path: &Path, source_dir: &Path) -> Result<()> {
    for patch in recipe.build_patches(recipe_path) {
        let output = Command::new("patch")
            .arg("-p1")
            .arg("-i")
            .arg(&patch)
            .current_dir(source_dir)
            .output()
            .with_context(|| format!("applying patch {}", patch.display()))?;
        if !output.status.success() {
            bail!(
                "patch {} failed: {}",
                patch.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    Ok(())
}

fn source_archive(work: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(work).ok()?;
    entries.flatten().map(|entry| entry.path()).find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("source.") && path.is_file())
    })
}

fn run_build(recipe: &BuildRecipe, source_dir: &Path, destdir: &Path, version: &str) -> Result<()> {
    let destination = destdir.to_string_lossy();
    let prefix = &recipe.build.prefix;
    let jobs = recipe.build.jobs.to_string();

    if let Some(script) = &recipe.build.script {
        let status = Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .current_dir(source_dir)
            .env("DESTDIR", destination.as_ref())
            .env("PREFIX", prefix)
            .env("JOBS", &jobs)
            .env("SRC", source_dir)
            .env("VERSION", version)
            .status()?;
        if !status.success() {
            bail!("build script failed");
        }
        return Ok(());
    }

    if let Some(configure) = &recipe.build.configure {
        if configure != "none" {
            let status = Command::new("/bin/sh")
                .arg("-c")
                .arg(configure)
                .current_dir(source_dir)
                .env("PREFIX", prefix)
                .env("SRC", source_dir)
                .env("VERSION", version)
                .status()?;
            if !status.success() {
                bail!("configure step failed");
            }
        }
    }

    for command in [
        format!("make -j{jobs}"),
        format!("make DESTDIR='{destination}' PREFIX='{prefix}' install"),
    ] {
        let status = Command::new("/bin/sh")
            .arg("-c")
            .arg(&command)
            .current_dir(source_dir)
            .env("SRC", source_dir)
            .env("DESTDIR", destination.as_ref())
            .env("PREFIX", prefix)
            .env("JOBS", &jobs)
            .env("VERSION", version)
            .status()?;
        if !status.success() {
            bail!("build step failed: {command}");
        }
    }
    Ok(())
}

fn install_hooks(
    recipe: &BuildRecipe,
    recipe_path: &Path,
    destdir: &Path,
    name: &str,
) -> Result<crate::model::Hooks> {
    let pairs = [
        ("pre_install", recipe.hooks.pre_install.as_ref()),
        ("post_install", recipe.hooks.post_install.as_ref()),
        ("pre_remove", recipe.hooks.pre_remove.as_ref()),
        ("post_remove", recipe.hooks.post_remove.as_ref()),
    ];
    let mut hooks = crate::model::Hooks::default();
    for (kind, source) in pairs {
        let Some(source) = source else { continue };
        let mut source = PathBuf::from(source);
        if !source.is_absolute() {
            let relative = recipe_path.parent().unwrap_or(Path::new(".")).join(&source);
            if relative.exists() {
                source = relative;
            }
        }
        let relative = format!("usr/lib/eva/hooks/{name}-{kind}.sh");
        let target = destdir.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&source, &target)
            .with_context(|| format!("copying hook {}", source.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
        }
        match kind {
            "pre_install" => hooks.pre_install = Some(relative.clone()),
            "post_install" => hooks.post_install = Some(relative.clone()),
            "pre_remove" => hooks.pre_remove = Some(relative.clone()),
            "post_remove" => hooks.post_remove = Some(relative.clone()),
            _ => {}
        }
    }
    Ok(hooks)
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Never copy build outputs or VCS metadata into package source trees.
        if matches!(name.as_ref(), "target" | ".git" | "artifacts") {
            continue;
        }
        let target = destination.join(name.as_ref());
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

impl BuildRecipe {
    fn build_patches(&self, recipe_path: &Path) -> Vec<PathBuf> {
        // `[build] patches = [...]` is accepted as an extension of the common
        // recipe format; use a permissive manual lookup so older recipes work.
        let text = std::fs::read_to_string(recipe_path).unwrap_or_default();
        let mut patches = Vec::new();
        let mut in_build = false;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                in_build = trimmed.eq_ignore_ascii_case("[build]");
                continue;
            }
            if in_build && trimmed.starts_with("patches") {
                if let Some((_, value)) = trimmed.split_once('=') {
                    let value = value.trim().trim_matches(['[', ']']);
                    for patch in value.split(',') {
                        let patch = patch.trim().trim_matches('"').trim_matches('\'');
                        if !patch.is_empty() {
                            let path = Path::new(patch);
                            let path = if path.is_absolute() || path.exists() {
                                path.to_path_buf()
                            } else {
                                recipe_path.parent().unwrap_or(Path::new(".")).join(path)
                            };
                            patches.push(path);
                        }
                    }
                }
            }
        }
        patches
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_source_recipe() {
        let text = r#"
[package]
name = "hello"
version = "1.0"
description = "Hello"

[source]
path = "src"

[build]
script = "make install DESTDIR=$DESTDIR"

[dependencies]
libc = ">=2.30"
"#;
        let recipe: BuildRecipe = toml::from_str(text).unwrap();
        assert_eq!(recipe.package.name, "hello");
        assert_eq!(
            recipe.build.script.as_deref(),
            Some("make install DESTDIR=$DESTDIR")
        );
        assert_eq!(recipe.dependencies.len(), 1);
    }

    #[test]
    fn copies_config_file_into_source_tree() {
        let dir = std::env::temp_dir().join(format!(
            "eva-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("defconfig"), "CONFIG_TEST=y\n").unwrap();
        let recipe_path = dir.join("recipe.toml");
        std::fs::write(
            &recipe_path,
            r#"
[package]
name = "configured"
version = "1.0"

[source]
path = "src"

[build]
config_file = "defconfig"
script = "mkdir -p \"$DESTDIR/etc\" && cp .config \"$DESTDIR/etc/kernel-config\""
"#,
        )
        .unwrap();

        let paths = Paths::for_root(&dir);
        let recipe = load_recipe(&recipe_path).unwrap();
        build_package(&paths, &recipe_path, &recipe, Vec::new(), Vec::new(), false).unwrap();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn builds_package_from_directory_source() {
        let dir = std::env::temp_dir().join(format!(
            "eva-build-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/message.txt"), b"hello\n").unwrap();
        let recipe_path = dir.join("recipe.toml");
        std::fs::write(
            &recipe_path,
            r#"
[package]
name = "hello"
version = "1.0"

[source]
path = "src"

[build]
script = "mkdir -p $DESTDIR/usr/share && cp message.txt $DESTDIR/usr/share/"
"#,
        )
        .unwrap();

        let paths = Paths::for_root(&dir);
        let recipe = load_recipe(&recipe_path).unwrap();
        let result =
            build_package(&paths, &recipe_path, &recipe, Vec::new(), Vec::new(), false).unwrap();
        assert_eq!(result.package.id.name, "hello");
        assert!(!result.checksum.is_empty());
        assert!(result.archive.exists());
        assert!(result
            .package
            .files
            .iter()
            .any(|file| file.path == "usr/share/message.txt"));
        std::fs::remove_dir_all(dir).ok();
    }
}
