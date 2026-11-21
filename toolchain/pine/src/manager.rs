//! Service manager: job execution, supervision, targets, and the control API.

use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cgroup::{Cgroup, Limits};
use crate::config::{Config, Paths};
use crate::graph;
use crate::log::Logger;
use crate::mount;
use crate::process::{self, SpawnSpec, WaitOutcome};
use crate::shutdown::{self, ShutdownAction};
use crate::unit::{
    Condition, ExitInfo, RestartPolicy, ServiceType, Unit, UnitConfig, UnitKind, UnitResult,
    UnitState,
};

/// A snapshot of a unit's state, suitable for the control protocol.
#[derive(Debug, Clone, Serialize)]
pub struct UnitStatus {
    pub name: String,
    pub description: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    pub main_pid: Option<u32>,
    pub result: String,
}

/// Reply to a control command.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Message { text: String },
    Unit { unit: UnitStatus },
    Units { units: Vec<UnitStatus> },
    Lines { lines: Vec<String> },
    Error { message: String },
}

impl Response {
    pub fn ok() -> Self {
        Self::Ok
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
        }
    }
}

/// Commands accepted by the manager event loop.
pub enum Command {
    Start {
        name: String,
        reply: Sender<Response>,
    },
    Stop {
        name: String,
        reply: Sender<Response>,
    },
    Restart {
        name: String,
        reply: Sender<Response>,
    },
    Reload {
        name: String,
        reply: Sender<Response>,
    },
    Status {
        name: String,
        reply: Sender<Response>,
    },
    List {
        reply: Sender<Response>,
    },
    Log {
        name: String,
        lines: usize,
        reply: Sender<Response>,
    },
    ResetFailed {
        name: Option<String>,
        reply: Sender<Response>,
    },
    DaemonReload {
        reply: Sender<Response>,
    },
    Shutdown {
        action: ShutdownAction,
        reply: Sender<Response>,
    },
    Signal(i32),
    Terminate,
}

/// Cloneable client handle for talking to a running manager.
#[derive(Clone)]
pub struct ManagerHandle {
    tx: Sender<Command>,
}

impl ManagerHandle {
    pub fn new(tx: Sender<Command>) -> Self {
        Self { tx }
    }

    fn call<F>(&self, build: F) -> Response
    where
        F: FnOnce(Sender<Response>) -> Command,
    {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self.tx.send(build(reply_tx)).is_err() {
            return Response::error("manager is not running");
        }
        reply_rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| Response::error("manager did not reply"))
    }

    pub fn start(&self, name: impl Into<String>) -> Response {
        let name = name.into();
        self.call(|reply| Command::Start { name, reply })
    }

    pub fn stop(&self, name: impl Into<String>) -> Response {
        let name = name.into();
        self.call(|reply| Command::Stop { name, reply })
    }

    pub fn restart(&self, name: impl Into<String>) -> Response {
        let name = name.into();
        self.call(|reply| Command::Restart { name, reply })
    }

    pub fn reload(&self, name: impl Into<String>) -> Response {
        let name = name.into();
        self.call(|reply| Command::Reload { name, reply })
    }

    pub fn status(&self, name: impl Into<String>) -> Response {
        let name = name.into();
        self.call(|reply| Command::Status { name, reply })
    }

    pub fn list(&self) -> Response {
        self.call(|reply| Command::List { reply })
    }

    pub fn log(&self, name: impl Into<String>, lines: usize) -> Response {
        let name = name.into();
        self.call(|reply| Command::Log { name, lines, reply })
    }

    pub fn reset_failed(&self, name: Option<String>) -> Response {
        self.call(|reply| Command::ResetFailed { name, reply })
    }

    pub fn daemon_reload(&self) -> Response {
        self.call(|reply| Command::DaemonReload { reply })
    }

    pub fn shutdown(&self, action: ShutdownAction) -> Response {
        self.call(|reply| Command::Shutdown { action, reply })
    }

    pub fn signal(&self, signal: i32) {
        let _ = self.tx.send(Command::Signal(signal));
    }

    pub fn terminate(&self) {
        let _ = self.tx.send(Command::Terminate);
    }
}

/// The service manager.
pub struct Manager {
    pub paths: Paths,
    pub config: Config,
    pub units: BTreeMap<String, Unit>,
    logger: Arc<Mutex<Logger>>,
    test_mode: bool,
    shutdown: Option<ShutdownAction>,
    tick_interval: Duration,
    hooks_running: bool,
}

impl Manager {
    pub fn new(paths: Paths, config: Config, logger: Arc<Mutex<Logger>>, test_mode: bool) -> Self {
        let report = crate::unit::load_units(&paths, &config);
        if let Ok(mut guard) = logger.lock() {
            for warning in &report.warnings {
                guard.global("warn", warning);
            }
        }

        Self {
            paths,
            config,
            units: report.units,
            logger,
            test_mode,
            shutdown: None,
            tick_interval: Duration::from_millis(100),
            hooks_running: false,
        }
    }

