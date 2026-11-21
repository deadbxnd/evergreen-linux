//! Unit model, TOML parsing, drop-in merging, and unit loading.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::config::{parse_duration, Config, Paths};

/// Kind of unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    Service,
    Target,
    Mount,
    Swap,
}

/// How a service reports successful startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServiceType {
    #[default]
    Simple,
    Forking,
    Dbus,
    Notify,
    Oneshot,
    Idle,
}

impl FromStr for ServiceType {
    type Err = UnitError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "simple" => Ok(Self::Simple),
            "forking" => Ok(Self::Forking),
            "dbus" => Ok(Self::Dbus),
            "notify" => Ok(Self::Notify),
            "oneshot" => Ok(Self::Oneshot),
            "idle" => Ok(Self::Idle),
            other => Err(UnitError::Parse(format!("unknown service type {other:?}"))),
        }
    }
}

/// When a service is restarted after exiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestartPolicy {
    #[default]
    No,
    Always,
    OnSuccess,
    OnFailure,
    OnAbnormal,
    OnAbort,
}

impl FromStr for RestartPolicy {
    type Err = UnitError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "no" | "never" => Ok(Self::No),
            "always" => Ok(Self::Always),
            "on-success" => Ok(Self::OnSuccess),
            "on-failure" => Ok(Self::OnFailure),
            "on-abnormal" => Ok(Self::OnAbnormal),
            "on-abort" => Ok(Self::OnAbort),
            other => Err(UnitError::Parse(format!(
                "unknown restart policy {other:?}"
            ))),
        }
    }
}

/// Lifecycle state of a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnitState {
    #[default]
    Inactive,
    Activating,
    Active,
    Deactivating,
    Failed,
}

impl fmt::Display for UnitState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Inactive => "inactive",
            Self::Activating => "activating",
            Self::Active => "active",
            Self::Deactivating => "deactivating",
            Self::Failed => "failed",
        };
        f.write_str(text)
    }
}

/// Standard I/O destination for a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stdio {
    Null,
    Inherit,
    #[default]
    Journal,
    Tty,
}

impl FromStr for Stdio {
    type Err = UnitError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "null" => Ok(Self::Null),
            "inherit" => Ok(Self::Inherit),
            "journal" | "journal+console" => Ok(Self::Journal),
            "tty" => Ok(Self::Tty),
            other => Err(UnitError::Parse(format!("unknown stdio target {other:?}"))),
        }
    }
}

/// A `Condition*` check evaluated before a unit starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    PathExists(PathBuf),
    PathIsDirectory(PathBuf),
    PathIsSymlink(PathBuf),
    FileNotEmpty(PathBuf),
    DirectoryNotEmpty(PathBuf),
    KernelCommandLine(String),
    FirstBoot,
}

/// A parsed and validated unit configuration.
#[derive(Debug, Clone)]
pub struct UnitConfig {
    pub name: String,
    pub kind: UnitKind,
    pub description: String,
    pub service_type: ServiceType,
    pub exec_start: Option<Vec<String>>,
    pub exec_start_pre: Vec<Vec<String>>,
    pub exec_start_post: Vec<Vec<String>>,
    pub exec_stop: Vec<Vec<String>>,
    pub exec_stop_post: Vec<Vec<String>>,
    pub exec_reload: Vec<Vec<String>>,
    pub restart: RestartPolicy,
    pub restart_sec: Duration,
    pub timeout_start: Duration,
    pub timeout_stop: Duration,
    pub start_limit_interval: Duration,
    pub start_limit_burst: u32,
    pub user: Option<String>,
    pub group: Option<String>,
    pub working_directory: Option<PathBuf>,
    pub environment: Vec<(String, String)>,
    pub environment_file: Vec<PathBuf>,
    pub stdout: Stdio,
    pub stderr: Stdio,
    pub remain_after_exit: bool,
    pub kill_signal: i32,
    pub send_sigkill: bool,
    pub after: Vec<String>,
    pub before: Vec<String>,
    pub requires: Vec<String>,
    pub requisite: Vec<String>,
    pub wants: Vec<String>,
    pub binds_to: Vec<String>,
    pub part_of: Vec<String>,
    pub conflicts: Vec<String>,
    pub on_failure: Vec<String>,
    pub on_success: Vec<String>,
    pub wanted_by: Vec<String>,
    pub conditions: Vec<Condition>,
    pub memory_max: Option<u64>,
    pub cpu_quota: Option<f64>,
    pub tasks_max: Option<u64>,
    /// `[Mount] What=` — device, image, or pseudo-filesystem source.
    pub mount_what: Option<String>,
    /// `[Mount] Where=` — mount point.
    pub mount_where: Option<PathBuf>,
    /// `[Mount] Type=` — filesystem type.
    pub mount_type: Option<String>,
    /// `[Mount] Options=` — comma-separated mount options.
    pub mount_options: Vec<String>,
    /// `[Swap] What=` — swap device or file.
    pub swap_what: Option<PathBuf>,
    /// `[Swap] Priority=`.
    pub swap_priority: Option<i32>,
}

