//! cgroups v2 control for service resource limits.
//!
//! All operations are best-effort: an unprivileged or cgroup-less environment
//! simply logs a warning and continues.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Root cgroup for pine-managed services on a real system.
pub const PINE_CGROUP_ROOT: &str = "/sys/fs/cgroup/pine";

/// Resource limits applied to a service cgroup.
#[derive(Debug, Clone, Default)]
pub struct Limits {
    pub memory_max: Option<u64>,
    pub cpu_quota: Option<f64>,
    pub tasks_max: Option<u64>,
}

/// Per-service cgroup handle.
#[derive(Debug, Clone)]
pub struct Cgroup {
    pub root: PathBuf,
    pub name: String,
}

impl Cgroup {
    pub fn new(root: impl Into<PathBuf>, unit: &str) -> Self {
        Self {
            root: root.into(),
            name: sanitize(unit),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.root.join(&self.name)
    }

    /// Create the cgroup directory and enable controllers.
    pub fn create(&self) -> io::Result<()> {
        if !self.root.exists() {
            fs::create_dir_all(&self.root)?;
        }
        let subtree = self.root.join("cgroup.subtree_control");
        if subtree.exists() {
            let _ = fs::write(subtree, "+cpu +memory +pids +io");
        }
        fs::create_dir_all(self.path())
    }

    /// Place a PID into the cgroup.
    pub fn add_pid(&self, pid: u32) -> io::Result<()> {
        fs::write(self.path().join("cgroup.procs"), format!("{pid}\n"))
    }

    /// Apply limits to the cgroup.
    pub fn set_limits(&self, limits: &Limits) -> io::Result<()> {
        let path = self.path();
        if let Some(bytes) = limits.memory_max {
            let _ = fs::write(path.join("memory.max"), format!("{bytes}\n"));
        }
        if let Some(percent) = limits.cpu_quota {
            // cpu.max is "<quota> <period>" in microseconds.
            let period = 100_000f64;
            let quota = (percent / 100.0 * period).round().max(1.0) as u64;
            let _ = fs::write(path.join("cpu.max"), format!("{quota} 100000\n"));
        }
        if let Some(tasks) = limits.tasks_max {
            let _ = fs::write(path.join("pids.max"), format!("{tasks}\n"));
        }
        Ok(())
    }

    /// Kill every process in the cgroup.
    pub fn kill(&self) -> io::Result<()> {
        let path = self.path();
        let kill_file = path.join("cgroup.kill");
        if kill_file.exists() {
            fs::write(kill_file, "1\n")?;
            return Ok(());
        }

        if let Ok(contents) = fs::read_to_string(path.join("cgroup.procs")) {
            for line in contents.lines() {
                if let Ok(pid) = line.trim().parse::<i32>() {
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }
        Ok(())
    }

    /// Remove the cgroup directory.
    pub fn remove(&self) -> io::Result<()> {
        match fs::remove_dir(self.path()) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            // A real cgroupfs empties itself when the last process leaves; a
            // plain filesystem used in tests keeps controller files behind.
            Err(err) if err.raw_os_error() == Some(libc::ENOTEMPTY) => {
                fs::remove_dir_all(self.path())
            }
            Err(err) => Err(err),
        }
    }

    /// Whether the cgroup exists.
    pub fn exists(&self) -> bool {
        self.path().exists()
    }
}

fn sanitize(unit: &str) -> String {
    unit.chars()
        .map(|ch| match ch {
            '/' | '\\' | ' ' => '_',
            ch => ch,
        })
        .collect()
}

/// Check whether cgroup v2 is available at `root`.
pub fn available(root: &Path) -> bool {
    root.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pine-cgroup-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        // Fake controller files expected by create().
        fs::write(dir.join("cgroup.subtree_control"), "").unwrap();
        dir
    }

    #[test]
    fn creates_and_removes_cgroup() {
        let root = temp_root("one");
        let cgroup = Cgroup::new(&root, "demo.service");
        cgroup.create().unwrap();
        assert!(cgroup.exists());

        fs::write(cgroup.path().join("cgroup.procs"), "").unwrap();
        fs::write(cgroup.path().join("memory.max"), "").unwrap();
        fs::write(cgroup.path().join("cpu.max"), "").unwrap();
        fs::write(cgroup.path().join("pids.max"), "").unwrap();
        cgroup
            .set_limits(&Limits {
                memory_max: Some(1024),
                cpu_quota: Some(50.0),
                tasks_max: Some(10),
            })
            .unwrap();

        assert_eq!(
            fs::read_to_string(cgroup.path().join("memory.max"))
                .unwrap()
                .trim(),
            "1024"
        );
        cgroup.remove().unwrap();
        assert!(!cgroup.exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn sanitizes_unit_names() {
        assert_eq!(sanitize("a b.service"), "a_b.service");
    }
}
