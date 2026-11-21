//! Transaction application, journaling, locking, and rollback.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::archive::{extract_package, fetch_package, with_payload_checksums};
use crate::config::Paths;
use crate::db::InstalledDb;
use crate::model::{InstallReason, InstalledPackage};
use crate::repo::RepoConfig;
use crate::solver::{Action, TransactionPlan};

/// Exclusive lock guarding mutating eva operations.
pub struct Lock {
    _file: File,
}

impl Lock {
    /// Acquire the lock, waiting up to `timeout`.
    pub fn acquire(path: &Path, timeout: std::time::Duration) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        bail!("another eva process is running (lock: {})", path.display());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(err) => return Err(err.into()),
            }
        }
    }
}

/// Errors specific to transaction application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionError {
    HookFailed {
        package: String,
        hook: String,
        code: i32,
        output: String,
    },
    FileConflict {
        path: String,
        packages: Vec<String>,
    },
    RollbackFailed(String),
    Other(String),
}

impl std::fmt::Display for TransactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HookFailed {
                package,
                hook,
                code,
                output,
            } => write!(
                f,
                "hook {hook} for {package} failed with exit code {code}: {output}"
            ),
            Self::FileConflict { path, packages } => {
                write!(f, "file conflict on {path} between {}", packages.join(", "))
            }
            Self::RollbackFailed(message) => write!(f, "rollback failed: {message}"),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for TransactionError {}

#[derive(Debug, Serialize, Deserialize)]
struct PlanFile {
    actions: Vec<Action>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct BackupManifest {
    /// Hex-encoded path -> original path relative to root.
    files: BTreeMap<String, String>,
    /// Files created by the transaction (no original backup exists).
    #[serde(default)]
    created: Vec<String>,
    /// Hex-encoded path -> original symlink target.
    #[serde(default)]
    symlinks: BTreeMap<String, String>,
    /// Symlinks created by the transaction.
    #[serde(default)]
    created_symlinks: Vec<String>,
}

/// An in-progress or committed transaction.
#[derive(Debug, Clone)]
pub struct Transaction {
    pub id: String,
    pub plan: TransactionPlan,
    paths: Paths,
}

impl Transaction {
    pub fn new(id: impl Into<String>, plan: TransactionPlan, paths: Paths) -> Self {
        Self {
            id: id.into(),
            plan,
            paths,
        }
    }

    pub fn journal_dir(&self) -> PathBuf {
        self.paths.journal_dir().join(&self.id)
    }

    /// Stage, apply, and commit the plan. On any failure, files and the
    /// database are restored from the journal before returning an error.
    pub fn commit(&self, repos: &[RepoConfig], db: &InstalledDb) -> Result<(), TransactionError> {
        let journal = self.journal_dir();
        std::fs::create_dir_all(journal.join("backup"))
            .map_err(|err| TransactionError::Other(err.to_string()))?;
        let plan_file = PlanFile {
            actions: self.plan.actions.clone(),
        };
        let plan_text = toml::to_string_pretty(&plan_file)
            .map_err(|err| TransactionError::Other(err.to_string()))?;
        std::fs::write(journal.join("plan.toml"), plan_text)
            .map_err(|err| TransactionError::Other(err.to_string()))?;
        std::fs::write(
            journal.join("assignments.json"),
            serde_json::to_string_pretty(&self.plan.assignments).unwrap(),
        )
        .map_err(|err| TransactionError::Other(err.to_string()))?;
        std::fs::write(journal.join("status"), "in-progress\n")
            .map_err(|err| TransactionError::Other(err.to_string()))?;

        // Best-effort btrfs snapshot; file-level rollback remains authoritative.
        create_snapshot(&self.paths, &self.id, &journal);

        db.backup_to(&journal.join("db.sqlite.bak"))
            .map_err(|err| TransactionError::Other(err.to_string()))?;
        db.record_txn(&self.id, "in-progress")
            .map_err(|err| TransactionError::Other(err.to_string()))?;

        let staging = self.paths.cache_dir().join("staging").join(&self.id);
        let _ = std::fs::remove_dir_all(&staging);

        let result = self.apply_actions(&staging, repos, db, &journal);
        let _ = std::fs::remove_dir_all(&staging);

        match result {
            Ok(()) => {
                std::fs::write(journal.join("status"), "committed\n")
                    .map_err(|err| TransactionError::Other(err.to_string()))?;
                db.record_txn(&self.id, "committed")
                    .map_err(|err| TransactionError::Other(err.to_string()))?;
                self.prune_journals();
                Ok(())
            }
            Err(err) => {
                self.rollback_internal(&journal, db)?;
                std::fs::write(journal.join("status"), "rolled-back\n")
                    .map_err(|err| TransactionError::Other(err.to_string()))?;
                Err(err)
            }
        }
    }