impl UnitConfig {
    pub fn is_target(&self) -> bool {
        self.kind == UnitKind::Target
    }

    pub fn is_service(&self) -> bool {
        self.kind == UnitKind::Service
    }
}

/// Runtime description of a process exit.
#[derive(Debug, Clone, Copy)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub success: bool,
    pub at: Instant,
}

/// Result of the most recent run of a unit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UnitResult {
    #[default]
    Success,
    ExitCode(i32),
    Signal(i32),
    Timeout,
    StartLimitHit,
    ConditionFailed,
    ExecFailed(String),
    Canceled,
}

impl fmt::Display for UnitResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success => f.write_str("success"),
            Self::ExitCode(code) => write!(f, "exit-code ({code})"),
            Self::Signal(sig) => write!(f, "signal ({sig})"),
            Self::Timeout => f.write_str("timeout"),
            Self::StartLimitHit => f.write_str("start-limit-hit"),
            Self::ConditionFailed => f.write_str("condition-failed"),
            Self::ExecFailed(message) => write!(f, "exec-failed ({message})"),
            Self::Canceled => f.write_str("canceled"),
        }
    }
}

/// A loaded unit with both configuration and runtime state.
#[derive(Debug)]
pub struct Unit {
    pub config: UnitConfig,
    pub state: UnitState,
    pub sub_state: String,
    pub main_pid: Option<u32>,
    pub started_at: Option<Instant>,
    pub start_deadline: Option<Instant>,
    pub stop_deadline: Option<Instant>,
    pub restart_at: Option<Instant>,
    pub failure_count: u32,
    pub window_start: Option<Instant>,
    pub last_exit: Option<ExitInfo>,
    pub result: UnitResult,
    pub manual_stop: bool,
}

impl Unit {
    pub fn new(config: UnitConfig) -> Self {
        let sub_state = "dead".to_string();
        Self {
            config,
            state: UnitState::Inactive,
            sub_state,
            main_pid: None,
            started_at: None,
            start_deadline: None,
            stop_deadline: None,
            restart_at: None,
            failure_count: 0,
            window_start: None,
            last_exit: None,
            result: UnitResult::Success,
            manual_stop: false,
        }
    }

    pub fn is_active(&self) -> bool {
        self.state == UnitState::Active
    }

    pub fn active_state(&self) -> &'static str {
        match self.state {
            UnitState::Inactive => "inactive",
            UnitState::Activating => "activating",
            UnitState::Active => "active",
            UnitState::Deactivating => "deactivating",
            UnitState::Failed => "failed",
        }
    }

    pub fn reset_failed(&mut self) {
        self.failure_count = 0;
        self.window_start = None;
        self.result = UnitResult::Success;
        if self.state == UnitState::Failed {
            self.state = UnitState::Inactive;
            self.sub_state = "dead".to_string();
        }
    }
}

// ---------------------------------------------------------------------------
// TOML representation
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum StringOrVec {
    One(String),
    Many(Vec<String>),
}

