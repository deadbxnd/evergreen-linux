//! Global configuration (`/etc/pine/pine.conf`) and filesystem locations.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// Manager-wide defaults.
#[derive(Debug, Clone)]
pub struct Config {
    pub default_timeout_start: Duration,
    pub default_timeout_stop: Duration,
    pub default_restart_sec: Duration,
    pub default_start_limit_interval: Duration,
    pub default_start_limit_burst: u32,
    pub log_level: String,
    pub log_target: String,
    pub runtime_watchdog_sec: Duration,
    pub shutdown_watchdog_sec: Duration,
    pub default_target: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_timeout_start: Duration::from_secs(90),
            default_timeout_stop: Duration::from_secs(90),
            default_restart_sec: Duration::from_millis(100),
            default_start_limit_interval: Duration::from_secs(10),
            default_start_limit_burst: 5,
            log_level: "info".to_string(),
            log_target: "journal".to_string(),
            runtime_watchdog_sec: Duration::ZERO,
            shutdown_watchdog_sec: Duration::from_secs(600),
            default_target: "graphical.target".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawConfig {
    #[serde(rename = "Manager")]
    manager: RawManager,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
struct RawManager {
    default_timeout_start_sec: Option<String>,
    default_timeout_stop_sec: Option<String>,
    default_restart_sec: Option<String>,
    default_start_limit_interval_sec: Option<String>,
    default_start_limit_burst: Option<u32>,
    log_level: Option<String>,
    log_target: Option<String>,
    runtime_watchdog_sec: Option<String>,
    shutdown_watchdog_sec: Option<String>,
    default_target: Option<String>,
}

impl Config {
    /// Load configuration from `path`, falling back to defaults when absent.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        let raw: RawConfig = toml::from_str(&text)?;
        let mut config = Self::default();

        if let Some(value) = raw.manager.default_timeout_start_sec {
            config.default_timeout_start = parse_duration(&value)?;
        }
        if let Some(value) = raw.manager.default_timeout_stop_sec {
            config.default_timeout_stop = parse_duration(&value)?;
        }
        if let Some(value) = raw.manager.default_restart_sec {
            config.default_restart_sec = parse_duration(&value)?;
        }
        if let Some(value) = raw.manager.default_start_limit_interval_sec {
            config.default_start_limit_interval = parse_duration(&value)?;
        }
        if let Some(value) = raw.manager.default_start_limit_burst {
            config.default_start_limit_burst = value;
        }
        if let Some(value) = raw.manager.log_level {
            config.log_level = value;
        }
        if let Some(value) = raw.manager.log_target {
            config.log_target = value;
        }
        if let Some(value) = raw.manager.runtime_watchdog_sec {
            config.runtime_watchdog_sec = parse_duration(&value)?;
        }
        if let Some(value) = raw.manager.shutdown_watchdog_sec {
            config.shutdown_watchdog_sec = parse_duration(&value)?;
        }
        if let Some(value) = raw.manager.default_target {
            config.default_target = value;
        }

        Ok(config)
    }
}

/// Parse durations such as `90s`, `100ms`, `10min`, `1h`, or `infinity`.
pub fn parse_duration(input: &str) -> anyhow::Result<Duration> {
    let text = input.trim();
    if text.is_empty() {
        anyhow::bail!("empty duration");
    }
    if text.eq_ignore_ascii_case("infinity") {
        return Ok(Duration::MAX);
    }

    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);

    if number.is_empty() {
        anyhow::bail!("duration {text:?} has no numeric value");
    }

    let value: f64 = number
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration number in {text:?}"))?;

    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
        "ms" | "msec" | "msecs" | "millisecond" | "milliseconds" => 0.001,
        "us" | "usec" | "usecs" | "microsecond" | "microseconds" => 0.000_001,
        "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
        "d" | "day" | "days" => 86_400.0,
        other => anyhow::bail!("unknown duration unit {other:?} in {text:?}"),
    };

    let seconds = value * multiplier;
    if !seconds.is_finite() || seconds < 0.0 {
        anyhow::bail!("invalid duration {text:?}");
    }
    Ok(Duration::from_secs_f64(seconds))
}

/// Filesystem locations used by pine.
///
/// `root` is `/` on a real system and a temporary directory in tests.
#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
    pub unit_dirs: Vec<PathBuf>,
    pub config_file: PathBuf,
    pub log_dir: PathBuf,
    pub run_dir: PathBuf,
    pub control_socket: PathBuf,
    pub cgroup_root: PathBuf,
}

impl Paths {
    pub fn system() -> Self {
        Self::for_root("/")
    }

    pub fn for_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let join = |path: &str| root.join(path.trim_start_matches('/'));
        let cgroup_root = if root == Path::new("/") {
            PathBuf::from("/sys/fs/cgroup/pine")
        } else {
            join("sys/fs/cgroup/pine")
        };

        Self {
            unit_dirs: vec![
                join("usr/lib/pine/services"),
                join("usr/local/lib/pine/services"),
                join("run/pine/services"),
                join("etc/pine/services"),
            ],
            config_file: join("etc/pine/pine.conf"),
            log_dir: join("var/log/pine"),
            run_dir: join("run/pine"),
            control_socket: join("run/pine/control.sock"),
            cgroup_root,
            root,
        }
    }

    /// Resolve a possibly-relative path against the configured root.
    pub fn resolve(&self, path: impl AsRef<Path>) -> PathBuf {
        let path = path.as_ref();
        if path.is_absolute() && self.root == Path::new("/") {
            path.to_path_buf()
        } else if path.is_absolute() {
            self.root.join(path.strip_prefix("/").unwrap_or(path))
        } else {
            self.root.join(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_durations() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("100ms").unwrap(), Duration::from_millis(100));
        assert_eq!(parse_duration("10min").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("2").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("infinity").unwrap(), Duration::MAX);
    }

    #[test]
    fn rejects_bad_durations() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("10fortnights").is_err());
    }

    #[test]
    fn paths_resolve_under_root() {
        let paths = Paths::for_root("/tmp/pine-test");
        assert_eq!(
            paths.resolve("/etc/pine/pine.conf"),
            PathBuf::from("/tmp/pine-test/etc/pine/pine.conf")
        );
    }
}
