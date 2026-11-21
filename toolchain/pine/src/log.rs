//! `pinelog`: per-unit and global log files under `<root>/var/log/pine`.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, SecondsFormat, Utc};

/// Default log directory.
pub const LOG_DIR: &str = "/var/log/pine";

/// A thread-safe-ish log writer. Callers wrap it in `Arc<Mutex<Logger>>`.
pub struct Logger {
    base_dir: PathBuf,
    console: bool,
    max_size: u64,
    keep: usize,
    files: HashMap<String, File>,
}

impl std::fmt::Debug for Logger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logger")
            .field("base_dir", &self.base_dir)
            .field("console", &self.console)
            .finish()
    }
}

impl Logger {
    pub fn new(base_dir: impl Into<PathBuf>, console: bool) -> std::io::Result<Self> {
        let base_dir = base_dir.into();
        std::fs::create_dir_all(&base_dir)?;
        Ok(Self {
            base_dir,
            console,
            max_size: 10 * 1024 * 1024,
            keep: 5,
            files: HashMap::new(),
        })
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.base_dir.join(format!("{key}.log"))
    }

    /// Log a service line, prefixed with a timestamp, unit name, and PID.
    pub fn log(&mut self, unit: &str, pid: Option<u32>, message: &str) {
        let timestamp = timestamp();
        let pid = pid.map(|pid| format!("[{pid}]")).unwrap_or_default();
        let line = format!("{timestamp} [{unit}]{pid} {message}\n");
        if self.console {
            eprint!("{line}");
        }
        let key = sanitize(unit);
        if let Err(err) = self.append(&key, line.as_bytes()) {
            eprintln!("pinelog: failed to write log for {unit}: {err}");
        }
    }

    /// Log a manager-level message to `pine.log`.
    pub fn global(&mut self, level: &str, message: &str) {
        let timestamp = timestamp();
        let line = format!("{timestamp} [{level}] {message}\n");
        if self.console {
            eprint!("{line}");
        }
        if let Err(err) = self.append("pine", line.as_bytes()) {
            eprintln!("pinelog: failed to write manager log: {err}");
        }
    }

    fn append(&mut self, key: &str, bytes: &[u8]) -> std::io::Result<()> {
        let path = self.path_for(key);
        let needs_rotate = self
            .files
            .get(key)
            .and_then(|file| file.metadata().ok())
            .map(|meta| meta.len() + bytes.len() as u64 > self.max_size)
            .unwrap_or_else(|| {
                std::fs::metadata(&path)
                    .map(|meta| meta.len() + bytes.len() as u64 > self.max_size)
                    .unwrap_or(false)
            });

        if needs_rotate {
            self.files.remove(key);
            self.rotate(key)?;
        }

        if !self.files.contains_key(key) {
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            self.files.insert(key.to_string(), file);
        }

        let file = self.files.get_mut(key).expect("file inserted above");
        file.write_all(bytes)?;
        file.flush()
    }

    fn rotate(&self, key: &str) -> std::io::Result<()> {
        let path = self.path_for(key);
        if !path.exists() {
            return Ok(());
        }
        for index in (1..=self.keep).rev() {
            let from = if index == 1 {
                path.clone()
            } else {
                self.base_dir.join(format!("{key}.log.{}", index - 1))
            };
            let to = self.base_dir.join(format!("{key}.log.{index}"));
            if from.exists() {
                let _ = std::fs::remove_file(&to);
                std::fs::rename(&from, &to)?;
            }
        }
        Ok(())
    }

    /// Return the last `count` lines of a unit's log.
    pub fn tail(&self, unit: &str, count: usize) -> std::io::Result<Vec<String>> {
        let path = self.path_for(&sanitize(unit));
        let mut file = File::open(&path)?;
        let length = file.metadata()?.len();
        let start = length.saturating_sub(64 * 1024);
        file.seek(SeekFrom::Start(start))?;
        let mut buffer = String::new();
        file.read_to_string(&mut buffer)?;
        let mut lines: Vec<String> = buffer.lines().map(str::to_string).collect();
        if start > 0 && !lines.is_empty() {
            lines.remove(0);
        }
        let keep_from = lines.len().saturating_sub(count);
        Ok(lines.split_off(keep_from))
    }
}

fn timestamp() -> String {
    let now: DateTime<Utc> = SystemTime::now().into();
    now.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pine-log-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_and_tails_logs() {
        let dir = temp_dir("one");
        let mut logger = Logger::new(&dir, false).unwrap();
        logger.log("demo.service", Some(42), "hello");
        logger.log("demo.service", Some(42), "world");
        let lines = logger.tail("demo.service", 1).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("world"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sanitizes_unit_names() {
        assert_eq!(sanitize("a/b.service"), "a_b.service");
        assert_eq!(sanitize("ok-service_1.service"), "ok-service_1.service");
    }
}
