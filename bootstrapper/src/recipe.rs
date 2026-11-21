//! Stage 0 recipe loading and build ordering.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::config::{Arch, Libc};

/// Source declaration for a stage 0 recipe.
#[derive(Debug, Clone, Deserialize)]
pub struct SourceSpec {
    /// HTTP(S) or `file://` URL, or a relative path under the recipe directory.
    pub url: Option<String>,
    /// Alternative to `url`: a path relative to the recipe file.
    pub path: Option<String>,
    /// Required SHA-256 of the downloaded source archive.
    pub sha256: String,
    /// Number of leading path components to strip when extracting.
    #[serde(default = "default_strip")]
    pub strip_components: usize,
}

fn default_strip() -> usize {
    1
}

/// Build instructions for a stage 0 recipe.
#[derive(Debug, Clone, Deserialize)]
pub struct BootstrapSpec {
    /// Must be `0` for stage 0 recipes.
    pub stage: u8,
    /// Pass number for packages with the same name (e.g. gcc pass 1/2).
    #[serde(default)]
    pub pass: u32,
    /// Packages that must be built before this one.
    #[serde(default)]
    pub build_deps: Vec<String>,
    /// Configure command, run in the extracted source directory.
    #[serde(default)]
    pub configure: Option<String>,
    /// Build command. Defaults to `make -j{jobs}`.
    #[serde(default)]
    pub build: Option<String>,
    /// Install command. Defaults to `make DESTDIR={sysroot} install`.
    #[serde(default)]
    pub install: Option<String>,
    /// Whether the result is installed into the stage 0 sysroot.
    #[serde(default = "default_true")]
    pub sysroot: bool,
    /// Editions for which this recipe applies: `glibc`, `musl`, or `both`.
    #[serde(default = "default_editions")]
    pub editions: Vec<String>,
}

fn default_true() -> bool {
    true
}

fn default_editions() -> Vec<String> {
    vec!["both".to_string()]
}

#[derive(Debug, Clone, Deserialize)]
pub struct PackageSpec {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub release: u32,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub license: String,
}

/// One stage 0 build recipe.
#[derive(Debug, Clone, Deserialize)]
pub struct Stage0Recipe {
    pub package: PackageSpec,
    pub source: SourceSpec,
    pub bootstrap: BootstrapSpec,
    /// Directory containing the recipe file (used to resolve `source.path`).
    #[serde(skip)]
    pub directory: PathBuf,
}

impl Stage0Recipe {
    pub fn name(&self) -> &str {
        &self.package.name
    }

    pub fn version(&self) -> &str {
        &self.package.version
    }

    pub fn pass(&self) -> u32 {
        self.bootstrap.pass
    }

    pub fn applies_to_arch(&self, arch: Arch) -> bool {
        match self.package.arch.trim().to_ascii_lowercase().as_str() {
            "" | "any" | "all" | "both" => true,
            "x86_64" => arch == Arch::X86_64,
            "aarch64" => arch == Arch::Aarch64,
            _ => false,
        }
    }

    pub fn applies_to(&self, libc: Libc) -> bool {
        self.bootstrap
            .editions
            .iter()
            .any(|edition| match edition.to_ascii_lowercase().as_str() {
                "both" | "any" => true,
                "glibc" => libc == Libc::Glibc,
                "musl" => libc == Libc::Musl,
                _ => false,
            })
    }

    /// Source location: `path` is resolved against the recipe directory.
    pub fn source_location(&self) -> Result<String> {
        if let Some(url) = &self.source.url {
            return Ok(url.clone());
        }
        let path = self
            .source
            .path
            .as_ref()
            .ok_or_else(|| anyhow!("{}: source.url or source.path is required", self.name()))?;
        let location = Path::new(path);
        if location.is_absolute() {
            return Ok(location.display().to_string());
        }
        Ok(self.directory.join(location).display().to_string())
    }

    pub fn configure_command(&self) -> String {
        self.bootstrap
            .configure
            .clone()
            .unwrap_or_else(|| "./configure --prefix=/usr".to_string())
    }

    pub fn build_command(&self) -> String {
        self.bootstrap
            .build
            .clone()
            .unwrap_or_else(|| "make -j{jobs}".to_string())
    }

    pub fn install_command(&self) -> String {
        self.bootstrap
            .install
            .clone()
            .unwrap_or_else(|| "make DESTDIR={sysroot} install".to_string())
    }
}