    /// Run the manager until terminated or shutdown completes.
    pub fn run(&mut self, rx: Receiver<Command>, target: &str) {
        if let Err(err) = self.boot(target) {
            self.log_global("err", &format!("boot failed: {err}"));
        }

        let mut last_tick = Instant::now();
        loop {
            if self.shutdown.is_some() {
                break;
            }

            let timeout = self.tick_interval.saturating_sub(last_tick.elapsed());
            match rx.recv_timeout(timeout) {
                Ok(Command::Terminate) => break,
                Ok(command) => {
                    if self.handle_command(command) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            if last_tick.elapsed() >= self.tick_interval {
                self.tick();
                last_tick = Instant::now();
            }
        }
    }

    fn handle_command(&mut self, command: Command) -> bool {
        match command {
            Command::Start { name, reply } => {
                let result = self.start_unit(&name, true);
                let _ = reply.send(match result {
                    Ok(()) => Response::ok(),
                    Err(err) => Response::error(err),
                });
            }
            Command::Stop { name, reply } => {
                let result = self.stop_unit(&name, true);
                let _ = reply.send(match result {
                    Ok(()) => Response::ok(),
                    Err(err) => Response::error(err),
                });
            }
            Command::Restart { name, reply } => {
                let result = self.restart_unit(&name);
                let _ = reply.send(match result {
                    Ok(()) => Response::ok(),
                    Err(err) => Response::error(err),
                });
            }
            Command::Reload { name, reply } => {
                let result = self.reload_unit(&name);
                let _ = reply.send(match result {
                    Ok(()) => Response::ok(),
                    Err(err) => Response::error(err),
                });
            }
            Command::Status { name, reply } => {
                let response = match self.status(&name) {
                    Some(status) => Response::Unit { unit: status },
                    None => Response::error(format!("unknown unit: {name}")),
                };
                let _ = reply.send(response);
            }
            Command::List { reply } => {
                let _ = reply.send(Response::Units { units: self.list() });
            }
            Command::Log { name, lines, reply } => {
                let response = match self.logger.lock() {
                    Ok(logger) => match logger.tail(&name, lines) {
                        Ok(lines) => Response::Lines { lines },
                        Err(err) => Response::error(format!("cannot read log: {err}")),
                    },
                    Err(_) => Response::error("log lock poisoned"),
                };
                let _ = reply.send(response);
            }
            Command::ResetFailed { name, reply } => match name {
                Some(name) => {
                    if let Some(unit) = self.units.get_mut(&name) {
                        unit.reset_failed();
                        let _ = reply.send(Response::ok());
                    } else {
                        let _ = reply.send(Response::error(format!("unknown unit: {name}")));
                    }
                }
                None => {
                    for unit in self.units.values_mut() {
                        unit.reset_failed();
                    }
                    let _ = reply.send(Response::ok());
                }
            },
            Command::DaemonReload { reply } => {
                self.reload_units();
                let _ = reply.send(Response::ok());
            }
            Command::Shutdown { action, reply } => {
                self.request_shutdown(action);
                let _ = reply.send(Response::ok());
                return true;
            }
            Command::Signal(signal) => self.handle_signal(signal),
            Command::Terminate => return true,
        }
        false
    }

    /// Boot toward `target`.
    pub fn boot(&mut self, target: &str) -> Result<(), String> {
        let order = graph::transaction(&self.units, target).map_err(|err| err.to_string())?;
        self.log_global("info", &format!("booting to {target}"));

        let mut errors = Vec::new();
        for name in order {
            let is_target = self
                .units
                .get(&name)
                .map(|unit| unit.config.is_target())
                .unwrap_or(false);

            if is_target {
                let _ = self.start_unit(&name, false);
            } else if let Err(err) = self.start_unit(&name, false) {
                errors.push(format!("{name}: {err}"));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    /// Start a unit.
    pub fn start_unit(&mut self, name: &str, manual: bool) -> Result<(), String> {
        let config = match self.units.get(name) {
            Some(unit) => unit.config.clone(),
            None => return Err(format!("unknown unit: {name}")),
        };

        match self.units.get(name).map(|unit| unit.state) {
            Some(UnitState::Active) | Some(UnitState::Activating) => return Ok(()),
            _ => {}
        }

        self.stop_conflicts(&config);
        if let Err(reason) = self.check_requirements(&config) {
            self.mark_failed(name, UnitResult::ExecFailed(reason.clone()));
            self.trigger_hooks(name, false);
            return Err(reason);
        }

        if config.is_target() {
            let unit = self.units.get_mut(name).expect("unit exists");
            unit.state = UnitState::Active;
            unit.sub_state = "active".to_string();
            unit.started_at = Some(Instant::now());
            unit.result = UnitResult::Success;
            unit.restart_at = None;
            self.log(name, None, "reached target");
            return Ok(());
        }

        if !config.conditions.is_empty() {
            let all_ok = config
                .conditions
                .iter()
                .all(|condition| self.condition_ok(condition));
            if !all_ok {
                let unit = self.units.get_mut(name).expect("unit exists");
                unit.state = UnitState::Inactive;
                unit.sub_state = "condition-failed".to_string();
                unit.result = UnitResult::ConditionFailed;
                self.log(name, None, "conditions not met; skipping");
                return Ok(());
            }
        }

        {
            let unit = self.units.get_mut(name).expect("unit exists");
            unit.state = UnitState::Activating;
            unit.sub_state = "start-pre".to_string();
            unit.result = UnitResult::Success;
            unit.manual_stop = false;
            unit.restart_at = None;
            if manual {
                unit.failure_count = 0;
                unit.window_start = None;
            }
        }

        match config.kind {
            UnitKind::Mount => return self.start_mount(&config),
            UnitKind::Swap => return self.start_swap(&config),
            UnitKind::Target | UnitKind::Service => {}
        }

        // ExecStartPre
        for argv in &config.exec_start_pre {
            let spec = self.spawn_spec(&config, argv.clone());
            match process::run_blocking(&spec, config.timeout_start, self.logger.clone()) {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    let message = format!("ExecStartPre exited with {status}");
                    self.mark_failed(name, UnitResult::ExecFailed(message.clone()));
                    self.schedule_restart(name, false, status.signal());
                    return Err(message);
                }
                Err(err) => {
                    let message = format!("ExecStartPre failed: {err}");
                    self.mark_failed(name, UnitResult::ExecFailed(message.clone()));
                    self.schedule_restart(name, false, None);
                    return Err(message);
                }
            }
        }

        self.log(name, None, "starting");

        let result = match config.service_type {
            ServiceType::Oneshot => {
                let spec = self.spawn_spec(&config, config.exec_start.clone().unwrap_or_default());
                match process::run_blocking(&spec, config.timeout_start, self.logger.clone()) {
                    Ok(status) if status.success() => {
                        let remain = config.remain_after_exit;
                        {
                            let unit = self.units.get_mut(name).expect("unit exists");
                            unit.state = if remain {
                                UnitState::Active
                            } else {
                                UnitState::Inactive
                            };
                            unit.sub_state = if remain {
                                "exited".to_string()
                            } else {
                                "dead".to_string()
                            };
                            unit.started_at = Some(Instant::now());
                            unit.result = UnitResult::Success;
                        }
                        self.log(name, None, "finished");
                        if !remain {
                            self.trigger_hooks(name, true);
                        }
                        self.schedule_restart(name, true, None);
                        Ok(())
                    }
                    Ok(status) => {
                        let message = format!("ExecStart exited with {status}");
                        self.mark_failed(name, UnitResult::ExecFailed(message.clone()));
                        self.schedule_restart(name, false, status.signal());
                        Err(message)
                    }
                    Err(err) => {
                        let message = format!("ExecStart failed: {err}");
                        self.mark_failed(name, UnitResult::ExecFailed(message.clone()));
                        self.schedule_restart(name, false, None);
                        Err(message)
                    }
                }
            }
            _ => self.start_process(name, &config),
        };

        result?;

        // ExecStartPost
        for argv in &config.exec_start_post {
            let spec = self.spawn_spec(&config, argv.clone());
            match process::run_blocking(&spec, config.timeout_start, self.logger.clone()) {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    let message = format!("ExecStartPost exited with {status}");
                    let _ = self.stop_unit(name, true);
                    self.mark_failed(name, UnitResult::ExecFailed(message.clone()));
                    self.schedule_restart(name, false, status.signal());
                    return Err(message);
                }
                Err(err) => {
                    let message = format!("ExecStartPost failed: {err}");
                    let _ = self.stop_unit(name, true);
                    self.mark_failed(name, UnitResult::ExecFailed(message.clone()));
                    self.schedule_restart(name, false, None);
                    return Err(message);
                }
            }
        }

        Ok(())
    }

    fn start_process(
        &mut self,
        name: &str,
        config: &crate::unit::UnitConfig,
    ) -> Result<(), String> {
        let cgroup = Cgroup::new(self.paths.cgroup_root.clone(), name);
        if let Err(err) = cgroup.create() {
            self.log_global("debug", &format!("cgroup unavailable for {name}: {err}"));
        }
        let _ = cgroup.set_limits(&Limits {
            memory_max: config.memory_max,
            cpu_quota: config.cpu_quota,
            tasks_max: config.tasks_max,
        });

        let argv = config
            .exec_start
            .clone()
            .ok_or_else(|| format!("{name}: missing ExecStart"))?;
        let spec = self.spawn_spec(config, argv);

        match config.service_type {
            ServiceType::Forking => {
                let mut child =
                    process::spawn(&spec, self.logger.clone(), Some(&cgroup)).map_err(|err| {
                        self.mark_failed(name, UnitResult::ExecFailed(err.to_string()));
                        self.schedule_restart(name, false, None);
                        err.to_string()
                    })?;
                match process::wait_timeout(&mut child, config.timeout_start) {
                    Ok(status) if status.success() => {
                        let unit = self.units.get_mut(name).expect("unit exists");
                        unit.main_pid = None;
                        unit.state = UnitState::Active;
                        unit.sub_state = "running".to_string();
                        unit.started_at = Some(Instant::now());
                        unit.result = UnitResult::Success;
                        // Keep the cgroup: the forked daemon inherits it, which
                        // lets Stop= kill the whole service even without a PID.
                        self.log(name, None, "forked");
                        Ok(())
                    }
                    Ok(status) => {
                        let message = format!("forking parent exited with {status}");
                        self.mark_failed(name, UnitResult::ExecFailed(message.clone()));
                        self.schedule_restart(name, false, status.signal());
                        Err(message)
                    }
                    Err(err) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        let message = format!("forking parent timed out: {err}");
                        self.mark_failed(name, UnitResult::ExecFailed(message.clone()));
                        self.schedule_restart(name, false, None);
                        Err(message)
                    }
                }
            }
            _ => {
                let child =
                    process::spawn(&spec, self.logger.clone(), Some(&cgroup)).map_err(|err| {
                        self.mark_failed(name, UnitResult::ExecFailed(err.to_string()));
                        self.schedule_restart(name, false, None);
                        err.to_string()
                    })?;
                let pid = child.id();
                // Supervision is PID based so that the manager can reap with
                // `waitpid` itself (required when pine runs as PID 1).
                drop(child);
                {
                    let unit = self.units.get_mut(name).expect("unit exists");
                    unit.main_pid = Some(pid);
                    unit.state = UnitState::Active;
                    unit.sub_state = "running".to_string();
                    unit.started_at = Some(Instant::now());
                    unit.start_deadline = None;
                    unit.result = UnitResult::Success;
                }
                self.log(name, Some(pid), "started");
                Ok(())
            }
        }
    }

    fn start_mount(&mut self, config: &UnitConfig) -> Result<(), String> {
        let where_relative = config
            .mount_where
            .clone()
            .ok_or_else(|| format!("{}: missing Where", config.name))?;
        let where_absolute = self.paths.resolve(&where_relative);
        if let Err(err) = std::fs::create_dir_all(&where_absolute) {
            return self.fail_start(
                &config.name,
                format!(
                    "cannot create mount point {}: {err}",
                    where_absolute.display()
                ),
            );
        }

        let what = config
            .mount_what
            .clone()
            .unwrap_or_else(|| "none".to_string());
        let fstype = config
            .mount_type
            .clone()
            .unwrap_or_else(|| "auto".to_string());

        match mount::mount(&what, &where_absolute, &fstype, &config.mount_options) {
            Ok(()) => {
                let unit = self.units.get_mut(&config.name).expect("unit exists");
                unit.state = UnitState::Active;
                unit.sub_state = "mounted".to_string();
                unit.started_at = Some(Instant::now());
                unit.result = UnitResult::Success;
                self.log(
                    &config.name,
                    None,
                    &format!("mounted {what} on {}", where_absolute.display()),
                );
                Ok(())
            }
            Err(err) if err.raw_os_error() == Some(libc::EBUSY) => {
                let unit = self.units.get_mut(&config.name).expect("unit exists");
                unit.state = UnitState::Active;
                unit.sub_state = "mounted".to_string();
                unit.result = UnitResult::Success;
                self.log(&config.name, None, "already mounted");
                Ok(())
            }
            Err(err) => self.fail_start(&config.name, format!("mount failed: {err}")),
        }
    }

    fn start_swap(&mut self, config: &UnitConfig) -> Result<(), String> {
        let what_relative = config
            .swap_what
            .clone()
            .ok_or_else(|| format!("{}: missing What", config.name))?;
        let what = self.paths.resolve(&what_relative);

        match mount::swapon(&what, config.swap_priority) {
            Ok(()) => {
                let unit = self.units.get_mut(&config.name).expect("unit exists");
                unit.state = UnitState::Active;
                unit.sub_state = "active".to_string();
                unit.started_at = Some(Instant::now());
                unit.result = UnitResult::Success;
                self.log(
                    &config.name,
                    None,
                    &format!("swap enabled on {}", what.display()),
                );
                Ok(())
            }
            Err(err) if err.raw_os_error() == Some(libc::EBUSY) => {
                let unit = self.units.get_mut(&config.name).expect("unit exists");
                unit.state = UnitState::Active;
                unit.sub_state = "active".to_string();
                unit.result = UnitResult::Success;
                self.log(&config.name, None, "swap already enabled");
                Ok(())
            }
            Err(err) => self.fail_start(&config.name, format!("swapon failed: {err}")),
        }
    }

    fn fail_start(&mut self, name: &str, message: String) -> Result<(), String> {
        self.mark_failed(name, UnitResult::ExecFailed(message.clone()));
        self.trigger_hooks(name, false);
        Err(message)
    }

    fn stop_conflicts(&mut self, config: &UnitConfig) {
        let mut conflicts: std::collections::BTreeSet<String> =
            config.conflicts.iter().cloned().collect();
        for (name, unit) in &self.units {
            if unit
                .config
                .conflicts
                .iter()
                .any(|other| other == &config.name)
            {
                conflicts.insert(name.clone());
            }
        }
        conflicts.remove(&config.name);

        for name in conflicts {
            let active = self
                .units
                .get(&name)
                .map(|unit| {
                    matches!(
                        unit.state,
                        UnitState::Active | UnitState::Activating | UnitState::Deactivating
                    )
                })
                .unwrap_or(false);
            if active {
                self.log_global(
                    "info",
                    &format!("stopping {name} (conflicts with {})", config.name),
                );
                let _ = self.stop_unit(&name, true);
            }
        }
    }

    fn check_requirements(&self, config: &UnitConfig) -> Result<(), String> {
        for dependency in config.requires.iter().chain(config.binds_to.iter()) {
            match self.units.get(dependency) {
                Some(unit)
                    if unit.state == UnitState::Active
                        || (unit.started_at.is_some()
                            && unit.result == UnitResult::Success
                            && matches!(
                                unit.state,
                                UnitState::Inactive
                                    | UnitState::Activating
                                    | UnitState::Deactivating
                            )) => {}
                Some(unit) if unit.state == UnitState::Failed => {
                    return Err(format!(
                        "{}: required unit {dependency} failed",
                        config.name
                    ));
                }
                Some(_) => {
                    return Err(format!(
                        "{}: required unit {dependency} did not start",
                        config.name
                    ));
                }
                None => {
                    return Err(format!(
                        "{}: required unit {dependency} is not loaded",
                        config.name
                    ));
                }
            }
        }

        for dependency in &config.requisite {
            let active = self
                .units
                .get(dependency)
                .map(|unit| unit.state == UnitState::Active)
                .unwrap_or(false);
            if !active {
                return Err(format!(
                    "{}: requisite unit {dependency} is not active",
                    config.name
                ));
            }
        }

        Ok(())
    }

    fn trigger_hooks(&mut self, name: &str, success: bool) {
        if self.hooks_running {
            return;
        }
        let hooks = match self.units.get(name) {
            Some(unit) if success => unit.config.on_success.clone(),
            Some(unit) => unit.config.on_failure.clone(),
            None => return,
        };
        if hooks.is_empty() {
            return;
        }

        self.hooks_running = true;
        for hook in hooks {
            if self.units.contains_key(&hook) {
                let _ = self.start_unit(&hook, false);
            } else {
                let message = format!("{name}: hook unit {hook} is not loaded");
                self.log_global("warn", &message);
            }
        }
        self.hooks_running = false;
    }

    /// Stop a unit.
    pub fn stop_unit(&mut self, name: &str, manual: bool) -> Result<(), String> {
        let config = match self.units.get(name) {
            Some(unit) => unit.config.clone(),
            None => return Err(format!("unknown unit: {name}")),
        };

        let active = self
            .units
            .get(name)
            .map(|unit| {
                unit.main_pid.is_some()
                    || matches!(
                        unit.state,
                        UnitState::Active | UnitState::Activating | UnitState::Deactivating
                    )
            })
            .unwrap_or(false);

        if !active {
            if let Some(unit) = self.units.get_mut(name) {
                unit.state = UnitState::Inactive;
                unit.sub_state = "dead".to_string();
                unit.restart_at = None;
            }
            return Ok(());
        }

        if config.is_target() {
            let unit = self.units.get_mut(name).expect("unit exists");
            unit.state = UnitState::Inactive;
            unit.sub_state = "dead".to_string();
            return Ok(());
        }

        {
            let unit = self.units.get_mut(name).expect("unit exists");
            unit.state = UnitState::Deactivating;
            unit.sub_state = "stop".to_string();
            unit.manual_stop = manual;
            unit.restart_at = None;
        }

        if config.kind == UnitKind::Mount {
            if let Some(where_) = &config.mount_where {
                let where_ = self.paths.resolve(where_);
                if let Err(err) = mount::unmount(&where_, false) {
                    self.log(name, None, &format!("unmount {}: {err}", where_.display()));
                    let _ = mount::unmount(&where_, true);
                }
            }
            let unit = self.units.get_mut(name).expect("unit exists");
            unit.state = UnitState::Inactive;
            unit.sub_state = "dead".to_string();
            self.log(name, None, "unmounted");
            return Ok(());
        }

        if config.kind == UnitKind::Swap {
            if let Some(what) = &config.swap_what {
                let what = self.paths.resolve(what);
                if let Err(err) = mount::swapoff(&what) {
                    self.log(name, None, &format!("swapoff {}: {err}", what.display()));
                }
            }
            let unit = self.units.get_mut(name).expect("unit exists");
            unit.state = UnitState::Inactive;
            unit.sub_state = "dead".to_string();
            self.log(name, None, "swap disabled");
            return Ok(());
        }

        let mut ran_exec_stop = false;
        for argv in &config.exec_stop {
            ran_exec_stop = true;
            let spec = self.spawn_spec(&config, argv.clone());
            let _ = process::run_blocking(&spec, config.timeout_stop, self.logger.clone());
        }

        let pid = self.units.get(name).and_then(|unit| unit.main_pid);
        if !ran_exec_stop {
            if let Some(pid) = pid {
                let _ = process::kill_group(pid, config.kill_signal);
            }
        }

        let deadline = Instant::now() + config.timeout_stop;
        let mut killed = false;
        loop {
            let done = match pid {
                Some(pid) => !matches!(process::wait_pid(pid), Ok(WaitOutcome::Running)),
                None => true,
            };
            if done {
                break;
            }

            if !killed && Instant::now() >= deadline {
                if config.send_sigkill {
                    if let Some(pid) = pid {
                        let _ = process::kill_group(pid, libc::SIGKILL);
                    }
                }
                killed = true;
            } else if killed && Instant::now() > deadline + Duration::from_secs(2) {
                break;
            }

            std::thread::sleep(Duration::from_millis(20));
        }

        if let Some(pid) = pid {
            let deadline = Instant::now() + Duration::from_secs(2);
            while matches!(process::wait_pid(pid), Ok(WaitOutcome::Running))
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        let cgroup = Cgroup::new(self.paths.cgroup_root.clone(), name);
        let _ = cgroup.kill();
        let _ = cgroup.remove();

        for argv in &config.exec_stop_post {
            let spec = self.spawn_spec(&config, argv.clone());
            let _ = process::run_blocking(&spec, config.timeout_stop, self.logger.clone());
        }

        {
            let unit = self.units.get_mut(name).expect("unit exists");
            unit.state = UnitState::Inactive;
            unit.sub_state = "dead".to_string();
            unit.main_pid = None;
            unit.start_deadline = None;
            unit.stop_deadline = None;
            unit.result = UnitResult::Success;
        }
        self.log(name, None, "stopped");
        Ok(())
    }

    /// Restart a unit (stop then start).
    pub fn restart_unit(&mut self, name: &str) -> Result<(), String> {
        self.stop_unit(name, true)?;
        self.start_unit(name, true)
    }

    /// Reload a unit.
    pub fn reload_unit(&mut self, name: &str) -> Result<(), String> {
        let config = match self.units.get(name) {
            Some(unit) => unit.config.clone(),
            None => return Err(format!("unknown unit: {name}")),
        };

        if !config.exec_reload.is_empty() {
            for argv in &config.exec_reload {
                let spec = self.spawn_spec(&config, argv.clone());
                let status =
                    process::run_blocking(&spec, config.timeout_start, self.logger.clone())
                        .map_err(|err| err.to_string())?;
                if !status.success() {
                    return Err(format!("ExecReload exited with {status}"));
                }
            }
            return Ok(());
        }

        if let Some(pid) = self.units.get(name).and_then(|unit| unit.main_pid) {
            process::kill_pid(pid, libc::SIGHUP).map_err(|err| err.to_string())?;
            return Ok(());
        }

        Err(format!("{name} has no ExecReload and is not running"))
    }

    /// Inspect a unit.
    pub fn status(&self, name: &str) -> Option<UnitStatus> {
        self.units.get(name).map(|unit| UnitStatus {
            name: unit.config.name.clone(),
            description: unit.config.description.clone(),
            load_state: "loaded".to_string(),
            active_state: unit.active_state().to_string(),
            sub_state: unit.sub_state.clone(),
            main_pid: unit.main_pid,
            result: unit.result.to_string(),
        })
    }

    /// List all units.
    pub fn list(&self) -> Vec<UnitStatus> {
        self.units
            .keys()
            .filter_map(|name| self.status(name))
            .collect()
    }

    /// Periodically drive supervision.
    pub fn tick(&mut self) {
        let now = Instant::now();

        let pids: Vec<u32> = self
            .units
            .values()
            .filter_map(|unit| unit.main_pid)
            .collect();
        for (pid, status) in process::reap_known(&pids) {
            let name = self
                .units
                .iter()
                .find(|(_, unit)| unit.main_pid == Some(pid))
                .map(|(name, _)| name.clone());
            if let Some(name) = name {
                if let Some(unit) = self.units.get_mut(&name) {
                    unit.main_pid = None;
                }
                self.on_child_exit(&name, status);
            }
        }
        process::reap_orphans();

        let names: Vec<String> = self.units.keys().cloned().collect();
        for name in &names {
            let start_timed_out = self.units.get(name).is_some_and(|unit| {
                unit.state == UnitState::Activating
                    && unit.start_deadline.is_some_and(|deadline| now >= deadline)
            });
            if start_timed_out {
                let pid = self.units.get(name).and_then(|unit| unit.main_pid);
                if let Some(pid) = pid {
                    let _ = process::kill_group(pid, libc::SIGKILL);
                }
                self.mark_failed(name, UnitResult::Timeout);
                self.schedule_restart(name, false, None);
                continue;
            }

            let restart_due = self
                .units
                .get(name)
                .is_some_and(|unit| unit.restart_at.is_some_and(|at| now >= at));
            if restart_due {
                if let Some(unit) = self.units.get_mut(name) {
                    unit.restart_at = None;
                }
                let _ = self.start_unit(name, false);
            }
        }
    }

    fn on_child_exit(&mut self, name: &str, status: ExitStatus) {
        let (code, signal, success) = process::exit_info(status);
        let info = ExitInfo {
            code,
            signal,
            success,
            at: Instant::now(),
        };

        let previous_state = {
            let unit = match self.units.get_mut(name) {
                Some(unit) => unit,
                None => return,
            };
            unit.last_exit = Some(info);
            unit.start_deadline = None;
            unit.state
        };

        if previous_state == UnitState::Deactivating {
            self.run_stop_post(name);
            if let Some(unit) = self.units.get_mut(name) {
                unit.state = UnitState::Inactive;
                unit.sub_state = "dead".to_string();
            }
            return;
        }

        let remain_after_exit = self
            .units
            .get(name)
            .map(|unit| unit.config.remain_after_exit)
            .unwrap_or(false);

        if success && remain_after_exit {
            if let Some(unit) = self.units.get_mut(name) {
                unit.state = UnitState::Active;
                unit.sub_state = "exited".to_string();
                unit.result = UnitResult::Success;
            }
            self.log(name, None, "exited successfully");
            return;
        }

        let result = match (code, signal) {
            (_, Some(signal)) => UnitResult::Signal(signal),
            (Some(0), _) | (None, None) => UnitResult::Success,
            (Some(code), _) => UnitResult::ExitCode(code),
        };

        {
            let unit = self.units.get_mut(name).expect("unit exists");
            unit.state = if success {
                UnitState::Inactive
            } else {
                UnitState::Failed
            };
            unit.sub_state = if success {
                "dead".to_string()
            } else {
                "failed".to_string()
            };
            unit.result = result;
        }

        self.log(
            name,
            None,
            &format!("exited (code={:?} signal={:?})", code, signal),
        );
        if success {
            self.trigger_hooks(name, true);
        }
        self.schedule_restart(name, success, signal);
    }

    fn run_stop_post(&mut self, name: &str) {
        let config = match self.units.get(name) {
            Some(unit) => unit.config.clone(),
            None => return,
        };
        for argv in &config.exec_stop_post {
            let spec = self.spawn_spec(&config, argv.clone());
            let _ = process::run_blocking(&spec, config.timeout_stop, self.logger.clone());
        }
    }

    fn mark_failed(&mut self, name: &str, result: UnitResult) {
        if let Some(unit) = self.units.get_mut(name) {
            unit.state = UnitState::Failed;
            unit.sub_state = "failed".to_string();
            unit.result = result.clone();
            unit.main_pid = None;
            unit.start_deadline = None;
        }
        self.log(name, None, &format!("failed: {result}"));
    }

    fn schedule_restart(&mut self, name: &str, success: bool, signal: Option<i32>) {
        let config = match self.units.get(name) {
            Some(unit) => unit.config.clone(),
            None => return,
        };
        if !restart_matches(config.restart, success, signal) {
            if !success {
                self.trigger_hooks(name, false);
            }
            return;
        }

        let now = Instant::now();
        let (attempt, hit_limit) = {
            let unit = self.units.get_mut(name).expect("unit exists");
            if unit.manual_stop {
                return;
            }

            if unit.window_start.map_or(true, |start| {
                now.duration_since(start) > config.start_limit_interval
            }) {
                unit.window_start = Some(now);
                unit.failure_count = 0;
            }
            unit.failure_count += 1;

            if unit.failure_count > config.start_limit_burst {
                unit.state = UnitState::Failed;
                unit.sub_state = "failed".to_string();
                unit.result = UnitResult::StartLimitHit;
                unit.restart_at = None;
                (unit.failure_count, true)
            } else {
                unit.state = UnitState::Inactive;
                unit.sub_state = "auto-restart".to_string();
                unit.restart_at = Some(now + config.restart_sec);
                (unit.failure_count, false)
            }
        };

        if hit_limit {
            self.log(name, None, "start limit hit; giving up");
            self.trigger_hooks(name, false);
        } else {
            self.log(
                name,
                None,
                &format!("scheduled restart {name} (attempt {attempt})"),
            );
        }
    }

    fn condition_ok(&self, condition: &Condition) -> bool {
        match condition {
            Condition::PathExists(path) => self.paths.resolve(path).exists(),
            Condition::PathIsDirectory(path) => self.paths.resolve(path).is_dir(),
            Condition::PathIsSymlink(path) => self
                .paths
                .resolve(path)
                .symlink_metadata()
                .map(|meta| meta.file_type().is_symlink())
                .unwrap_or(false),
            Condition::FileNotEmpty(path) => self
                .paths
                .resolve(path)
                .metadata()
                .map(|meta| meta.is_file() && meta.len() > 0)
                .unwrap_or(false),
            Condition::DirectoryNotEmpty(path) => self
                .paths
                .resolve(path)
                .read_dir()
                .map(|mut entries| entries.next().is_some())
                .unwrap_or(false),
            Condition::KernelCommandLine(needle) => std::fs::read_to_string("/proc/cmdline")
                .map(|line| line.contains(needle))
                .unwrap_or(false),
            Condition::FirstBoot => !self.paths.resolve("/etc/machine-id").exists(),
        }
    }

    fn spawn_spec(&self, config: &crate::unit::UnitConfig, argv: Vec<String>) -> SpawnSpec {
        SpawnSpec {
            unit: config.name.clone(),
            argv,
            user: config.user.clone(),
            group: config.group.clone(),
            working_directory: config
                .working_directory
                .as_ref()
                .map(|path| self.paths.resolve(path)),
            environment: config.environment.clone(),
            environment_file: config
                .environment_file
                .iter()
                .map(|path| self.paths.resolve(path))
                .collect(),
            stdout: config.stdout,
            stderr: config.stderr,
        }
    }

    fn reload_units(&mut self) {
        let report = crate::unit::load_units(&self.paths, &self.config);
        if let Ok(mut logger) = self.logger.lock() {
            for warning in &report.warnings {
                logger.global("warn", warning);
            }
        }

        let old = std::mem::replace(&mut self.units, report.units);
        for (name, previous) in old {
            if let Some(unit) = self.units.get_mut(&name) {
                if previous.main_pid.is_some()
                    || matches!(
                        previous.state,
                        UnitState::Active | UnitState::Activating | UnitState::Deactivating
                    )
                {
                    unit.state = previous.state;
                    unit.sub_state = previous.sub_state.clone();
                    unit.main_pid = previous.main_pid;
                    unit.started_at = previous.started_at;
                    unit.start_deadline = previous.start_deadline;
                    unit.stop_deadline = previous.stop_deadline;
                    unit.restart_at = previous.restart_at;
                    unit.failure_count = previous.failure_count;
                    unit.window_start = previous.window_start;
                    unit.last_exit = previous.last_exit;
                    unit.result = previous.result.clone();
                    unit.manual_stop = previous.manual_stop;
                }
            } else {
                self.units.insert(name, previous);
            }
        }

        self.log_global("info", "unit files reloaded");
    }

    fn handle_signal(&mut self, signal: i32) {
        match signal {
            libc::SIGTERM => self.request_shutdown(ShutdownAction::PowerOff),
            libc::SIGINT => self.request_shutdown(ShutdownAction::Reboot),
            libc::SIGHUP => self.reload_units(),
            libc::SIGUSR1 => self.dump_state(),
            libc::SIGUSR2 => self.log_global(
                "warn",
                "daemon-reexec requested but not implemented; reloading units",
            ),
            _ => {}
        }
    }

    fn dump_state(&mut self) {
        let mut lines = String::new();
        for unit in self.units.values() {
            lines.push_str(&format!(
                "{} {} {} pid={:?} result={}\n",
                unit.config.name,
                unit.active_state(),
                unit.sub_state,
                unit.main_pid,
                unit.result
            ));
        }
        self.log_global("info", "state dump:\n{lines}");
        let path = self.paths.run_dir.join("dump");
        let _ = std::fs::create_dir_all(&self.paths.run_dir);
        let _ = std::fs::write(path, lines);
    }

    fn request_shutdown(&mut self, action: ShutdownAction) {
        self.log_global("info", &format!("shutdown requested: {action}"));
        let target = self.config.default_target.clone();
        let mut order = graph::transaction(&self.units, &target)
            .map(|order| graph::stop_order(&order))
            .unwrap_or_else(|_| {
                let mut names: Vec<String> = self.units.keys().cloned().collect();
                names.reverse();
                names
            });

        // Shut down manually-started units that are outside the default
        // target transaction too.
        let planned: std::collections::BTreeSet<String> = order.iter().cloned().collect();
        let mut extras: Vec<String> = self
            .units
            .values()
            .filter(|unit| {
                !planned.contains(&unit.config.name)
                    && matches!(
                        unit.state,
                        UnitState::Active | UnitState::Activating | UnitState::Deactivating
                    )
            })
            .map(|unit| unit.config.name.clone())
            .collect();
        extras.reverse();
        order.extend(extras);
        order.dedup();

        for name in order {
            let _ = self.stop_unit(&name, true);
        }

        if !self.test_mode {
            match shutdown::perform(action) {
                Ok(()) => {}
                Err(err) => self.log_global("err", &format!("shutdown failed: {err}")),
            }
        }
        self.shutdown = Some(action);
    }

    fn log(&mut self, unit: &str, pid: Option<u32>, message: &str) {
        if let Ok(mut logger) = self.logger.lock() {
            logger.log(unit, pid, message);
        }
    }

    fn log_global(&mut self, level: &str, message: &str) {
        if let Ok(mut logger) = self.logger.lock() {
            logger.global(level, message);
        }
    }
}

fn restart_matches(policy: RestartPolicy, success: bool, signal: Option<i32>) -> bool {
    match policy {
        RestartPolicy::No => false,
        RestartPolicy::Always => true,
        RestartPolicy::OnSuccess => success,
        RestartPolicy::OnFailure => !success,
        RestartPolicy::OnAbnormal => signal.is_some(),
        RestartPolicy::OnAbort => signal.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_policies_match() {
        assert!(!restart_matches(RestartPolicy::No, false, Some(9)));
        assert!(restart_matches(RestartPolicy::Always, false, None));
        assert!(restart_matches(RestartPolicy::OnFailure, false, None));
        assert!(!restart_matches(RestartPolicy::OnFailure, true, None));
        assert!(restart_matches(RestartPolicy::OnSuccess, true, None));
        assert!(restart_matches(RestartPolicy::OnAbnormal, false, Some(9)));
        assert!(!restart_matches(RestartPolicy::OnAbnormal, false, None));
    }
}
