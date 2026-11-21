//! Stage 1 build execution: build Evergreen Rust components as `.eva` packages.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use eva::build::{build_package, BuildRecipe};
use eva::config::Paths as EvaPaths;
use serde::{Deserialize, Serialize};

use crate::config::{Arch, BootstrapConfig};
use crate::stages::StageStatus;

#[derive(Debug, Clone, Default, Deserialize)]
struct Stage1Spec {
    #[serde(default)]
    stage: u8,
    #[serde(default)]
    build_deps: Vec<String>,
    #[serde(default)]
    arch: String,
}

#[derive(Debug, Deserialize)]
struct RecipeFile {
    #[serde(flatten)]
    recipe: BuildRecipe,
    #[serde(default)]
    stage1: Stage1Spec,
}

/// One stage 1 recipe.
#[derive(Debug, Clone)]
pub struct Stage1Recipe {
    pub recipe: BuildRecipe,
    pub build_deps: Vec<String>,
    pub directory: PathBuf,
}

impl Stage1Recipe {
    pub fn name(&self) -> &str {
        &self.recipe.package.name
    }

    pub fn version(&self) -> &str {
        &self.recipe.package.version
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PackageState {
    version: String,
    status: StageStatus,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct BuildState {
    packages: BTreeMap<String, PackageState>,
}

#[derive(Debug, Clone)]
pub struct Stage1Paths {
    pub root: PathBuf,
    pub build_root: PathBuf,
    pub packages: PathBuf,
    pub state: PathBuf,
}

impl Stage1Paths {
    pub fn new(config: &BootstrapConfig) -> Self {
        let root = config.workdir.clone();
        Self {
            build_root: root.join("stage1"),
            packages: root.join("packages").join(config.arch.as_str()),
            state: root.join("state"),
            root,
        }
    }

    fn state_file(&self, arch: Arch) -> PathBuf {
        self.state.join(format!("stage1-{}.json", arch.as_str()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage1Report {
    pub built: Vec<String>,
    pub skipped: Vec<String>,
    pub packages: Vec<PathBuf>,
}

/// Load stage 1 recipes for the configured architecture.
pub fn load_recipes(directory: &Path, arch: Arch) -> Result<Vec<Stage1Recipe>> {
    if !directory.exists() {
        bail!(
            "stage 1 recipe directory {} does not exist",
            directory.display()
        );
    }

    let mut files = Vec::new();
    collect_recipe_files(directory, &mut files)?;
    files.sort();

    let mut recipes: BTreeMap<String, Stage1Recipe> = BTreeMap::new();
    for path in files {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let file: RecipeFile =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if file.stage1.stage != 1 {
            continue;
        }
        let recipe_arch = file.stage1.arch.trim().to_ascii_lowercase();
        if !recipe_arch.is_empty()
            && recipe_arch != "any"
            && recipe_arch != "both"
            && recipe_arch != arch.as_str()
        {
            continue;
        }
        let directory = path.parent().unwrap_or(directory).to_path_buf();
        let recipe = Stage1Recipe {
            recipe: file.recipe,
            build_deps: file.stage1.build_deps,
            directory,
        };
        if let Some(previous) = recipes.get(recipe.name()) {
            bail!(
                "duplicate stage 1 recipe {} ({} and {})",
                recipe.name(),
                previous.directory.display(),
                recipe.directory.display()
            );
        }
        recipes.insert(recipe.name().to_string(), recipe);
    }
    Ok(recipes.into_values().collect())
}

fn collect_recipe_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(directory)
        .with_context(|| format!("reading {}", directory.display()))?
        .flatten()
    {
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_recipe_files(&path, files)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
            files.push(path);
        }
    }
    Ok(())
}

/// Topologically order stage 1 recipes by their build dependencies.
pub fn build_order(recipes: &[Stage1Recipe]) -> Result<Vec<Stage1Recipe>> {
    let by_name: BTreeMap<&str, &Stage1Recipe> = recipes
        .iter()
        .map(|recipe| (recipe.name(), recipe))
        .collect();
    let mut indegree: BTreeMap<String, usize> = recipes
        .iter()
        .map(|recipe| (recipe.name().to_string(), 0))
        .collect();
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for recipe in recipes {
        for dependency in &recipe.build_deps {
            if !by_name.contains_key(dependency.as_str()) {
                continue; // stage 0 toolchain dependencies are not recipes here
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

    let mut ready: BTreeSet<String> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(name, _)| name.clone())
        .collect();
    let mut ordered = Vec::new();
    while let Some(name) = ready.iter().next().cloned() {
        ready.remove(&name);
        ordered.push(by_name[name.as_str()].clone());
        if let Some(children) = edges.get(&name) {
            for child in children {
                if let Some(degree) = indegree.get_mut(child) {
                    *degree -= 1;
                    if *degree == 0 {
                        ready.insert(child.clone());
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
        bail!("stage 1 dependency cycle: {}", remaining.join(" -> "));
    }
    Ok(ordered)
}

/// Build all stage 1 recipes into `.eva` packages in the workdir.
pub fn build_stage1(
    config: &BootstrapConfig,
    recipes_dir: &Path,
    resume: bool,
) -> Result<Stage1Report> {
    let paths = Stage1Paths::new(config);
    for directory in [&paths.build_root, &paths.packages, &paths.state] {
        std::fs::create_dir_all(directory)?;
    }

    let recipes = load_recipes(recipes_dir, config.arch)?;
    let ordered = build_order(&recipes)?;
    let state_file = paths.state_file(config.arch);
    let mut state: BuildState = if resume && state_file.exists() {
        serde_json::from_str(&std::fs::read_to_string(&state_file)?)?
    } else {
        BuildState::default()
    };

    let mut built = Vec::new();
    let mut skipped = Vec::new();
    let mut packages = Vec::new();

    for recipe in &ordered {
        let name = recipe.name();
        if let Some(previous) = state.packages.get(name) {
            if previous.status == StageStatus::Done
                && previous.version == recipe.recipe.package.version
            {
                skipped.push(name.to_string());
                packages.push(final_archive_path(
                    &paths,
                    name,
                    &recipe.recipe.package.version,
                ));
                continue;
            }
        }

        state.packages.insert(
            name.to_string(),
            PackageState {
                version: recipe.recipe.package.version.clone(),
                status: StageStatus::Building,
                error: None,
            },
        );
        save_state(&state_file, &state)?;

        let result = build_one(recipe, &paths).with_context(|| format!("building {name}"));
        match result {
            Ok(archive) => {
                if let Some(entry) = state.packages.get_mut(name) {
                    entry.status = StageStatus::Done;
                }
                built.push(name.to_string());
                packages.push(archive);
            }
            Err(err) => {
                let message = format!("{err:#}");
                if let Some(entry) = state.packages.get_mut(name) {
                    entry.status = StageStatus::Failed;
                    entry.error = Some(message.clone());
                }
                save_state(&state_file, &state)?;
                return Err(anyhow::anyhow!("{message}"));
            }
        }
        save_state(&state_file, &state)?;
    }

    Ok(Stage1Report {
        built,
        skipped,
        packages,
    })
}

fn save_state(path: &Path, state: &BuildState) -> Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

/// Load recorded stage 1 status for `eva status`.
pub fn load_state(config: &BootstrapConfig) -> Result<BTreeMap<String, StageStatus>> {
    let paths = Stage1Paths::new(config);
    let file = paths.state_file(config.arch);
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

/// Remove stage 1 build state and outputs.
pub fn clean_stage1(config: &BootstrapConfig) -> Result<()> {
    let paths = Stage1Paths::new(config);
    for directory in [&paths.build_root, &paths.packages] {
        if directory.exists() {
            std::fs::remove_dir_all(directory)?;
        }
    }
    let _ = std::fs::remove_file(paths.state_file(config.arch));
    Ok(())
}

fn final_archive_path(paths: &Stage1Paths, name: &str, version: &str) -> PathBuf {
    paths.packages.join(format!("{name}-{version}.eva"))
}

fn build_one(recipe: &Stage1Recipe, paths: &Stage1Paths) -> Result<PathBuf> {
    let recipe_path = recipe
        .directory
        .join(format!("{}.toml", recipe.recipe.package.name));
    let eva_paths = EvaPaths::for_root(&paths.build_root);
    let result = build_package(
        &eva_paths,
        &recipe_path,
        &recipe.recipe,
        Vec::new(),
        Vec::new(),
        false,
    )?;

    let destination = final_archive_path(
        paths,
        &result.package.id.name,
        &result.package.id.version.to_string(),
    );
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&result.archive, &destination)?;
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_shipped_stage1_recipes() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../packages");
        let recipes = load_recipes(&dir, Arch::X86_64).expect("stage1 recipes");
        let names: Vec<&str> = recipes.iter().map(Stage1Recipe::name).collect();
        assert!(names.contains(&"pine"));
        assert!(names.contains(&"eva"));
        let order = build_order(&recipes).expect("order");
        assert_eq!(order.len(), recipes.len());
    }
}