/// Load all stage 0 recipes from `directory` that apply to `libc`.
pub fn load_recipes(directory: &Path, libc: Libc, arch: Arch) -> Result<Vec<Stage0Recipe>> {
    let mut recipes = Vec::new();
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "stage 0 recipe directory {} does not exist",
                directory.display()
            )
        }
        Err(err) => return Err(err.into()),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut recipe: Stage0Recipe =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        recipe.directory = directory.to_path_buf();

        if recipe.bootstrap.stage != 0 || !recipe.applies_to(libc) || !recipe.applies_to_arch(arch)
        {
            continue;
        }
        if recipe.source.sha256.trim().len() != 64 {
            bail!(
                "{}: source.sha256 must be a 64-character hex digest",
                recipe.name()
            );
        }
        recipes.push(recipe);
    }

    recipes.sort_by(|a, b| {
        a.bootstrap
            .pass
            .cmp(&b.bootstrap.pass)
            .then(a.name().cmp(b.name()))
    });
    Ok(recipes)
}

/// Topologically order recipes by their declared build dependencies.
pub fn build_order(recipes: &[Stage0Recipe]) -> Result<Vec<Stage0Recipe>> {
    let by_name: BTreeMap<&str, &Stage0Recipe> = recipes
        .iter()
        .map(|recipe| (recipe.name(), recipe))
        .collect();
    let mut indegree: BTreeMap<String, usize> = recipes
        .iter()
        .map(|recipe| (recipe.name().to_string(), 0))
        .collect();
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for recipe in recipes {
        for dependency in &recipe.bootstrap.build_deps {
            if !by_name.contains_key(dependency.as_str()) {
                bail!(
                    "{}: build dependency {dependency} has no applicable recipe",
                    recipe.name()
                );
            }
            if edges
                .entry(dependency.clone())
                .or_default()
                .insert(recipe.name().to_string())
            {
                *indegree.get_mut(recipe.name()).expect("recipe exists") += 1;
            }
        }
    }

    let mut ready: BTreeSet<(u32, String)> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(name, _)| (by_name[name.as_str()].bootstrap.pass, name.clone()))
        .collect();
    let mut ordered = Vec::new();

    while let Some((_, name)) = ready.iter().next().cloned() {
        ready.remove(&(by_name[name.as_str()].bootstrap.pass, name.clone()));
        let recipe = by_name[name.as_str()].clone();
        ordered.push(recipe);
        if let Some(children) = edges.get(&name) {
            for child in children {
                if let Some(degree) = indegree.get_mut(child) {
                    *degree -= 1;
                    if *degree == 0 {
                        ready.insert((by_name[child.as_str()].bootstrap.pass, child.clone()));
                    }
                }
            }
        }
    }

    if ordered.len() != recipes.len() {
        let remaining: Vec<String> = indegree
            .iter()
            .filter(|(_, degree)| **degree > 0)
            .map(|(name, _)| name.clone())
            .collect();
        bail!("stage 0 dependency cycle: {}", remaining.join(" -> "));
    }
    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe(name: &str, pass: u32, deps: &[&str], editions: &[&str]) -> Stage0Recipe {
        Stage0Recipe {
            package: PackageSpec {
                name: name.to_string(),
                version: "1.0".to_string(),
                release: 0,
                arch: "x86_64".to_string(),
                description: String::new(),
                license: String::new(),
            },
            source: SourceSpec {
                url: None,
                path: Some("src.tar.gz".to_string()),
                sha256: "0".repeat(64),
                strip_components: 1,
            },
            bootstrap: BootstrapSpec {
                stage: 0,
                pass,
                build_deps: deps.iter().map(|dep| dep.to_string()).collect(),
                configure: None,
                build: None,
                install: None,
                sysroot: true,
                editions: editions.iter().map(|edition| edition.to_string()).collect(),
            },
            directory: PathBuf::from("/tmp"),
        }
    }

    #[test]
    fn orders_by_dependencies_and_pass() {
        let recipes = vec![
            recipe("gcc-pass2", 2, &["libc"], &[]),
            recipe("libc", 1, &["gcc-pass1"], &[]),
            recipe("gcc-pass1", 1, &[], &[]),
        ];
        let order = build_order(&recipes).unwrap();
        let names: Vec<&str> = order.iter().map(Stage0Recipe::name).collect();
        assert_eq!(names, vec!["gcc-pass1", "libc", "gcc-pass2"]);
    }

    #[test]
    fn detects_cycles() {
        let recipes = vec![recipe("a", 0, &["b"], &[]), recipe("b", 0, &["a"], &[])];
        assert!(build_order(&recipes).is_err());
    }

    #[test]
    fn filters_editions() {
        let glibc_only = recipe("glibc", 1, &[], &["glibc"]);
        let musl_only = recipe("musl", 1, &[], &["musl"]);
        let both = recipe("make", 2, &[], &["both"]);
        assert!(glibc_only.applies_to(Libc::Glibc));
        assert!(!glibc_only.applies_to(Libc::Musl));
        assert!(musl_only.applies_to(Libc::Musl));
        assert!(both.applies_to(Libc::Glibc));
        assert!(both.applies_to(Libc::Musl));
    }
}
