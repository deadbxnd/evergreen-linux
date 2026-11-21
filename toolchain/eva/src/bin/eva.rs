//! `eva` — command-line interface.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use serde_json::json;

use eva::build::{build_package, load_recipe};
use eva::db::InstalledDb;
use eva::model::{normalize, PackageSpec, Request, UpgradeMode};
use eva::repo::{self, RepoConfig};
use eva::resolver;
use eva::solver::{Action, Resolver, TransactionPlan};
use eva::transaction::{Lock, Transaction};
use eva::{config, EvaConfig, Paths};

#[derive(Debug, Parser)]
#[command(name = "eva", version, about = "Evergreen Linux package manager")]
struct Cli {
    /// Alternate filesystem root (mainly for testing).
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    /// Allow unsigned repository indexes (development only).
    #[arg(long, global = true)]
    allow_unsigned: bool,

    /// Do not ask for confirmation before applying a transaction.
    #[arg(short = 'y', long, global = true)]
    yes: bool,

    /// Print machine-readable JSON where supported.
    #[arg(long, global = true)]
    json: bool,

    /// Force a refresh of dynamically resolved package versions.
    #[arg(long, global = true)]
    refresh: bool,

    /// Do not install recommended packages.
    #[arg(long, global = true)]
    no_recommends: bool,

    /// Upgrade held packages as well.
    #[arg(long, global = true)]
    force_holds: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Download and verify repository indexes.
    Sync,
    /// Install packages.
    Install {
        /// Package names, optionally with a constraint (e.g. `firefox>=128`).
        packages: Vec<String>,
    },
    /// Remove packages and orphaned dependencies.
    Remove { packages: Vec<String> },
    /// Upgrade all packages or a named subset.
    Upgrade {
        /// Optional package names.
        packages: Vec<String>,
    },
    /// Reinstall installed packages at their current version.
    Reinstall { packages: Vec<String> },
    /// Search available packages.
    Search { term: String },
    /// Show detailed information about a package.
    Info { package: String },
    /// Inspect dynamically resolved package sources.
    Source {
        #[command(subcommand)]
        action: SourceAction,
    },
    /// List installed or available packages.
    List {
        #[arg(long)]
        installed: bool,
        #[arg(long)]
        available: bool,
    },
    /// Verify installed file checksums.
    Verify {
        /// Limit verification to one package.
        package: Option<String>,
    },
    /// Build a `.eva` package from a source recipe.
    Build {
        recipe: PathBuf,
        /// Install the resulting package after building.
        #[arg(long)]
        install: bool,
    },
    /// Generate a repository index from `.eva` packages.
    Index {
        /// Directory containing `.eva` archives.
        directory: PathBuf,
        #[arg(long, default_value = "core")]
        name: String,
        #[arg(long, default_value = "stable")]
        branch: String,
        /// Signing key file (hex secret key); omit for unsigned indexes.
        #[arg(long)]
        sign: Option<PathBuf>,
    },
    /// Roll back a committed transaction.
    Rollback {
        /// Transaction id; defaults to the most recent committed one.
        id: Option<String>,
        /// Restore the btrfs snapshot instead of only files and the database.
        #[arg(long)]
        snapshot: bool,
    },
    /// Manage configured repositories.
    Repo {
        #[command(subcommand)]
        action: RepoCommand,
    },
    /// Manage trusted repository signing keys.
    Key {
        #[command(subcommand)]
        action: KeyCommand,
    },
}

#[derive(Debug, Subcommand)]
enum SourceAction {
    /// Resolve the newest upstream version for a recipe.
    Resolve {
        recipe: PathBuf,
        #[arg(long)]
        refresh: bool,
    },
}

#[derive(Debug, Subcommand)]
enum RepoCommand {
    /// Add a repository.
    Add {
        url: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "stable")]
        branch: String,
        #[arg(long, default_value_t = 0)]
        priority: i32,
    },
    /// Remove a repository by name.
    Remove { name: String },
    /// List configured repositories.
    List,
}