    fn apply_actions(
        &self,
        staging: &Path,
        repos: &[RepoConfig],
        db: &InstalledDb,
        journal: &Path,
    ) -> Result<(), TransactionError> {
        let mut manifest = BackupManifest::default();

        for action in &self.plan.actions {
            match action {
                Action::Remove { name } => {
                    self.apply_remove(name, staging, repos, db, journal, &mut manifest)?;
                }
                Action::Install { name, version } | Action::Reinstall { name, version } => {
                    let candidate = self.plan.assignments.get(name).ok_or_else(|| {
                        TransactionError::Other(format!("no metadata for {name}"))
                    })?;
                    if &candidate.id.version != version {
                        return Err(TransactionError::Other(format!(
                            "metadata version mismatch for {name}"
                        )));
                    }
                    let reason = if self.plan.explicit.contains(name) {
                        InstallReason::Explicit
                    } else {
                        InstallReason::Dependency
                    };
                    self.apply_install(
                        name,
                        candidate,
                        repos,
                        db,
                        staging,
                        journal,
                        &mut manifest,
                        reason,
                    )?;
                }
                Action::Upgrade { name, from, to } => {
                    let candidate = self.plan.assignments.get(name).ok_or_else(|| {
                        TransactionError::Other(format!("no metadata for {name}"))
                    })?;
                    if &candidate.id.version != to {
                        return Err(TransactionError::Other(format!(
                            "metadata version mismatch for {name}"
                        )));
                    }
                    let reason = db
                        .get(name)
                        .map(|package| {
                            package
                                .map(|package| package.reason)
                                .unwrap_or(InstallReason::Explicit)
                        })
                        .unwrap_or(InstallReason::Explicit);
                    if let Some(installed) = db.get(name).unwrap_or(None) {
                        if &installed.candidate.id.version != from {
                            return Err(TransactionError::Other(format!(
                                "installed version of {name} changed since planning"
                            )));
                        }
                    }
                    self.apply_install(
                        name,
                        candidate,
                        repos,
                        db,
                        staging,
                        journal,
                        &mut manifest,
                        reason,
                    )?;
                }
            }
        }

        std::fs::write(
            journal.join("backup/manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .map_err(|err| TransactionError::Other(err.to_string()))?;
        Ok(())
    }

    fn apply_remove(
        &self,
        name: &str,
        staging: &Path,
        repos: &[RepoConfig],
        db: &InstalledDb,
        journal: &Path,
        manifest: &mut BackupManifest,
    ) -> Result<(), TransactionError> {
        let installed = db
            .get(name)
            .map_err(|err| TransactionError::Other(err.to_string()))?;
        let Some(installed) = installed else {
            return Ok(());
        };

        let script_dir = self.prepare_scripts(name, &installed.candidate, staging, repos)?;
        self.run_hook(&installed.candidate, "pre_remove", script_dir.as_deref())?;

        for file in &installed.candidate.files {
            self.backup_and_remove(&file.path, journal, manifest)?;
        }
        for link in &installed.candidate.symlinks {
            self.backup_and_remove(&link.path, journal, manifest)?;
        }

        self.run_hook(&installed.candidate, "post_remove", script_dir.as_deref())?;
        db.remove(name)
            .map_err(|err| TransactionError::Other(err.to_string()))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_install(
        &self,
        name: &str,
        candidate: &crate::model::Candidate,
        repos: &[RepoConfig],
        db: &InstalledDb,
        staging: &Path,
        journal: &Path,
        manifest: &mut BackupManifest,
        reason: InstallReason,
    ) -> Result<(), TransactionError> {
        let package_path = fetch_package(candidate, repos, &self.paths)
            .map_err(|err| TransactionError::Other(err.to_string()))?;
        let extracted = staging.join(name);
        let _ = std::fs::remove_dir_all(&extracted);
        extract_package(&package_path, &extracted, &candidate.meta.checksum)
            .map_err(|err| TransactionError::Other(err.to_string()))?;

        let candidate = with_payload_checksums(candidate.clone(), &extracted)
            .map_err(|err| TransactionError::Other(err.to_string()))?;

        // Detect conflicts against files owned by other packages.
        let owners = db
            .file_owners()
            .map_err(|err| TransactionError::Other(err.to_string()))?;
        for file in &candidate.files {
            if let Some(owner) = owners.get(&file.path) {
                if owner != name && !candidate.replaces.contains(owner) {
                    return Err(TransactionError::FileConflict {
                        path: file.path.clone(),
                        packages: vec![owner.clone(), name.to_string()],
                    });
                }
            }
        }
        for link in &candidate.symlinks {
            if let Some(owner) = owners.get(&link.path) {
                if owner != name && !candidate.replaces.contains(owner) {
                    return Err(TransactionError::FileConflict {
                        path: link.path.clone(),
                        packages: vec![owner.clone(), name.to_string()],
                    });
                }
            }
        }

        self.run_hook(&candidate, "pre_install", Some(&extracted))?;

        for file in &candidate.files {
            let source = extracted.join(&file.path);
            let target = self.paths.resolve(&file.path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| TransactionError::Other(err.to_string()))?;
            }
            if std::fs::symlink_metadata(&target).is_ok() {
                self.backup_file(&target, &file.path, journal, manifest)?;
            } else {
                manifest.created.push(file.path.clone());
            }
            std::fs::copy(&source, &target)
                .map_err(|err| TransactionError::Other(err.to_string()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = file.mode & 0o7777;
                let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode));
            }
        }

        for link in &candidate.symlinks {
            let target = self.paths.resolve(&link.path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| TransactionError::Other(err.to_string()))?;
            }
            match std::fs::symlink_metadata(&target) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    let previous = std::fs::read_link(&target)
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let key = hex::encode(link.path.as_bytes());
                    manifest.symlinks.insert(key, previous);
                }
                Ok(_) => self.backup_file(&target, &link.path, journal, manifest)?,
                Err(_) => manifest.created_symlinks.push(link.path.clone()),
            }
            if std::fs::symlink_metadata(&target).is_ok() {
                std::fs::remove_file(&target)
                    .map_err(|err| TransactionError::Other(err.to_string()))?;
            }
            std::os::unix::fs::symlink(&link.target, &target)
                .map_err(|err| TransactionError::Other(err.to_string()))?;
        }

