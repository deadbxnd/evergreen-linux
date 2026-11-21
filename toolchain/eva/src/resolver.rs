//! Dynamic upstream version resolution for package recipes.
//!
//! Recipes mark a source URL with a `{version}` placeholder and leave
//! `source.sha256` empty. The resolver queries the upstream API or directory
//! listing, picks the newest stable version, expands the URL, and caches the
//! result for 24h. The cache is refreshed with `eva build --refresh`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::build::BuildRecipe;
use crate::config::Paths;

const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedSource {
    pub version: String,
    pub url: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub resolved_at: String,
}

/// Resolve the source for `recipe`, returning `None` for static/local sources.
pub fn resolve(
    recipe: &BuildRecipe,
    paths: &Paths,
    refresh: bool,
) -> Result<Option<ResolvedSource>> {
    let Some(template) = recipe.source.url() else {
        return Ok(None);
    };
    if !template.contains("{version}") && !template.contains("{tag}") {
        return Ok(None);
    }

    let cache = cache_path(paths, &recipe.package.name);
    if !refresh && cache.exists() {
        if let Ok(text) = std::fs::read_to_string(&cache) {
            if let Ok(cached) = serde_json::from_str::<ResolvedSource>(&text) {
                if cache_age(&cached) < CACHE_TTL {
                    return Ok(Some(cached));
                }
            }
        }
    }

    let resolved = resolve_remote(recipe, template)?;
    if let Some(parent) = cache.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&cache, serde_json::to_string_pretty(&resolved)?)?;
    Ok(Some(resolved))
}

/// Store a freshly computed checksum in the cache.
pub fn record_checksum(paths: &Paths, name: &str, sha256: &str) -> Result<()> {
    let cache = cache_path(paths, name);
    if !cache.exists() {
        return Ok(());
    }
    let mut resolved: ResolvedSource = serde_json::from_str(&std::fs::read_to_string(&cache)?)?;
    resolved.sha256 = sha256.to_string();
    std::fs::write(cache, serde_json::to_string_pretty(&resolved)?)?;
    Ok(())
}

fn cache_path(paths: &Paths, name: &str) -> PathBuf {
    paths
        .cache_dir()
        .join("resolved")
        .join(format!("{}.json", name.replace('/', "_")))
}

fn cache_age(resolved: &ResolvedSource) -> Duration {
    let Ok(stored) = chrono::DateTime::parse_from_rfc3339(&resolved.resolved_at) else {
        return Duration::MAX;
    };
    let elapsed = chrono::Utc::now().signed_duration_since(stored);
    Duration::from_secs(elapsed.num_seconds().max(0) as u64)
}

fn resolve_remote(recipe: &BuildRecipe, template: &str) -> Result<ResolvedSource> {
    if template.starts_with("https://github.com/") {
        return resolve_github(template);
    }
    if template.starts_with("https://gitlab.freedesktop.org/") {
        return resolve_gitlab(template);
    }
    if template.contains("crates.io") {
        return resolve_crates(template);
    }
    resolve_directory(recipe, template)
}

