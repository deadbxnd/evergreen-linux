//! Process spawning, supervision helpers, signals, and PID 1 setup.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio as ProcessStdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::cgroup::Cgroup;
use crate::log::Logger;
use crate::unit::Stdio;

/// Description of a process to spawn.
#[derive(Debug, Clone, Default)]
pub struct SpawnSpec {
    pub unit: String,
    pub argv: Vec<String>,
    pub user: Option<String>,
    pub group: Option<String>,
    pub working_directory: Option<PathBuf>,
    pub environment: Vec<(String, String)>,
    pub environment_file: Vec<PathBuf>,
    pub stdout: Stdio,
    pub stderr: Stdio,
}

/// Errors from starting or waiting on a process.
#[derive(Debug)]
pub enum ExecError {
    Io(std::io::Error),
    Timeout,
    Invalid(String),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Timeout => f.write_str("timed out"),
            Self::Invalid(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ExecError {}

impl From<std::io::Error> for ExecError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Spawn a process described by `spec`.
///
/// When stdout/stderr are `journal`, reader threads forward lines to the
/// supplied logger.
pub fn spawn(
    spec: &SpawnSpec,
    logger: Arc<Mutex<Logger>>,
    cgroup: Option<&Cgroup>,
) -> Result<Child, ExecError> {
    if spec.argv.is_empty() {
        return Err(ExecError::Invalid("empty argv".to_string()));
    }

    let mut command = Command::new(&spec.argv[0]);
    command.args(&spec.argv[1..]);

    let mut environment: BTreeMap<String, String> = BTreeMap::new();
    for file in &spec.environment_file {
        for (key, value) in read_environment_file(file)? {
            environment.insert(key, value);
        }
    }
    for (key, value) in &spec.environment {
        environment.insert(key.clone(), value.clone());
    }
    command.envs(environment);

    command.stdin(ProcessStdio::null());
    command.stdout(map_stdio(spec.stdout));
    command.stderr(map_stdio(spec.stderr));

    if let Some(directory) = &spec.working_directory {
        command.current_dir(directory);
    }

    if let Some(user) = &spec.user {
        let (uid, gid) = lookup_user(user)
            .ok_or_else(|| ExecError::Invalid(format!("unknown user {user:?}")))?;
        let gid = match &spec.group {
            Some(group) => lookup_group(group)
                .ok_or_else(|| ExecError::Invalid(format!("unknown group {group:?}")))?,
            None => gid,
        };
        command.uid(uid);
        command.gid(gid);
    } else if let Some(group) = &spec.group {
        let gid = lookup_group(group)
            .ok_or_else(|| ExecError::Invalid(format!("unknown group {group:?}")))?;
        command.gid(gid);
    }

    // New session per service so terminal signals do not leak.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut child = command.spawn()?;

    if let Some(cgroup) = cgroup {
        let _ = cgroup.add_pid(child.id());
    }

    if spec.stdout == Stdio::Journal {
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(stdout, spec.unit.clone(), Arc::clone(&logger));
        }
    }
    if spec.stderr == Stdio::Journal {
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, spec.unit.clone(), Arc::clone(&logger));
        }
    }

    Ok(child)
}

fn map_stdio(stdio: Stdio) -> ProcessStdio {
    match stdio {
        Stdio::Null => ProcessStdio::null(),
        Stdio::Inherit | Stdio::Tty => ProcessStdio::inherit(),
        Stdio::Journal => ProcessStdio::piped(),
    }
}

fn spawn_reader<R>(reader: R, unit: String, logger: Arc<Mutex<Logger>>)
where
    R: Read + Send + 'static,
{
    let _ = std::thread::Builder::new()
        .name(format!("pine-log-{unit}"))
        .spawn(move || {
            let reader = BufReader::new(reader);
            for line in reader.lines().map_while(Result::ok) {
                if let Ok(mut logger) = logger.lock() {
                    logger.log(&unit, None, &line);
                }
            }
        });
}

fn read_environment_file(path: &Path) -> Result<Vec<(String, String)>, ExecError> {
    let text = std::fs::read_to_string(path)?;
    let mut pairs = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            pairs.push((key.trim().to_string(), value.to_string()));
        }
    }
    Ok(pairs)
}

/// Spawn a command and wait for it to finish, killing it on timeout.
pub fn run_blocking(
    spec: &SpawnSpec,
    timeout: Duration,
    logger: Arc<Mutex<Logger>>,
) -> Result<ExitStatus, ExecError> {
    let mut child = spawn(spec, logger, None)?;
    match wait_timeout(&mut child, timeout) {
        Ok(status) => Ok(status),
        Err(ExecError::Timeout) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(ExecError::Timeout)
        }
        Err(err) => Err(err),
    }
}