        let package = InstalledPackage {
            candidate,
            reason,
            installed_at: chrono::Utc::now().to_rfc3339(),
        };
        db.insert(&package)
            .map_err(|err| TransactionError::Other(err.to_string()))?;
        self.run_hook(&package.candidate, "post_install", Some(&extracted))?;
        Ok(())
    }

    fn prepare_scripts(
        &self,
        name: &str,
        candidate: &crate::model::Candidate,
        staging: &Path,
        repos: &[RepoConfig],
    ) -> Result<Option<PathBuf>, TransactionError> {
        if candidate.hooks.pre_remove.is_none() && candidate.hooks.post_remove.is_none() {
            return Ok(None);
        }
        let Ok(package_path) = fetch_package(candidate, repos, &self.paths) else {
            return Ok(None);
        };
        let directory = staging.join(format!("hooks-{name}"));
        let _ = std::fs::remove_dir_all(&directory);
        if extract_package(&package_path, &directory, &candidate.meta.checksum).is_err() {
            return Ok(None);
        }
        Ok(Some(directory))
    }

    fn run_hook(
        &self,
        candidate: &crate::model::Candidate,
        hook: &str,
        script_dir: Option<&Path>,
    ) -> Result<(), TransactionError> {
        let script = match hook {
            "pre_install" => candidate.hooks.pre_install.as_ref(),
            "post_install" => candidate.hooks.post_install.as_ref(),
            "pre_remove" => candidate.hooks.pre_remove.as_ref(),
            "post_remove" => candidate.hooks.post_remove.as_ref(),
            _ => None,
        };
        let Some(script) = script else {
            return Ok(());
        };
        let Some(script_dir) = script_dir else {
            return Ok(());
        };
        let path = script_dir.join(script);
        if !path.exists() {
            return Ok(());
        }

        let output = Command::new("/bin/sh")
            .arg(&path)
            .current_dir(&self.paths.root)
            .env("EVA_ROOT", &self.paths.root)
            .output()
            .map_err(|err| TransactionError::Other(err.to_string()))?;
        if !output.status.success() {
            return Err(TransactionError::HookFailed {
                package: candidate.id.name.clone(),
                hook: hook.to_string(),
                code: output.status.code().unwrap_or(-1),
                output: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(())
    }

    fn backup_file(
        &self,
        source: &Path,
        relative: &str,
        journal: &Path,
        manifest: &mut BackupManifest,
    ) -> Result<(), TransactionError> {
        let key = hex::encode(relative.as_bytes());
        let destination = journal.join("backup").join(&key);
        std::fs::copy(source, destination)
            .map_err(|err| TransactionError::Other(err.to_string()))?;
        manifest.files.insert(key, relative.to_string());
        Ok(())
    }

    fn backup_and_remove(
        &self,
        relative: &str,
        journal: &Path,
        manifest: &mut BackupManifest,
    ) -> Result<(), TransactionError> {
        let absolute = self.paths.resolve(relative);
        let Ok(metadata) = std::fs::symlink_metadata(&absolute) else {
            return Ok(());
        };
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&absolute)
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
            manifest
                .symlinks
                .insert(hex::encode(relative.as_bytes()), target);
        } else {
            self.backup_file(&absolute, relative, journal, manifest)?;
        }
        std::fs::remove_file(&absolute).map_err(|err| TransactionError::Other(err.to_string()))?;
        Ok(())
    }

    fn rollback_internal(&self, journal: &Path, db: &InstalledDb) -> Result<(), TransactionError> {
        let manifest_path = journal.join("backup/manifest.json");
        if let Ok(text) = std::fs::read_to_string(&manifest_path) {
            if let Ok(manifest) = serde_json::from_str::<BackupManifest>(&text) {
                for (key, relative) in manifest.files {
                    let backup = journal.join("backup").join(&key);
                    let target = self.paths.resolve(&relative);
                    if let Some(parent) = target.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if backup.exists() {
                        let _ = std::fs::copy(backup, target);
                    } else {
                        let _ = std::fs::remove_file(target);
                    }
                }
                for relative in manifest.created {
                    let _ = std::fs::remove_file(self.paths.resolve(&relative));
                }
                for (key, target) in manifest.symlinks {
                    let Ok(bytes) = hex::decode(&key) else {
                        continue;
                    };
                    let Ok(relative) = String::from_utf8(bytes) else {
                        continue;
                    };
                    let path = self.paths.resolve(&relative);
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if std::fs::symlink_metadata(&path).is_ok() {
                        let _ = std::fs::remove_file(&path);
                    }
                    let _ = std::os::unix::fs::symlink(target, path);
                }
                for relative in manifest.created_symlinks {
                    let _ = std::fs::remove_file(self.paths.resolve(&relative));
                }
            }
        }
        let backup_db = journal.join("db.sqlite.bak");
        if backup_db.exists() {
            let _ = std::fs::copy(&backup_db, &db.path);
        }
        let _ = db.record_txn(&self.id, "rolled-back");
        Ok(())
    }

    fn prune_journals(&self) {
        let Ok(entries) = std::fs::read_dir(self.paths.journal_dir()) else {
            return;
        };
        let mut committed: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                std::fs::read_to_string(path.join("status"))
                    .map(|status| status.trim() == "committed")
                    .unwrap_or(false)
            })
            .collect();
        committed.sort();
        while committed.len() > 5 {
            if let Some(old) = committed.first() {
                let _ = std::fs::remove_dir_all(old);
                committed.remove(0);
            }
        }
    }

    /// Roll back a previously committed transaction.
    pub fn rollback(paths: &Paths, id: Option<&str>) -> Result<String, TransactionError> {
        let root = paths.journal_dir();
        let target = match id {
            Some(id) => root.join(id),
            None => {
                let mut entries: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&root)
                    .map_err(|err| TransactionError::Other(err.to_string()))?
                    .flatten()
                    .map(|entry| {
                        let path = entry.path();
                        let modified = std::fs::metadata(&path)
                            .and_then(|meta| meta.modified())
                            .unwrap_or(std::time::UNIX_EPOCH);
                        (modified, path)
                    })
                    .filter(|(_, path)| {
                        std::fs::read_to_string(path.join("status"))
                            .map(|status| status.trim() == "committed")
                            .unwrap_or(false)
                    })
                    .collect();
                entries.sort();
                entries
                    .last()
                    .map(|(_, path)| path.clone())
                    .ok_or_else(|| {
                        TransactionError::Other("no committed transaction found".into())
                    })?
            }
        };
        if !target.exists() {
            return Err(TransactionError::Other(format!(
                "transaction {} not found",
                target.display()
            )));
        }

        let db =
            InstalledDb::open_at(paths).map_err(|err| TransactionError::Other(err.to_string()))?;
        let transaction = Self {
            id: target
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("unknown")
                .to_string(),
            plan: TransactionPlan::default(),
            paths: paths.clone(),
        };
        transaction.rollback_internal(&target, &db)?;
        std::fs::write(target.join("status"), "rolled-back\n")
            .map_err(|err| TransactionError::Other(err.to_string()))?;
        Ok(transaction.id)
    }
}