fn http_get(url: &str) -> Result<String> {
    let response = ureq::get(url)
        .set("User-Agent", "eva-package-resolver/0.1")
        .set("Accept", "application/json, text/html, */*")
        .call()
        .map_err(|err| anyhow!("fetching {url}: {err}"))?;
    use std::io::Read;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading response from {url}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn expand(template: &str, version: &str, tag: &str) -> String {
    template.replace("{version}", version).replace("{tag}", tag)
}

fn finish(version: String, url: String, sha256: String) -> ResolvedSource {
    ResolvedSource {
        version,
        url,
        sha256,
        resolved_at: chrono::Utc::now().to_rfc3339(),
    }
}

fn resolve_github(template: &str) -> Result<ResolvedSource> {
    let re = Regex::new(r"^https://github\.com/([^/]+)/([^/]+)/").unwrap();
    let captures = re
        .captures(template)
        .ok_or_else(|| anyhow!("cannot parse GitHub URL {template}"))?;
    let owner = &captures[1];
    let repo = &captures[2];
    let api = format!("https://api.github.com/repos/{owner}/{repo}/tags?per_page=100");
    let mut tags: Vec<String> = match http_get(&api) {
        Ok(text) => {
            let json: Value =
                serde_json::from_str(&text).context("parsing GitHub tags response")?;
            json.as_array()
                .ok_or_else(|| anyhow!("GitHub tags response is not an array"))?
                .iter()
                .filter_map(|entry| {
                    entry
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        }
        Err(_) => Vec::new(),
    };

    // The API does not always list recent tags first (GNOME/gtk is a notable
    // example), so merge the public tags page as well.
    if let Ok(html) = http_get(&format!("https://github.com/{owner}/{repo}/tags")) {
        let pattern = Regex::new(r#"releases/tag/([^"/?#]+)"#)?;
        tags.extend(
            pattern
                .captures_iter(&html)
                .map(|capture| capture[1].to_string()),
        );
    }
    tags.sort();
    tags.dedup();
    if tags.is_empty() {
        bail!("no tags found for {owner}/{repo}");
    }
    let tag =
        choose_version(&tags).ok_or_else(|| anyhow!("no stable tags found for {owner}/{repo}"))?;
    let version = strip_v(&tag);
    let url = expand(template, &version, &tag);
    Ok(finish(version, url, String::new()))
}

fn resolve_gitlab(template: &str) -> Result<ResolvedSource> {
    let re = Regex::new(r"^https://gitlab\.freedesktop\.org/(.+?)/-/archive/").unwrap();
    let captures = re
        .captures(template)
        .ok_or_else(|| anyhow!("cannot parse GitLab URL {template}"))?;
    let project = urlencoding::encode(&captures[1]);
    let api = format!(
        "https://gitlab.freedesktop.org/api/v4/projects/{project}/repository/tags?per_page=100"
    );
    let text = http_get(&api)?;
    let json: Value = serde_json::from_str(&text).context("parsing GitLab tags response")?;
    let tags: Vec<String> = json
        .as_array()
        .ok_or_else(|| anyhow!("GitLab tags response is not an array"))?
        .iter()
        .filter_map(|entry| {
            entry
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let tag = choose_version(&tags).ok_or_else(|| anyhow!("no stable tags found for {project}"))?;
    let version = strip_v(&tag);
    let url = expand(template, &version, &tag);
    Ok(finish(version, url, String::new()))
}

fn resolve_crates(template: &str) -> Result<ResolvedSource> {
    let re = Regex::new(r"/api/v1/crates/([^/]+)/\{version\}/download").unwrap();
    let captures = re
        .captures(template)
        .ok_or_else(|| anyhow!("cannot parse crates.io URL {template}"))?;
    let name = &captures[1];
    let text = http_get(&format!("https://crates.io/api/v1/crates/{name}"))?;
    let json: Value = serde_json::from_str(&text).context("parsing crates.io response")?;
    let version = json
        .get("crate")
        .and_then(|crate_info| crate_info.get("max_version"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("crates.io response has no max_version"))?
        .to_string();
    let checksum = json
        .get("versions")
        .and_then(Value::as_array)
        .and_then(|versions| {
            versions
                .iter()
                .find(|entry| entry.get("num").and_then(Value::as_str) == Some(version.as_str()))
        })
        .and_then(|entry| entry.get("checksum"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(finish(
        version.clone(),
        expand(template, &version, &version),
        checksum,
    ))
}

fn resolve_directory(recipe: &BuildRecipe, template: &str) -> Result<ResolvedSource> {
    let filename = template
        .rsplit('/')
        .next()
        .ok_or_else(|| anyhow!("invalid URL {template}"))?;
    let (prefix, suffix) = filename
        .split_once("{version}")
        .ok_or_else(|| anyhow!("URL {template} has no {{version}} placeholder"))?;
    let pattern = Regex::new(&format!(
        "^{}(\\d[0-9A-Za-z.]*){}$",
        regex::escape(prefix),
        regex::escape(suffix)
    ))?;

    let mut start = template
        .rsplit_once('/')
        .map(|(base, _)| base.to_string())
        .ok_or_else(|| anyhow!("invalid URL {template}"))?;
    let mut found: Vec<(String, String)> = Vec::new(); // (version, full URL)
    let mut visited = std::collections::BTreeSet::new();

    // Search the current directory and up to two parents. Parent listings
    // contain newer version directories (GNOME 2.86 -> 2.88, kernel v6.x -> v7.x)
    // which we recurse into.
    for level in 0..3 {
        search_listing(&start, &pattern, 0, &mut visited, &mut found)?;
        if level == 2 {
            break;
        }
        let Some((parent, _)) = start.rsplit_once('/') else {
            break;
        };
        start = parent.to_string();
    }

    let (version, url) = found
        .into_iter()
        .max_by(|a, b| compare_versions(&a.0, &b.0))
        .ok_or_else(|| {
            anyhow!(
                "no versions matching {filename} found for {}",
                recipe.package.name
            )
        })?;
    Ok(finish(version.clone(), url, String::new()))
}

fn search_listing(
    directory: &str,
    pattern: &Regex,
    depth: usize,
    visited: &mut std::collections::BTreeSet<String>,
    found: &mut Vec<(String, String)>,
) -> Result<()> {
    if !visited.insert(directory.to_string()) || depth > 2 {
        return Ok(());
    }
    let Ok(text) = http_get(directory) else {
        return Ok(());
    };
    let hrefs: Vec<String> = Regex::new(r#"href=["']([^"']+)["']"#)?
        .captures_iter(&text)
        .map(|capture| capture[1].replace("&amp;", "&"))
        .collect();

    for href in &hrefs {
        let name = href.rsplit('/').next().unwrap_or(href);
        if let Some(captures) = pattern.captures(name) {
            let version = captures[1].to_string();
            found.push((version, format!("{directory}/{name}")));
        }
    }

    if depth < 2 {
        let version_dir = Regex::new(r"^v?\d[\d.]*/?$").unwrap();
        let mut dirs: Vec<String> = hrefs
            .iter()
            .map(|href| href.trim_end_matches('/').to_string())
            .filter(|name| version_dir.is_match(name))
            .collect();
        dirs.sort_by(|a, b| compare_versions(&strip_v(a), &strip_v(b)).reverse());
        for name in dirs.into_iter().take(20) {
            let child = format!("{directory}/{name}");
            search_listing(&child, pattern, depth + 1, visited, found)?;
            if found.len() > 50 {
                break;
            }
        }
    }
    Ok(())
}

fn strip_v(tag: &str) -> String {
    match tag.strip_prefix('v') {
        Some(rest) if rest.starts_with(|ch: char| ch.is_ascii_digit()) => rest.to_string(),
        _ => tag.to_string(),
    }
}

fn is_prerelease(version: &str) -> bool {
    let lower = version.to_ascii_lowercase();
    ["rc", "beta", "alpha", "pre", "dev"]
        .iter()
        .any(|part| lower.contains(part))
}

fn choose_version(candidates: &[String]) -> Option<String> {
    candidates
        .iter()
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !is_prerelease(tag))
        .filter(|tag| !tag.is_empty())
        .max_by(|a, b| compare_versions(&strip_v(a), &strip_v(b)))
}

/// Natural version comparison: numeric runs compare numerically, text runs
/// lexically. Works for semver, CalVer, and kernel-style versions.
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let a = match a.strip_prefix('v') {
        Some(rest) if rest.starts_with(|ch: char| ch.is_ascii_digit()) => rest,
        _ => a,
    };
    let b = match b.strip_prefix('v') {
        Some(rest) if rest.starts_with(|ch: char| ch.is_ascii_digit()) => rest,
        _ => b,
    };
    let tokenize = |input: &str| -> Vec<String> {
        Regex::new(r"\d+|[A-Za-z]+")
            .unwrap()
            .find_iter(input)
            .map(|m| m.as_str().to_string())
            .collect()
    };
    let left = tokenize(a);
    let right = tokenize(b);
    for (left, right) in left.iter().zip(right.iter()) {
        let ordering = match (left.parse::<u64>(), right.parse::<u64>()) {
            (Ok(left), Ok(right)) => left.cmp(&right),
            (Ok(_), Err(_)) => Ordering::Greater,
            (Err(_), Ok(_)) => Ordering::Less,
            (Err(_), Err(_)) => left.cmp(right),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_naturally() {
        assert_eq!(compare_versions("1.10", "1.9"), std::cmp::Ordering::Greater);
        assert_eq!(
            compare_versions("20261019", "20260916"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(compare_versions("v1.2", "1.2"), std::cmp::Ordering::Equal);
    }

    #[test]
    fn filters_prereleases() {
        let tags = vec![
            "1.12".to_string(),
            "1.13-rc1".to_string(),
            "1.11".to_string(),
        ];
        assert_eq!(choose_version(&tags).as_deref(), Some("1.12"));
    }
}
