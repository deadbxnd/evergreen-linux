//! Repository publishing and recipe validation.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use eva::publish::{build_index, write_signed_index};

#[derive(Debug, Clone)]
pub struct PublishOptions {
    pub packages_dir: PathBuf,
    pub output_dir: PathBuf,
    pub repo: String,
    pub branch: String,
    pub signing_key: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct PublishReport {
    pub index: PathBuf,
    pub signature: Option<PathBuf>,
    pub packages: Vec<PathBuf>,
}

/// Copy packages into a repository directory and generate a signed index.
pub fn publish(options: &PublishOptions) -> Result<PublishReport> {
    if !options.packages_dir.is_dir() {
        bail!(
            "package directory {} does not exist",
            options.packages_dir.display()
        );
    }
    std::fs::create_dir_all(&options.output_dir)?;

    let mut packages = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&options.packages_dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("eva"))
        .collect();
    entries.sort();
    for source in entries {
        let name = source
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let destination = options.output_dir.join(name);
        if source != destination {
            std::fs::copy(&source, &destination)
                .with_context(|| format!("copying {}", source.display()))?;
        }
        packages.push(destination);
    }

    let index = build_index(&options.output_dir, &options.repo, &options.branch)?;
    let (index_path, signature) =
        write_signed_index(&options.output_dir, &index, options.signing_key.as_deref())?;

    Ok(PublishReport {
        index: index_path,
        signature,
        packages,
    })
}

#[derive(Debug, Clone, Default)]
pub struct RecipeReport {
    pub recipes: Vec<PathBuf>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Validate every Stage 1 recipe under `root` without building anything.
pub fn validate_recipes(root: &Path) -> Result<RecipeReport> {
    let mut files = Vec::new();
    collect_toml(root, &mut files)?;
    files.sort();

    let mut report = RecipeReport::default();
    let mut names = BTreeSet::new();

    for file in &files {
        let text = std::fs::read_to_string(file)?;
        let value: toml::Value =
            toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        let stage1 = value.get("stage1");
        if stage1.is_none() {
            continue;
        }
        let stage = stage1
            .and_then(|section| section.get("stage"))
            .and_then(toml::Value::as_integer)
            .unwrap_or(0);
        if stage != 1 {
            continue;
        }

        report.recipes.push(file.clone());
        let name = value
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_string();
        if name.is_empty() || !names.insert(name.clone()) {
            report
                .errors
                .push(format!("{}: missing or duplicate name", file.display()));
        }

        if let Some(source) = value.get("source") {
            if let Some(url) = source.get("url").and_then(toml::Value::as_str) {
                if !url.contains("{version}") && !url.contains("{tag}") {
                    report.warnings.push(format!(
                        "{}: source URL is not dynamically versioned",
                        file.display()
                    ));
                }
            } else if let Some(path) = source.get("path").and_then(toml::Value::as_str) {
                let directory = if Path::new(path).is_absolute() {
                    PathBuf::from(path)
                } else {
                    file.parent().unwrap_or(root).join(path)
                };
                if !directory.exists() {
                    report.errors.push(format!(
                        "{}: local source path {} does not exist",
                        file.display(),
                        directory.display()
                    ));
                }
            } else {
                report
                    .warnings
                    .push(format!("{}: no source declared", file.display()));
            }
        }
    }

    for recipe in &report.recipes {
        let text = std::fs::read_to_string(recipe)?;
        let value: toml::Value = toml::from_str(&text)?;
        if let Some(deps) = value
            .get("stage1")
            .and_then(|section| section.get("build_deps"))
            .and_then(toml::Value::as_array)
        {
            for dep in deps.iter().filter_map(toml::Value::as_str) {
                if !names.contains(dep) {
                    report.warnings.push(format!(
                        "{}: build dependency {dep} is not a Stage 1 recipe (stage 0 toolchain?)",
                        recipe.display()
                    ));
                }
            }
        }
    }

    Ok(report)
}

fn collect_toml(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(directory)
        .with_context(|| format!("reading {}", directory.display()))?
        .flatten()
    {
        let path = entry.path();
        if path.is_dir() {
            collect_toml(&path, files)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_recipe_tree() {
        let dir = std::env::temp_dir().join(format!(
            "build-validate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("demo.toml"),
            r#"
[package]
name = "demo"
version = "1.0"
[source]
url = "https://example.org/demo-{version}.tar.gz"
[stage1]
stage = 1
build_deps = ["missing"]
"#,
        )
        .unwrap();

        let report = validate_recipes(&dir).unwrap();
        assert_eq!(report.recipes.len(), 1);
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("missing")));
        std::fs::remove_dir_all(dir).ok();
    }
}