impl StringOrVec {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(value) => vec![value],
            Self::Many(values) => values,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum ExecField {
    Line(String),
    Args(Vec<String>),
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum ExecList {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum Scalar {
    Str(String),
    Float(f64),
    Int(i64),
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawUnitFile {
    #[serde(rename = "Unit")]
    unit: RawUnitSection,
    #[serde(rename = "Service")]
    service: RawServiceSection,
    #[serde(rename = "Mount")]
    mount: RawMountSection,
    #[serde(rename = "Swap")]
    swap: RawSwapSection,
    #[serde(rename = "Install")]
    install: RawInstallSection,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
struct RawUnitSection {
    description: Option<String>,
    after: Vec<String>,
    before: Vec<String>,
    requires: Vec<String>,
    requisite: Vec<String>,
    wants: Vec<String>,
    binds_to: Vec<String>,
    part_of: Vec<String>,
    conflicts: Vec<String>,
    on_failure: Vec<String>,
    on_success: Vec<String>,
    condition_path_exists: Option<StringOrVec>,
    condition_path_is_directory: Option<StringOrVec>,
    condition_path_is_symlink: Option<StringOrVec>,
    condition_file_not_empty: Option<StringOrVec>,
    condition_directory_not_empty: Option<StringOrVec>,
    condition_kernel_command_line: Option<StringOrVec>,
    condition_first_boot: Option<StringOrVec>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
struct RawServiceSection {
    #[serde(rename = "Type")]
    service_type: Option<String>,
    exec_start: Option<ExecField>,
    exec_start_pre: Option<ExecList>,
    exec_start_post: Option<ExecList>,
    exec_stop: Option<ExecList>,
    exec_stop_post: Option<ExecList>,
    exec_reload: Option<ExecList>,
    restart: Option<String>,
    restart_sec: Option<String>,
    timeout_start_sec: Option<String>,
    timeout_stop_sec: Option<String>,
    start_limit_interval_sec: Option<String>,
    start_limit_burst: Option<u32>,
    user: Option<String>,
    group: Option<String>,
    working_directory: Option<String>,
    environment: Option<StringOrVec>,
    environment_file: Option<StringOrVec>,
    standard_output: Option<String>,
    standard_error: Option<String>,
    remain_after_exit: Option<bool>,
    kill_signal: Option<String>,
    send_sigkill: Option<bool>,
    memory_max: Option<Scalar>,
    cpu_quota: Option<Scalar>,
    tasks_max: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
struct RawInstallSection {
    wanted_by: Vec<String>,
    required_by: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
struct RawMountSection {
    what: Option<String>,
    #[serde(rename = "Where")]
    where_: Option<String>,
    #[serde(rename = "Type")]
    mount_type: Option<String>,
    options: Option<StringOrVec>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "PascalCase")]
struct RawSwapSection {
    what: Option<String>,
    priority: Option<i32>,
}

impl UnitConfig {
    pub(crate) fn from_value(
        name: &str,
        value: toml::Value,
        defaults: &Config,
    ) -> Result<Self, UnitError> {
        let kind = if name.ends_with(".service") {
            UnitKind::Service
        } else if name.ends_with(".target") {
            UnitKind::Target
        } else if name.ends_with(".mount") {
            UnitKind::Mount
        } else if name.ends_with(".swap") {
            UnitKind::Swap
        } else {
            return Err(UnitError::Validation(format!(
                "unsupported unit type: {name}"
            )));
        };

        let raw: RawUnitFile = value
            .try_into()
            .map_err(|err: toml::de::Error| UnitError::Parse(err.to_string()))?;

        let exec_start = match raw.service.exec_start {
            Some(field) => Some(exec_argv(field)?),
            None => None,
        };

        if kind == UnitKind::Service && exec_start.is_none() {
            return Err(UnitError::Validation(format!(
                "{name}: service units require ExecStart"
            )));
        }
        if kind == UnitKind::Mount && raw.mount.where_.is_none() {
            return Err(UnitError::Validation(format!(
                "{name}: mount units require Where"
            )));
        }
        if kind == UnitKind::Swap && raw.swap.what.is_none() {
            return Err(UnitError::Validation(format!(
                "{name}: swap units require What"
            )));
        }

        let service_type = match raw.service.service_type {
            Some(value) => value.parse()?,
            None => ServiceType::default(),
        };

        let restart = match raw.service.restart {
            Some(value) => value.parse()?,
            None => RestartPolicy::default(),
        };

        let conditions = parse_conditions(&raw.unit);

        Ok(Self {
            name: name.to_string(),
            kind,
            description: raw.unit.description.unwrap_or_default(),
            service_type,
            exec_start,
            exec_start_pre: exec_list(raw.service.exec_start_pre)?,
            exec_start_post: exec_list(raw.service.exec_start_post)?,
            exec_stop: exec_list(raw.service.exec_stop)?,
            exec_stop_post: exec_list(raw.service.exec_stop_post)?,
            exec_reload: exec_list(raw.service.exec_reload)?,
            restart,
            restart_sec: match raw.service.restart_sec {
                Some(value) => parse_duration(&value)?,
                None => defaults.default_restart_sec,
            },
            timeout_start: match raw.service.timeout_start_sec {
                Some(value) => parse_duration(&value)?,
                None => defaults.default_timeout_start,
            },
            timeout_stop: match raw.service.timeout_stop_sec {
                Some(value) => parse_duration(&value)?,
                None => defaults.default_timeout_stop,
            },
            start_limit_interval: match raw.service.start_limit_interval_sec {
                Some(value) => parse_duration(&value)?,
                None => defaults.default_start_limit_interval,
            },
            start_limit_burst: raw
                .service
                .start_limit_burst
                .unwrap_or(defaults.default_start_limit_burst),
            user: raw.service.user,
            group: raw.service.group,
            working_directory: raw.service.working_directory.map(PathBuf::from),
            environment: parse_environment(raw.service.environment),
            environment_file: raw
                .service
                .environment_file
                .map(StringOrVec::into_vec)
                .unwrap_or_default()
                .into_iter()
                .map(PathBuf::from)
                .collect(),
            stdout: match raw.service.standard_output {
                Some(value) => value.parse()?,
                None => Stdio::default(),
            },
            stderr: match raw.service.standard_error {
                Some(value) => value.parse()?,
                None => Stdio::default(),
            },
            remain_after_exit: raw.service.remain_after_exit.unwrap_or(false),
            kill_signal: match raw.service.kill_signal {
                Some(value) => parse_signal(&value)?,
                None => libc::SIGTERM,
            },
            send_sigkill: raw.service.send_sigkill.unwrap_or(true),
            after: dedupe(raw.unit.after),
            before: dedupe(raw.unit.before),
            requires: dedupe(raw.unit.requires),
            requisite: dedupe(raw.unit.requisite),
            wants: dedupe(raw.unit.wants),
            binds_to: dedupe(raw.unit.binds_to),
            part_of: dedupe(raw.unit.part_of),
            conflicts: dedupe(raw.unit.conflicts),
            on_failure: dedupe(raw.unit.on_failure),
            on_success: dedupe(raw.unit.on_success),
            wanted_by: dedupe(raw.install.wanted_by),
            conditions,
            memory_max: raw.service.memory_max.map(parse_quantity).transpose()?,
            cpu_quota: raw.service.cpu_quota.map(parse_quota).transpose()?,
            tasks_max: raw.service.tasks_max,
            mount_what: raw.mount.what,
            mount_where: raw.mount.where_.map(PathBuf::from),
            mount_type: raw.mount.mount_type,
            mount_options: raw
                .mount
                .options
                .map(StringOrVec::into_vec)
                .unwrap_or_default()
                .into_iter()
                .flat_map(|value| {
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|part| !part.is_empty())
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .collect(),
            swap_what: raw.swap.what.map(PathBuf::from),
            swap_priority: raw.swap.priority,
        })
    }
}

fn dedupe(values: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn parse_environment(value: Option<StringOrVec>) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for entry in value.map(StringOrVec::into_vec).unwrap_or_default() {
        if let Some((key, value)) = entry.split_once('=') {
            pairs.push((key.trim().to_string(), value.to_string()));
        }
    }
    pairs
}

fn exec_argv(field: ExecField) -> Result<Vec<String>, UnitError> {
    match field {
        ExecField::Line(line) => split_command(&line),
        ExecField::Args(args) => {
            if args.is_empty() {
                Err(UnitError::Parse("empty ExecStart".to_string()))
            } else {
                Ok(args)
            }
        }
    }
}

fn exec_list(field: Option<ExecList>) -> Result<Vec<Vec<String>>, UnitError> {
    let Some(field) = field else {
        return Ok(Vec::new());
    };
    match field {
        ExecList::One(line) => Ok(vec![split_command(&line)?]),
        ExecList::Many(lines) => lines.iter().map(|line| split_command(line)).collect(),
    }
}

fn parse_conditions(section: &RawUnitSection) -> Vec<Condition> {
    let mut conditions = Vec::new();

    let mut paths = |value: &Option<StringOrVec>, build: fn(PathBuf) -> Condition| {
        if let Some(value) = value {
            for path in value.clone().into_vec() {
                conditions.push(build(PathBuf::from(path)));
            }
        }
    };

    paths(&section.condition_path_exists, Condition::PathExists);
    paths(
        &section.condition_path_is_directory,
        Condition::PathIsDirectory,
    );
    paths(&section.condition_path_is_symlink, Condition::PathIsSymlink);
    paths(&section.condition_file_not_empty, Condition::FileNotEmpty);
    paths(
        &section.condition_directory_not_empty,
        Condition::DirectoryNotEmpty,
    );

    if let Some(value) = &section.condition_kernel_command_line {
        for needle in value.clone().into_vec() {
            conditions.push(Condition::KernelCommandLine(needle));
        }
    }
    if section.condition_first_boot.is_some() {
        conditions.push(Condition::FirstBoot);
    }

    conditions
}

fn parse_quantity(value: Scalar) -> Result<u64, UnitError> {
    match value {
        Scalar::Int(value) if value >= 0 => Ok(value as u64),
        Scalar::Float(value) if value >= 0.0 => Ok(value as u64),
        Scalar::Str(value) => parse_bytes(&value),
        _ => Err(UnitError::Parse("negative memory quantity".to_string())),
    }
}

fn parse_quota(value: Scalar) -> Result<f64, UnitError> {
    match value {
        Scalar::Float(value) => Ok(value),
        Scalar::Int(value) => Ok(value as f64),
        Scalar::Str(value) => {
            let trimmed = value.trim();
            let percent = trimmed.strip_suffix('%').unwrap_or(trimmed);
            percent
                .parse::<f64>()
                .map_err(|_| UnitError::Parse(format!("invalid CPU quota {value:?}")))
        }
    }
}

/// Parse byte sizes such as `256M`, `1G`, `512K`, or `1024`.
fn parse_bytes(value: &str) -> Result<u64, UnitError> {
    let text = value.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let amount: f64 = number
        .parse()
        .map_err(|_| UnitError::Parse(format!("invalid size {value:?}")))?;
    let multiplier: f64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "K" | "KB" | "KIB" => 1024.0,
        "M" | "MB" | "MIB" => 1024.0 * 1024.0,
        "G" | "GB" | "GIB" => 1024.0 * 1024.0 * 1024.0,
        "T" | "TB" | "TIB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        other => {
            return Err(UnitError::Parse(format!(
                "unknown size unit {other:?} in {value:?}"
            )))
        }
    };
    Ok((amount * multiplier) as u64)
}

/// Parse signal names (`TERM`, `SIGTERM`, `15`).
pub fn parse_signal(value: &str) -> Result<i32, UnitError> {
    let text = value.trim();
    if let Ok(number) = text.parse::<i32>() {
        if number > 0 {
            return Ok(number);
        }
    }
    let name = text
        .strip_prefix("SIG")
        .or_else(|| text.strip_prefix("sig"))
        .unwrap_or(text)
        .to_ascii_uppercase();
    let signal = match name.as_str() {
        "HUP" => libc::SIGHUP,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "ILL" => libc::SIGILL,
        "TRAP" => libc::SIGTRAP,
        "ABRT" => libc::SIGABRT,
        "BUS" => libc::SIGBUS,
        "FPE" => libc::SIGFPE,
        "KILL" => libc::SIGKILL,
        "USR1" => libc::SIGUSR1,
        "SEGV" => libc::SIGSEGV,
        "USR2" => libc::SIGUSR2,
        "PIPE" => libc::SIGPIPE,
        "ALRM" => libc::SIGALRM,
        "TERM" => libc::SIGTERM,
        "CHLD" => libc::SIGCHLD,
        "CONT" => libc::SIGCONT,
        "STOP" => libc::SIGSTOP,
        "TSTP" => libc::SIGTSTP,
        "TTIN" => libc::SIGTTIN,
        "TTOU" => libc::SIGTTOU,
        other => return Err(UnitError::Parse(format!("unknown signal {other:?}"))),
    };
    Ok(signal)
}

/// Split a command line into argv, honouring single and double quotes and
/// backslash escapes. No shell expansion is performed.
pub fn split_command(input: &str) -> Result<Vec<String>, UnitError> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut has_content = false;

    while let Some(ch) = chars.next() {
        match ch {
            '\\' if !in_single => {
                if let Some(next) = chars.next() {
                    current.push(next);
                    has_content = true;
                }
            }
            '\'' if !in_double => {
                in_single = !in_single;
                has_content = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                has_content = true;
            }
            ch if ch.is_whitespace() && !in_single && !in_double => {
                if has_content {
                    args.push(std::mem::take(&mut current));
                    has_content = false;
                }
            }
            ch => {
                current.push(ch);
                has_content = true;
            }
        }
    }

    if in_single || in_double {
        return Err(UnitError::Parse(format!(
            "unterminated quote in command {input:?}"
        )));
    }
    if has_content {
        args.push(current);
    }
    if args.is_empty() {
        return Err(UnitError::Parse("empty command".to_string()));
    }
    Ok(args)
}

// ---------------------------------------------------------------------------
// Unit loading
// ---------------------------------------------------------------------------

/// Result of scanning the unit search path.
#[derive(Debug)]
pub struct LoadReport {
    pub units: BTreeMap<String, Unit>,
    pub warnings: Vec<String>,
}

/// Load all units from the configured search paths, applying drop-ins and
/// `.wants` / `.requires` directories.
pub fn load_units(paths: &Paths, config: &Config) -> LoadReport {
    let mut warnings = Vec::new();
    let mut raw_units: BTreeMap<String, toml::Value> = BTreeMap::new();

    for dir in &paths.unit_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !(name.ends_with(".service")
                || name.ends_with(".target")
                || name.ends_with(".mount")
                || name.ends_with(".swap"))
            {
                continue;
            }
            match read_toml(&path) {
                Ok(value) => {
                    raw_units.insert(name.to_string(), value);
                }
                Err(err) => warnings.push(format!("{}: {err}", path.display())),
            }
        }
    }

    // Apply drop-ins: <unit>.d/*.toml, in search-path then filename order.
    for (name, value) in raw_units.iter_mut() {
        for dir in &paths.unit_dirs {
            let dropin_dir = dir.join(format!("{name}.d"));
            if !dropin_dir.is_dir() {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dropin_dir) else {
                continue;
            };
            let mut files: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
                .collect();
            files.sort();
            for file in files {
                match read_toml(&file) {
                    Ok(overlay) => deep_merge(value, overlay),
                    Err(err) => warnings.push(format!("{}: {err}", file.display())),
                }
            }
        }
    }

    let mut units = BTreeMap::new();
    for (name, value) in raw_units {
        match UnitConfig::from_value(&name, value, config) {
            Ok(unit_config) => {
                units.insert(name, Unit::new(unit_config));
            }
            Err(err) => warnings.push(format!("{name}: {err}")),
        }
    }

    // Apply target .wants / .requires directories.
    for dir in &paths.unit_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let (target, list) = if let Some(target) = name.strip_suffix(".wants") {
                (target, true)
            } else if let Some(target) = name.strip_suffix(".requires") {
                (target, false)
            } else {
                continue;
            };
            let Ok(children) = std::fs::read_dir(&path) else {
                continue;
            };
            let mut child_names: Vec<String> = children
                .flatten()
                .filter_map(|child| child.file_name().to_str().map(str::to_string))
                .filter(|child_name| units.contains_key(child_name))
                .collect();

            let Some(target_unit) = units.get_mut(target) else {
                continue;
            };
            let list_ref = if list {
                &mut target_unit.config.wants
            } else {
                &mut target_unit.config.requires
            };
            for child_name in child_names.drain(..) {
                if !list_ref.contains(&child_name) {
                    list_ref.push(child_name);
                }
            }
        }
    }

    LoadReport { units, warnings }
}

fn read_toml(path: &Path) -> Result<toml::Value, UnitError> {
    let text = std::fs::read_to_string(path)?;
    toml::from_str(&text).map_err(|err| UnitError::Parse(err.to_string()))
}

fn deep_merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base_map), toml::Value::Table(overlay_map)) => {
            for (key, value) in overlay_map {
                if let Some(existing) = base_map.get_mut(&key) {
                    deep_merge(existing, value);
                } else {
                    base_map.insert(key, value);
                }
            }
        }
        (toml::Value::Array(base_array), toml::Value::Array(overlay_array)) => {
            base_array.extend(overlay_array);
        }
        (base_slot, overlay) => {
            *base_slot = overlay;
        }
    }
}