fn snapshot_parent(paths: &Paths) -> PathBuf {
    paths.db_dir().join("snapshots")
}

/// Attempt to create a read-only btrfs snapshot before applying a transaction.
/// Returns `None` when btrfs is unavailable; this is never fatal.
fn create_snapshot(paths: &Paths, id: &str, journal: &Path) -> Option<PathBuf> {
    let parent = snapshot_parent(paths);
    std::fs::create_dir_all(&parent).ok()?;
    let name = format!("@pre-eva-{id}");
    let destination = parent.join(&name);
    let status = Command::new("btrfs")
        .args(["subvolume", "snapshot", "-r"])
        .arg(&paths.root)
        .arg(&destination)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    std::fs::write(journal.join("snapshot.ref"), format!("{name}\n")).ok()?;
    Some(destination)
}

/// Restore files from a btrfs snapshot into the live root.
pub fn restore_snapshot(paths: &Paths, journal: &Path) -> Result<(), TransactionError> {
    let name = std::fs::read_to_string(journal.join("snapshot.ref"))
        .map_err(|err| TransactionError::Other(format!("no snapshot recorded: {err}")))?;
    let snapshot = snapshot_parent(paths).join(name.trim());
    if !snapshot.exists() {
        return Err(TransactionError::Other(format!(
            "snapshot {} is missing",
            snapshot.display()
        )));
    }
    copy_tree(&snapshot, &paths.root).map_err(|err| TransactionError::Other(err.to_string()))
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let target = destination.join(&name);
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_serializes_access() {
        let dir = std::env::temp_dir().join(format!(
            "eva-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let lock_path = dir.join("lock");
        let _first = Lock::acquire(&lock_path, std::time::Duration::from_millis(100)).unwrap();
        assert!(Lock::acquire(&lock_path, std::time::Duration::from_millis(20)).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn transaction_writes_journal() {
        let dir = std::env::temp_dir().join(format!(
            "eva-txn-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let paths = Paths::for_root(&dir);
        let transaction = Transaction::new("txn-1", TransactionPlan::default(), paths);
        std::fs::create_dir_all(transaction.journal_dir()).unwrap();
        std::fs::write(transaction.journal_dir().join("status"), "committed\n").unwrap();
        assert!(transaction.journal_dir().join("status").exists());
        std::fs::remove_dir_all(dir).ok();
    }
}