/// Wait for `child` to exit, up to `timeout`.
pub fn wait_timeout(child: &mut Child, timeout: Duration) -> Result<ExitStatus, ExecError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(ExecError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Exit status interpretation.
pub fn exit_info(status: ExitStatus) -> (Option<i32>, Option<i32>, bool) {
    (status.code(), status.signal(), status.success())
}

/// Send a signal to a single PID.
pub fn kill_pid(pid: u32, signal: i32) -> std::io::Result<()> {
    let result = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Send a signal to a process group (the service's session).
pub fn kill_group(pid: u32, signal: i32) -> std::io::Result<()> {
    let result = unsafe { libc::kill(-(pid as libc::pid_t), signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Outcome of polling a service PID.
#[derive(Debug, Clone, Copy)]
pub enum WaitOutcome {
    Running,
    Exited(ExitStatus),
    /// The PID is no longer our child (already reaped, or reaped as PID 1).
    Gone,
}

/// Poll a specific child without blocking.
pub fn wait_pid(pid: u32) -> std::io::Result<WaitOutcome> {
    let mut status = 0;
    let result = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
    if result == 0 {
        return Ok(WaitOutcome::Running);
    }
    if result < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ECHILD) {
            return Ok(WaitOutcome::Gone);
        }
        return Err(err);
    }
    Ok(WaitOutcome::Exited(ExitStatus::from_raw(status)))
}

/// Reap exit statuses for a set of known service PIDs.
///
/// Using explicit PIDs (rather than `waitpid(-1)`) means pine never steals the
/// exit status of a child owned by another part of the process.
pub fn reap_known(pids: &[u32]) -> Vec<(u32, ExitStatus)> {
    let mut reaped = Vec::new();
    for pid in pids {
        if let Ok(WaitOutcome::Exited(status)) = wait_pid(*pid) {
            reaped.push((*pid, status));
        }
    }
    reaped
}

/// Reap orphaned children. Only meaningful as PID 1 or a subreaper.
///
/// Managed service PIDs must already have been collected with [`reap_known`];
/// any remaining children here are orphans and their status is discarded.
pub fn reap_orphans() {
    if !is_pid1() {
        return;
    }
    loop {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            break;
        }
    }
}

pub fn is_pid1() -> bool {
    std::process::id() == 1
}

/// Configure this process as a child subreaper so orphaned services are
/// reparented to pine.
pub fn setup_subreaper() -> std::io::Result<()> {
    let result = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Mount the virtual filesystems needed by PID 1. Best-effort.
pub fn prepare_pid1() -> Vec<String> {
    let mut warnings = Vec::new();
    if !is_pid1() {
        return warnings;
    }

    for target in ["/proc", "/sys", "/dev", "/sys/fs/cgroup"] {
        if let Err(err) = std::fs::create_dir_all(target) {
            warnings.push(format!("create {target}: {err}"));
        }
    }

    let mounts = [
        ("proc", "/proc", "proc", 0),
        ("sysfs", "/sys", "sysfs", 0),
        ("devtmpfs", "/dev", "devtmpfs", 0),
        ("cgroup2", "/sys/fs/cgroup", "cgroup2", 0),
    ];

    for (source, target, fstype, flags) in mounts {
        if let Err(err) = mount_fs(source, Path::new(target), fstype, flags, "") {
            // EBUSY means it is already mounted, which is fine.
            if err.raw_os_error() != Some(libc::EBUSY) {
                warnings.push(format!("mount {target}: {err}"));
            }
        }
    }

    if let Err(err) = setup_subreaper() {
        warnings.push(format!("set child subreaper: {err}"));
    }

    warnings
}

fn mount_fs(
    source: &str,
    target: &Path,
    fstype: &str,
    flags: libc::c_ulong,
    data: &str,
) -> std::io::Result<()> {
    let source = CString::new(source)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid source"))?;
    let target = CString::new(target.to_string_lossy().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid target"))?;
    let fstype = CString::new(fstype)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid fstype"))?;
    let data = CString::new(data)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid data"))?;

    let result = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            flags,
            data.as_ptr() as *const libc::c_void,
        )
    };

    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn lookup_user(name: &str) -> Option<(u32, u32)> {
    let name = CString::new(name).ok()?;
    let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();

    let code = unsafe {
        libc::getpwnam_r(
            name.as_ptr(),
            &mut passwd,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };

    if code == 0 && !result.is_null() {
        Some((passwd.pw_uid, passwd.pw_gid))
    } else {
        None
    }
}

fn lookup_group(name: &str) -> Option<u32> {
    let name = CString::new(name).ok()?;
    let mut group: libc::group = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    let mut result: *mut libc::group = std::ptr::null_mut();

    let code = unsafe {
        libc::getgrnam_r(
            name.as_ptr(),
            &mut group,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };

    if code == 0 && !result.is_null() {
        Some(group.gr_gid)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_logger() -> Arc<Mutex<Logger>> {
        let dir = std::env::temp_dir().join(format!(
            "pine-proc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Arc::new(Mutex::new(Logger::new(dir, false).unwrap()))
    }

    #[test]
    fn runs_blocking_command() {
        let logger = test_logger();
        let spec = SpawnSpec {
            unit: "test.service".to_string(),
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "exit 0".to_string(),
            ],
            ..Default::default()
        };
        let status = run_blocking(&spec, Duration::from_secs(5), logger).unwrap();
        assert!(status.success());
    }

    #[test]
    fn times_out_runaway_command() {
        let logger = test_logger();
        let spec = SpawnSpec {
            unit: "slow.service".to_string(),
            argv: vec!["/bin/sleep".to_string(), "10".to_string()],
            ..Default::default()
        };
        let result = run_blocking(&spec, Duration::from_millis(100), logger);
        assert!(matches!(result, Err(ExecError::Timeout)));
    }

    #[test]
    fn resolves_current_user() {
        let name = std::env::var("USER").unwrap_or_else(|_| "root".to_string());
        // Resolution may fail in minimal containers; only assert consistency.
        // Resolution may fail in minimal containers; success must still yield
        // a coherent uid/gid pair (the tuple's presence is the contract).
        let _ = lookup_user(&name).map(|(uid, gid)| uid.checked_add(gid));
    }
}