/// Errors produced while loading units.
#[derive(Debug)]
pub enum UnitError {
    Io(std::io::Error),
    Parse(String),
    Validation(String),
}

impl fmt::Display for UnitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Parse(err) => write!(f, "parse error: {err}"),
            Self::Validation(err) => write!(f, "validation error: {err}"),
        }
    }
}

impl std::error::Error for UnitError {}

impl From<std::io::Error> for UnitError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<anyhow::Error> for UnitError {
    fn from(err: anyhow::Error) -> Self {
        Self::Validation(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_for(name: &str, body: &str) -> Result<UnitConfig, UnitError> {
        let value: toml::Value = toml::from_str(body).unwrap();
        UnitConfig::from_value(name, value, &Config::default())
    }

    #[test]
    fn splits_quoted_command() {
        let args = split_command("/bin/sh -c 'echo hello world'").unwrap();
        assert_eq!(args, vec!["/bin/sh", "-c", "echo hello world"]);
    }

    #[test]
    fn splits_escaped_spaces() {
        let args = split_command("/bin/echo hello\\ world").unwrap();
        assert_eq!(args, vec!["/bin/echo", "hello world"]);
    }

    #[test]
    fn parses_service_unit() {
        let unit = config_for(
            "demo.service",
            r#"
[Unit]
Description = "Demo"
After = ["network.target"]

[Service]
Type = "simple"
ExecStart = "/bin/sleep 5"
Restart = "on-failure"
RestartSec = "250ms"

[Install]
WantedBy = ["multi-user.target"]
"#,
        )
        .unwrap();

        assert_eq!(unit.description, "Demo");
        assert_eq!(unit.service_type, ServiceType::Simple);
        assert_eq!(unit.restart, RestartPolicy::OnFailure);
        assert_eq!(unit.restart_sec, Duration::from_millis(250));
        assert_eq!(unit.exec_start.unwrap(), vec!["/bin/sleep", "5"]);
        assert_eq!(unit.after, vec!["network.target"]);
        assert_eq!(unit.wanted_by, vec!["multi-user.target"]);
    }

    #[test]
    fn rejects_service_without_exec() {
        let result = config_for(
            "broken.service",
            r#"
[Service]
Type = "simple"
"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn parses_signals() {
        assert_eq!(parse_signal("TERM").unwrap(), libc::SIGTERM);
        assert_eq!(parse_signal("SIGKILL").unwrap(), libc::SIGKILL);
        assert_eq!(parse_signal("15").unwrap(), 15);
        assert!(parse_signal("NOPE").is_err());
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_bytes("256M").unwrap(), 256 * 1024 * 1024);
        assert_eq!(parse_bytes("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_bytes("1024").unwrap(), 1024);
    }

    #[test]
    fn parses_mount_and_swap_units() {
        let mount = config_for(
            "data.mount",
            r#"
[Mount]
What = "/dev/vdb1"
Where = "/mnt/data"
Type = "btrfs"
Options = "compress=zstd,noatime"
"#,
        )
        .unwrap();
        assert_eq!(mount.kind, UnitKind::Mount);
        assert_eq!(mount.mount_what.as_deref(), Some("/dev/vdb1"));
        assert_eq!(mount.mount_where, Some(PathBuf::from("/mnt/data")));
        assert_eq!(mount.mount_type.as_deref(), Some("btrfs"));
        assert_eq!(mount.mount_options, vec!["compress=zstd", "noatime"]);

        let swap = config_for(
            "swapfile.swap",
            r#"
[Swap]
What = "/swap/swapfile"
Priority = 10
"#,
        )
        .unwrap();
        assert_eq!(swap.kind, UnitKind::Swap);
        assert_eq!(swap.swap_what, Some(PathBuf::from("/swap/swapfile")));
        assert_eq!(swap.swap_priority, Some(10));
    }

    #[test]
    fn rejects_incomplete_mount_and_swap() {
        assert!(config_for("bad.mount", "[Mount]\nWhat = \"/dev/x\"\n").is_err());
        assert!(config_for("bad.swap", "[Swap]\nPriority = 1\n").is_err());
    }

    #[test]
    fn deep_merge_concatenates_arrays() {
        let mut base: toml::Value = toml::from_str(
            r#"
[Unit]
Wants = ["a.service"]
"#,
        )
        .unwrap();
        let overlay: toml::Value = toml::from_str(
            r#"
[Unit]
Wants = ["b.service"]
Description = "overlay"
"#,
        )
        .unwrap();
        deep_merge(&mut base, overlay);
        let parsed: RawUnitFile = base.try_into().unwrap();
        assert_eq!(parsed.unit.wants, vec!["a.service", "b.service"]);
    }
}