#[derive(Debug, Subcommand)]
enum KeyCommand {
    /// Add a public key (path or hex).
    Add { key: String },
    /// List trusted keys.
    List,
    /// Generate a new ed25519 signing keypair.
    Generate {
        #[arg(long, default_value = "release")]
        label: String,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Print the fingerprint of a key file or hex string.
    Fingerprint { key: String },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("eva: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let paths = match &cli.root {
        Some(root) => Paths::for_root(root),
        None => Paths::system(),
    };
    let config = EvaConfig::load(&paths.eva_config())?;
    let repos = repo::load_config(&paths.repo_config())?;
    let db = InstalledDb::open_at(&paths)?;

    match &cli.command {
        Command::Sync => {
            let verify = config.verify_signatures && !cli.allow_unsigned;
            let mut total = 0;
            for repo in repos.iter().filter(|repo| repo.enabled) {
                let count = repo::sync_repo(&paths, repo, verify)?;
                println!("synced {}: {count} packages", repo.name);
                total += count;
            }
            println!("{total} package versions indexed");
        }
        Command::Repo { action } => {
            return run_repo(&paths, action);
        }
        Command::Key { action } => {
            return run_key(&paths, action);
        }
        Command::List {
            installed,
            available,
        } => {
            return run_list(&paths, &db, &repos, *installed, *available, cli.json);
        }
        Command::Search { term } => {
            return run_search(&paths, &repos, term, cli.json);
        }
        Command::Info { package } => {
            return run_info(&paths, &db, &repos, package, cli.json);
        }
        Command::Source { action } => match action {
            SourceAction::Resolve { recipe, refresh } => {
                let recipe = eva::build::load_recipe(recipe)?;
                match resolver::resolve(&recipe, &paths, *refresh || cli.refresh)? {
                    Some(resolved) => {
                        println!("version: {}", resolved.version);
                        println!("url: {}", resolved.url);
                        println!("sha256: {}", resolved.sha256);
                    }
                    None => println!("recipe uses a static or local source"),
                }
            }
        },
        Command::Verify { package } => {
            let problems = db.verify_files(&paths)?;
            let problems: Vec<_> = problems
                .into_iter()
                .filter(|problem| {
                    package
                        .as_ref()
                        .map_or(true, |name| problem.package == normalize(name))
                })
                .collect();
            if cli.json {
                let items: Vec<_> = problems
                    .iter()
                    .map(|problem| json!({"path": problem.path, "package": problem.package, "problem": problem.to_string()}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else if problems.is_empty() {
                println!("all installed files verified");
            } else {
                for problem in &problems {
                    eprintln!("{problem}");
                }
            }
            return Ok(if problems.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            });
        }
        Command::Rollback { id, snapshot } => {
            let _lock = Lock::acquire(&paths.lock_file(), std::time::Duration::from_secs(60))?;
            if *snapshot {
                let id = id.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("--snapshot requires an explicit transaction id")
                })?;
                let journal = paths.journal_dir().join(id);
                eva::transaction::restore_snapshot(&paths, &journal)?;
            }
            let rolled_back = Transaction::rollback(&paths, id.as_deref())?;
            println!("rolled back transaction {rolled_back}");
        }
        Command::Index {
            directory,
            name,
            branch,
            sign,
        } => {
            let index = eva::publish::build_index(directory, name, branch)?;
            let (index_path, signature) =
                eva::publish::write_signed_index(directory, &index, sign.as_deref())?;
            println!("wrote {}", index_path.display());
            if let Some(signature) = signature {
                println!("wrote {}", signature.display());
            }
            println!("{} packages indexed", index.packages.len());
        }
        Command::Build { recipe, install } => {
            let _lock = Lock::acquire(&paths.lock_file(), std::time::Duration::from_secs(60))?;
            let recipe_file = recipe;
            let recipe = load_recipe(recipe_file)?;
            let candidates = repo::load_candidates(&paths, &repos)?;
            let installed = db.list()?;
            let result = build_package(
                &paths,
                recipe_file,
                &recipe,
                candidates,
                installed,
                cli.refresh,
            )?;
            println!(
                "built {} -> {} ({})",
                result.package.id.name,
                result.archive.display(),
                result.checksum
            );
            if *install {
                let mut plan = TransactionPlan {
                    actions: vec![Action::Install {
                        name: result.package.id.name.clone(),
                        version: result.package.id.version.clone(),
                    }],
                    assignments: Default::default(),
                    explicit: [result.package.id.name.clone()].into_iter().collect(),
                };
                plan.assignments
                    .insert(result.package.id.name.clone(), result.package.clone());
                let transaction = new_transaction(plan, &paths);
                transaction.commit(&repos, &db)?;
                println!("installed {}", result.package.id.name);
            }
        }
        Command::Install { packages }
        | Command::Remove { packages }
        | Command::Upgrade { packages }
        | Command::Reinstall { packages } => {
            let request = build_request(&cli.command, packages)?;
            let mut resolver = make_resolver(&paths, &config, &repos, &db)?;
            resolver.config.install_recommends = config.install_recommends && !cli.no_recommends;
            if cli.force_holds {
                resolver.set_holds(Default::default());
            }
            let plan = match resolver.resolve(&request) {
                Ok(plan) => plan,
                Err(err) => {
                    eprintln!("{err}");
                    return Ok(ExitCode::FAILURE);
                }
            };
            if plan.is_empty() {
                println!("nothing to do");
                return Ok(ExitCode::SUCCESS);
            }
            if !cli.yes && !confirm(&plan)? {
                println!("aborted");
                return Ok(ExitCode::SUCCESS);
            }
            let _lock = Lock::acquire(&paths.lock_file(), std::time::Duration::from_secs(60))?;
            let transaction = new_transaction(plan, &paths);
            transaction.commit(&repos, &db)?;
            println!("transaction {} committed", transaction.id);
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn build_request(command: &Command, packages: &[String]) -> Result<Request> {
    match command {
        Command::Install { .. } => Ok(Request {
            install: packages
                .iter()
                .map(|spec| PackageSpec::parse(spec))
                .collect::<Result<Vec<_>, _>>()?,
            ..Request::default()
        }),
        Command::Remove { .. } => Ok(Request {
            remove: packages.iter().map(|name| normalize(name)).collect(),
            ..Request::default()
        }),
        Command::Upgrade { .. } => Ok(Request {
            upgrade: if packages.is_empty() {
                UpgradeMode::All
            } else {
                UpgradeMode::Specific(packages.iter().map(|name| normalize(name)).collect())
            },
            ..Request::default()
        }),
        Command::Reinstall { .. } => Ok(Request {
            reinstall: packages.iter().map(|name| normalize(name)).collect(),
            ..Request::default()
        }),
        other => bail!("not a mutating command: {other:?}"),
    }
}

fn make_resolver(
    paths: &Paths,
    config: &EvaConfig,
    repos: &[RepoConfig],
    db: &InstalledDb,
) -> Result<Resolver> {
    let mut resolver = Resolver::new();
    resolver.add_candidates(repo::load_candidates(paths, repos)?);
    resolver.set_installed(db.list()?);
    resolver.set_features(config::load_features(&paths.features_file())?);
    resolver.set_pins(config::load_pins(&paths.pins_file())?);
    resolver.set_holds(config::load_holds(&paths.holds_file())?);
    resolver.config = config.clone();
    resolver.root = paths.root.clone();
    Ok(resolver)
}

fn new_transaction(plan: TransactionPlan, paths: &Paths) -> Transaction {
    let id = format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%d%H%M%S%f"),
        std::process::id()
    );
    Transaction::new(id, plan, paths.clone())
}

fn confirm(plan: &TransactionPlan) -> Result<bool> {
    println!("{plan}");
    print!("Proceed? [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer)? == 0 {
        return Ok(false);
    }
    Ok(answer.trim().eq_ignore_ascii_case("y") || answer.trim().eq_ignore_ascii_case("yes"))
}

fn run_repo(paths: &Paths, action: &RepoCommand) -> Result<ExitCode> {
    let mut repos = repo::load_config(&paths.repo_config())?;
    match action {
        RepoCommand::Add {
            url,
            name,
            branch,
            priority,
        } => {
            let name = name.clone().unwrap_or_else(|| {
                url.trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("repo")
                    .trim_end_matches(".toml")
                    .to_string()
            });
            if repos.iter().any(|repo| repo.name == name) {
                bail!("repository {name} already exists");
            }
            repos.push(RepoConfig {
                name: name.clone(),
                url: url.clone(),
                branch: branch.clone(),
                priority: *priority,
                enabled: true,
            });
            repo::save_config(&paths.repo_config(), &repos)?;
            println!("added repository {name}");
        }
        RepoCommand::Remove { name } => {
            let before = repos.len();
            repos.retain(|repo| repo.name != *name);
            if repos.len() == before {
                bail!("repository {name} not found");
            }
            repo::save_config(&paths.repo_config(), &repos)?;
            println!("removed repository {name}");
        }
        RepoCommand::List => {
            if repos.is_empty() {
                println!("no repositories configured");
            }
            for repo in &repos {
                println!(
                    "{:<16} {:<24} branch={:<8} priority={} enabled={}",
                    repo.name, repo.url, repo.branch, repo.priority, repo.enabled
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_key(paths: &Paths, action: &KeyCommand) -> Result<ExitCode> {
    match action {
        KeyCommand::Add { key } => {
            let path = repo::add_key(paths, key)?;
            println!("added key {}", path.display());
        }
        KeyCommand::List => {
            let keys = repo::list_keys(paths)?;
            if keys.is_empty() {
                println!("no trusted keys configured");
            }
            for (path, fingerprint) in keys {
                println!("{} {}", fingerprint, path.display());
            }
        }
        KeyCommand::Generate { label, output } => {
            let directory = output
                .clone()
                .unwrap_or_else(|| paths.config_dir().join("keys"));
            let (secret, public) = eva::publish::generate_key_pair(&directory, label)?;
            println!("secret: {}", secret.display());
            println!("public: {}", public.display());
            println!(
                "fingerprint: {}",
                eva::publish::fingerprint(&std::fs::read_to_string(public)?)?
            );
        }
        KeyCommand::Fingerprint { key } => {
            let contents = if Path::new(key).exists() {
                std::fs::read_to_string(key)?
            } else {
                key.clone()
            };
            println!("{}", eva::publish::fingerprint(&contents)?);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_list(
    paths: &Paths,
    db: &InstalledDb,
    repos: &[RepoConfig],
    installed_only: bool,
    available_only: bool,
    json: bool,
) -> Result<ExitCode> {
    let installed = db.list()?;
    let available = if installed_only {
        Vec::new()
    } else {
        repo::load_candidates(paths, repos)?
    };

    if json {
        let items: Vec<_> = installed
            .iter()
            .map(|package| {
                json!({
                    "name": package.candidate.id.name,
                    "version": package.candidate.id.version.to_string(),
                    "reason": format!("{:?}", package.reason),
                    "installed": true,
                })
            })
            .chain(available.iter().map(|candidate| {
                json!({
                    "name": candidate.id.name,
                    "version": candidate.id.version.to_string(),
                    "arch": candidate.id.arch.to_string(),
                    "description": candidate.meta.description,
                    "installed": false,
                })
            }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(ExitCode::SUCCESS);
    }

    if !available_only {
        println!(
            "{:<28} {:<14} {:<12} DESCRIPTION",
            "INSTALLED", "VERSION", "REASON"
        );
        for package in &installed {
            println!(
                "{:<28} {:<14} {:<12} {}",
                package.candidate.id.name,
                package.candidate.id.version,
                format!("{:?}", package.reason),
                package.candidate.meta.description
            );
        }
    }
    if !installed_only {
        println!();
        println!(
            "{:<28} {:<14} {:<10} DESCRIPTION",
            "AVAILABLE", "VERSION", "ARCH"
        );
        for candidate in &available {
            println!(
                "{:<28} {:<14} {:<10} {}",
                candidate.id.name,
                candidate.id.version,
                candidate.id.arch,
                candidate.meta.description
            );
        }
    }
    let _ = paths;
    Ok(ExitCode::SUCCESS)
}

fn run_search(paths: &Paths, repos: &[RepoConfig], term: &str, json: bool) -> Result<ExitCode> {
    let needle = term.to_lowercase();
    let matches: Vec<_> = repo::load_candidates(paths, repos)?
        .into_iter()
        .filter(|candidate| {
            candidate.id.name.contains(&needle)
                || candidate.meta.description.to_lowercase().contains(&needle)
        })
        .collect();
    if json {
        let items: Vec<_> = matches
            .iter()
            .map(|candidate| {
                json!({
                    "name": candidate.id.name,
                    "version": candidate.id.version.to_string(),
                    "description": candidate.meta.description,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else {
        for candidate in &matches {
            println!(
                "{:<28} {:<14} {}",
                candidate.id.name, candidate.id.version, candidate.meta.description
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_info(
    paths: &Paths,
    db: &InstalledDb,
    repos: &[RepoConfig],
    name: &str,
    json: bool,
) -> Result<ExitCode> {
    let name = normalize(name);
    let installed = db.get(&name)?;
    let available: Vec<_> = repo::load_candidates(paths, repos)?
        .into_iter()
        .filter(|candidate| {
            candidate.id.name == name || candidate.satisfied_names().contains(&name)
        })
        .collect();

    if installed.is_none() && available.is_empty() {
        bail!("no such package: {name}");
    }

    if json {
        let value = json!({
            "name": name,
            "installed": installed.as_ref().map(|package| package.candidate.id.version.to_string()),
            "candidates": available.iter().map(|candidate| json!({
                "version": candidate.id.version.to_string(),
                "arch": candidate.id.arch.to_string(),
                "description": candidate.meta.description,
                "dependencies": candidate.deps.iter().map(|dep| json!({
                    "name": dep.name, "constraint": dep.constraint.to_string()
                })).collect::<Vec<_>>(),
                "provides": candidate.provides.iter().map(|provide| provide.name.clone()).collect::<Vec<_>>(),
                "conflicts": candidate.conflicts.iter().map(|conflict| conflict.name.clone()).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("Package: {name}");
        if let Some(package) = &installed {
            println!("Installed: {}", package.candidate.id.version);
        } else {
            println!("Installed: no");
        }
        for candidate in &available {
            println!();
            println!("Version: {} ({})", candidate.id.version, candidate.id.arch);
            println!("Description: {}", candidate.meta.description);
            if !candidate.meta.url.is_empty() {
                println!("URL: {}", candidate.meta.url);
            }
            if !candidate.meta.license.is_empty() {
                println!("License: {}", candidate.meta.license.join(", "));
            }
            if !candidate.deps.is_empty() {
                println!("Dependencies:");
                for dep in &candidate.deps {
                    println!("  {} {}", dep.name, dep.constraint);
                }
            }
            if !candidate.provides.is_empty() {
                println!(
                    "Provides: {}",
                    candidate
                        .provides
                        .iter()
                        .map(|provide| provide.name.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if !candidate.recommends.is_empty() {
                println!(
                    "Recommends: {}",
                    candidate
                        .recommends
                        .iter()
                        .map(|dep| dep.name.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if !candidate.suggests.is_empty() {
                println!(
                    "Suggests: {}",
                    candidate
                        .suggests
                        .iter()
                        .map(|dep| dep.name.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
