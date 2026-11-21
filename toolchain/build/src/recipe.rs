//! Recipe loading (shared TOML format for eva packages and bootstrap stages).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use evergreen_common::{BuildSpec, Dependency, PackageMeta, Recipe};
use serde::Deserialize;

/// Errors produced while loading a recipe.
#[derive(Debug)]
pub enum RecipeError {
    Io(std::io::Error),
    Parse(String),
}

impl std::fmt::Display for RecipeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecipeError::Io(err) => write!(f, "recipe I/O error: {err}"),
            RecipeError::Parse(err) => write!(f, "recipe parse error: {err}"),
        }
    }
}

impl std::error::Error for RecipeError {}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DepValue {
    Constraint(String),
    Detailed {
        #[serde(default)]
        constraint: String,
        #[serde(default)]
        optional: bool,
    },
}

#[derive(Debug, Deserialize, Default)]
struct RawRecipe {
    #[serde(default)]
    package: RawPackage,
    #[serde(default)]
    dependencies: BTreeMap<String, DepValue>,
    #[serde(default)]
    source: RawSource,
    #[serde(default)]
    build: RawBuild,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawPackage {
    name: String,
    version: String,
    release: u32,
    arch: String,
    description: String,
    license: String,
    maintainer: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawSource {
    url: Option<String>,
    path: Option<String>,
    sha256: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawBuild {
    configure: Option<String>,
    build_deps: Vec<String>,
    patches: Vec<String>,
    stage: Option<u8>,
}

/// Load a recipe from a TOML file.
pub fn load_recipe(path: &Path) -> Result<Recipe, RecipeError> {
    let text = std::fs::read_to_string(path).map_err(RecipeError::Io)?;
    parse_recipe(&text).map_err(RecipeError::Parse)
}

/// Parse a recipe from TOML text.
pub fn parse_recipe(text: &str) -> Result<Recipe, String> {
    let raw: RawRecipe = toml::from_str(text).map_err(|err| err.to_string())?;
    if raw.package.name.trim().is_empty() {
        return Err("recipe [package] requires name".to_string());
    }
    if raw.package.version.trim().is_empty() {
        return Err("recipe [package] requires version".to_string());
    }

    let dependencies = raw
        .dependencies
        .into_iter()
        .map(|(name, value)| {
            let (constraint, optional) = match value {
                DepValue::Constraint(constraint) => (constraint, false),
                DepValue::Detailed {
                    constraint,
                    optional,
                } => (constraint, optional),
            };
            Dependency {
                name,
                constraint,
                optional,
            }
        })
        .collect();

    Ok(Recipe {
        package: PackageMeta {
            name: raw.package.name,
            version: raw.package.version,
            release: raw.package.release,
            arch: raw.package.arch,
            description: raw.package.description,
            license: raw.package.license,
            maintainer: raw.package.maintainer,
        },
        dependencies,
        build: BuildSpec {
            source: raw.source.url.or(raw.source.path),
            sha256: raw.source.sha256,
            configure: raw.build.configure,
            build_deps: raw.build.build_deps,
            patches: raw.build.patches.into_iter().map(PathBuf::from).collect(),
            stage: raw.build.stage,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_recipe() {
        let text = r#"
[package]
name = "hello"
version = "1.0"
release = 2
arch = "x86_64"
description = "Hello"

[dependencies]
libc = ">=2.30"
optional-dep = { constraint = "*", optional = true }

[source]
url = "https://example.org/hello-1.0.tar.gz"
sha256 = "abc"

[build]
configure = "--prefix=/usr"
build_deps = ["make", "gcc"]
patches = ["fix.patch"]
stage = 1
"#;
        let recipe = parse_recipe(text).unwrap();
        assert_eq!(recipe.package.name, "hello");
        assert_eq!(recipe.package.release, 2);
        assert_eq!(recipe.dependencies.len(), 2);
        assert!(recipe.dependencies[1].optional);
        assert_eq!(recipe.build.build_deps, vec!["make", "gcc"]);
        assert_eq!(recipe.build.stage, Some(1));
    }

    #[test]
    fn loads_from_file() {
        let dir = std::env::temp_dir().join(format!(
            "build-recipe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recipe.toml");
        std::fs::write(&path, "[package]\nname = \"a\"\nversion = \"1\"\n").unwrap();
        assert_eq!(load_recipe(&path).unwrap().package.name, "a");
        assert!(load_recipe(&dir.join("missing.toml")).is_err());
        std::fs::remove_dir_all(dir).ok();
    }
}
