//! Installed package database backed by SQLite.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::config::Paths;
use crate::model::{normalize, Candidate, InstallReason, InstalledPackage};

/// Handle to the installed package database.
#[derive(Debug, Clone)]
pub struct InstalledDb {
    pub path: PathBuf,
}

impl InstalledDb {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Self { path };
        db.connection()?.execute_batch(
            "CREATE TABLE IF NOT EXISTS packages (
                 name TEXT PRIMARY KEY,
                 epoch INTEGER NOT NULL,
                 core TEXT NOT NULL,
                 release INTEGER NOT NULL,
                 arch TEXT NOT NULL,
                 repo INTEGER NOT NULL,
                 metadata TEXT NOT NULL,
                 reason TEXT NOT NULL,
                 installed_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS files (
                 path TEXT PRIMARY KEY,
                 package TEXT NOT NULL,
                 sha256 TEXT NOT NULL,
                 mode INTEGER NOT NULL,
                 kind TEXT NOT NULL DEFAULT 'file',
                 link_target TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE IF NOT EXISTS txns (
                 id TEXT PRIMARY KEY,
                 status TEXT NOT NULL,
                 created TEXT NOT NULL
             );",
        )?;
        db.migrate_files_table()?;
        Ok(db)
    }

    fn migrate_files_table(&self) -> Result<()> {
        let connection = self.connection()?;
        let mut statement = connection.prepare("PRAGMA table_info(files)")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
        let mut columns = std::collections::BTreeSet::new();
        for column in rows {
            columns.insert(column?);
        }
        drop(statement);
        if !columns.contains("kind") {
            connection.execute(
                "ALTER TABLE files ADD COLUMN kind TEXT NOT NULL DEFAULT 'file'",
                [],
            )?;
        }
        if !columns.contains("link_target") {
            connection.execute(
                "ALTER TABLE files ADD COLUMN link_target TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        Ok(())
    }

    pub fn open_at(paths: &Paths) -> Result<Self> {
        Self::open(paths.installed_db())
    }

    fn connection(&self) -> Result<Connection> {
        Connection::open(&self.path).context("opening installed package database")
    }

    /// List every installed package, sorted by name.
    pub fn list(&self) -> Result<Vec<InstalledPackage>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT metadata, reason, installed_at FROM packages ORDER BY name")?;
        let rows = statement.query_map([], |row| {
            let metadata: String = row.get(0)?;
            let reason: String = row.get(1)?;
            let installed_at: String = row.get(2)?;
            Ok((metadata, reason, installed_at))
        })?;

        let mut packages = Vec::new();
        for row in rows {
            let (metadata, reason, installed_at) = row?;
            let candidate: Candidate = serde_json::from_str(&metadata)?;
            packages.push(InstalledPackage {
                candidate,
                reason: if reason == "explicit" {
                    InstallReason::Explicit
                } else {
                    InstallReason::Dependency
                },
                installed_at,
            });
        }
        Ok(packages)
    }

    /// Look up one installed package by normalized name.
    pub fn get(&self, name: &str) -> Result<Option<InstalledPackage>> {
        let connection = self.connection()?;
        let row: Option<(String, String, String)> = connection
            .query_row(
                "SELECT metadata, reason, installed_at FROM packages WHERE name = ?1",
                params![normalize(name)],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((metadata, reason, installed_at)) => Ok(Some(InstalledPackage {
                candidate: serde_json::from_str(&metadata)?,
                reason: if reason == "explicit" {
                    InstallReason::Explicit
                } else {
                    InstallReason::Dependency
                },
                installed_at,
            })),
        }
    }

    /// Insert or replace an installed package and its owned files.
    pub fn insert(&self, package: &InstalledPackage) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let candidate = &package.candidate;
        let reason = match package.reason {
            InstallReason::Explicit => "explicit",
            InstallReason::Dependency => "dependency",
        };
        let metadata = serde_json::to_string(candidate)?;
        transaction.execute(
            "INSERT INTO packages (name, epoch, core, release, arch, repo, metadata, reason, installed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(name) DO UPDATE SET
                 epoch=excluded.epoch, core=excluded.core, release=excluded.release,
                 arch=excluded.arch, repo=excluded.repo, metadata=excluded.metadata,
                 reason=excluded.reason, installed_at=excluded.installed_at",
            params![
                candidate.id.name,
                candidate.id.version.epoch,
                candidate.id.version.to_string(),
                candidate.id.version.release,
                candidate.id.arch.to_string(),
                candidate.id.repo.0,
                metadata,
                reason,
                package.installed_at,
            ],
        )?;
        transaction.execute(
            "DELETE FROM files WHERE package = ?1",
            params![candidate.id.name],
        )?;
        for file in &candidate.files {
            transaction.execute(
                "INSERT INTO files (path, package, sha256, mode, kind, link_target)
                 VALUES (?1, ?2, ?3, ?4, 'file', '')
                 ON CONFLICT(path) DO UPDATE SET package=excluded.package,
                     sha256=excluded.sha256, mode=excluded.mode,
                     kind='file', link_target=''",
                params![file.path, candidate.id.name, file.sha256, file.mode],
            )?;
        }
        for link in &candidate.symlinks {
            transaction.execute(
                "INSERT INTO files (path, package, sha256, mode, kind, link_target)
                 VALUES (?1, ?2, '', ?3, 'symlink', ?4)
                 ON CONFLICT(path) DO UPDATE SET package=excluded.package,
                     sha256='', mode=excluded.mode,
                     kind='symlink', link_target=excluded.link_target",
                params![link.path, candidate.id.name, link.mode, link.target],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Remove a package and its owned file records.
    pub fn remove(&self, name: &str) -> Result<()> {
        let name = normalize(name);
        let connection = self.connection()?;
        connection.execute("DELETE FROM files WHERE package = ?1", params![name])?;
        connection.execute("DELETE FROM packages WHERE name = ?1", params![name])?;
        Ok(())
    }

    /// Change a package's install reason.
    pub fn set_reason(&self, name: &str, reason: InstallReason) -> Result<()> {
        let reason = match reason {
            InstallReason::Explicit => "explicit",
            InstallReason::Dependency => "dependency",
        };
        let connection = self.connection()?;
        connection.execute(
            "UPDATE packages SET reason = ?1 WHERE name = ?2",
            params![reason, normalize(name)],
        )?;
        Ok(())
    }

    /// Map owned paths to their owning package.
    pub fn file_owners(&self) -> Result<BTreeMap<String, String>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT path, package FROM files")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut owners = BTreeMap::new();
        for row in rows {
            let (path, package): (String, String) = row?;
            owners.insert(path, package);
        }
        Ok(owners)
    }

    /// Files owned by one package.
    pub fn files(&self, name: &str) -> Result<Vec<(String, String)>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT path, sha256 FROM files WHERE package = ?1 AND kind = 'file' ORDER BY path",
        )?;
        let rows = statement.query_map(params![normalize(name)], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Record transaction status in the database.
    pub fn record_txn(&self, id: &str, status: &str) -> Result<()> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO txns (id, status, created) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET status=excluded.status",
            params![id, status, chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Copy the database file to `destination` for journaling.
    pub fn backup_to(&self, destination: &Path) -> Result<()> {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&self.path, destination)?;
        Ok(())
    }

    /// Verify checksums of all installed files under `paths.root`.
    pub fn verify_files(&self, paths: &Paths) -> Result<Vec<VerifyProblem>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT path, package, sha256, kind, link_target FROM files ORDER BY path")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;

        let mut problems = Vec::new();
        for row in rows {
            let (path, package, expected, kind, link_target) = row?;
            let absolute = paths.resolve(&path);
            if std::fs::symlink_metadata(&absolute).is_err() {
                problems.push(VerifyProblem {
                    path,
                    package,
                    kind: VerifyKind::Missing,
                });
                continue;
            }
            if kind == "symlink" {
                let actual = std::fs::read_link(&absolute)
                    .map(|target| target.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if actual != link_target {
                    problems.push(VerifyProblem {
                        path,
                        package,
                        kind: VerifyKind::ChecksumMismatch {
                            expected: link_target,
                            actual,
                        },
                    });
                }
                continue;
            }
            let digest = match std::fs::read(&absolute) {
                Ok(bytes) => hex::encode(Sha256::digest(bytes)),
                Err(_) => {
                    problems.push(VerifyProblem {
                        path,
                        package,
                        kind: VerifyKind::Unreadable,
                    });
                    continue;
                }
            };
            if !expected.is_empty() && digest != expected {
                problems.push(VerifyProblem {
                    path,
                    package,
                    kind: VerifyKind::ChecksumMismatch {
                        expected,
                        actual: digest,
                    },
                });
            }
        }
        Ok(problems)
    }
}

/// A problem found while verifying installed files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyProblem {
    pub path: String,
    pub package: String,
    pub kind: VerifyKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyKind {
    Missing,
    Unreadable,
    ChecksumMismatch { expected: String, actual: String },
}

impl std::fmt::Display for VerifyProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            VerifyKind::Missing => write!(f, "{}: missing (owned by {})", self.path, self.package),
            VerifyKind::Unreadable => {
                write!(f, "{}: unreadable (owned by {})", self.path, self.package)
            }
            VerifyKind::ChecksumMismatch { expected, actual } => write!(
                f,
                "{}: checksum mismatch (owned by {}, expected {expected}, got {actual})",
                self.path, self.package
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Arch, FileEntry, PackageId, PackageMeta};
    use evergreen_common::{RepoId, Version};

    fn candidate(name: &str, version: &str) -> Candidate {
        Candidate {
            id: PackageId {
                name: name.to_string(),
                version: Version::parse(version).unwrap(),
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
            files: vec![FileEntry {
                path: "usr/share/demo.txt".to_string(),
                sha256: String::new(),
                mode: 0o644,
            }],
            symlinks: Vec::new(),
            hooks: Default::default(),
            archive: String::new(),
        }
    }

    #[test]
    fn stores_and_lists_packages() {
        let dir = std::env::temp_dir().join(format!(
            "eva-db-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db = InstalledDb::open(dir.join("installed.db")).unwrap();
        db.insert(&InstalledPackage {
            candidate: candidate("demo", "1.0"),
            reason: InstallReason::Explicit,
            installed_at: "now".to_string(),
        })
        .unwrap();

        let list = db.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].candidate.id.name, "demo");
        assert_eq!(list[0].reason, InstallReason::Explicit);
        assert!(db.file_owners().unwrap().contains_key("usr/share/demo.txt"));

        db.remove("demo").unwrap();
        assert!(db.list().unwrap().is_empty());
        std::fs::remove_dir_all(dir).ok();
    }
}
