//! End-to-end tests for the pine manager and control socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pine::config::{Config, Paths};
use pine::control;
use pine::log::Logger;
use pine::manager::{Manager, ManagerHandle, Response};
use pine::shutdown::ShutdownAction;
use pine::unit::{UnitResult, UnitState};

fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "pine-it-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("etc/pine/services")).unwrap();
    root
}

fn write_unit(root: &Path, name: &str, body: &str) {
    let dir = root.join("etc/pine/services");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), body).unwrap();
}

fn logger_for(paths: &Paths) -> Arc<Mutex<Logger>> {
    Arc::new(Mutex::new(Logger::new(&paths.log_dir, false).unwrap()))
}

fn wait_for(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    predicate()
}

#[test]
fn boots_target_and_supervises_services() {
    let root = temp_root("boot");
    let marker = root.join("hello.txt");

    write_unit(
        &root,
        "hello.service",
        &format!(
            r#"
[Service]
Type = "oneshot"
ExecStart = "/bin/sh -c 'echo hello > {}'"
"#,
            marker.display()
        ),
    );
    write_unit(
        &root,
        "sleeper.service",
        r#"
[Service]
Type = "simple"
ExecStart = "/bin/sleep 30"
"#,
    );
    write_unit(
        &root,
        "app.target",
        r#"
[Unit]
Wants = ["hello.service", "sleeper.service"]
"#,
    );

    let paths = Paths::for_root(&root);
    let logger = logger_for(&paths);
    let mut manager = Manager::new(paths, Config::default(), logger, true);

    manager.boot("app.target").unwrap();

    assert!(
        wait_for(Duration::from_secs(3), || marker.exists()),
        "oneshot did not run"
    );
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "hello");

    let sleeper = manager.status("sleeper.service").unwrap();
    assert_eq!(sleeper.active_state, "active");
    assert_eq!(sleeper.sub_state, "running");
    assert!(sleeper.main_pid.is_some());

    manager.stop_unit("sleeper.service", true).unwrap();
    let sleeper = manager.status("sleeper.service").unwrap();
    assert_eq!(sleeper.active_state, "inactive");
    assert!(sleeper.main_pid.is_none());

    manager.stop_unit("app.target", true).unwrap();
    assert_eq!(
        manager.status("app.target").unwrap().active_state,
        "inactive"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn restart_limit_marks_unit_failed() {
    let root = temp_root("restart");
    write_unit(
        &root,
        "flaky.service",
        r#"
[Service]
Type = "simple"
ExecStart = "/bin/false"
Restart = "on-failure"
RestartSec = "20ms"
StartLimitIntervalSec = "5s"
StartLimitBurst = 2
"#,
    );

    let paths = Paths::for_root(&root);
    let logger = logger_for(&paths);
    let mut manager = Manager::new(paths, Config::default(), logger, true);
    manager.boot("flaky.service").unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        manager.tick();
        let unit = manager.units.get("flaky.service").unwrap();
        if unit.state == UnitState::Failed && unit.result == UnitResult::StartLimitHit {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let unit = manager.units.get("flaky.service").unwrap();
    assert_eq!(unit.state, UnitState::Failed, "state={:?}", unit.state);
    assert_eq!(unit.result, UnitResult::StartLimitHit);

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn control_socket_lists_and_shuts_down() {
    let root = temp_root("control");
    write_unit(
        &root,
        "sleeper.service",
        r#"
[Service]
Type = "simple"
ExecStart = "/bin/sleep 30"
"#,
    );
    write_unit(
        &root,
        "app.target",
        r#"
[Unit]
Wants = ["sleeper.service"]
"#,
    );

    let paths = Paths::for_root(&root);
    let socket = paths.control_socket.clone();
    let logger = logger_for(&paths);
    let manager = Manager::new(paths, Config::default(), logger, true);

    let (tx, rx) = mpsc::channel();
    let handle = ManagerHandle::new(tx);
    let manager_thread = std::thread::spawn(move || {
        let mut manager = manager;
        manager.run(rx, "app.target");
    });

    let _server = control::spawn_server(&socket, handle.clone()).unwrap();
    assert!(
        wait_for(Duration::from_secs(3), || socket.exists()),
        "control socket was not created"
    );

    let mut stream = UnixStream::connect(&socket).unwrap();
    writeln!(stream, r#"{{"id":1,"method":"list","params":{{}}}}"#).unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["ok"], true, "response: {response}");
    let units = response["result"]["units"].as_array().unwrap();
    assert!(
        units.iter().any(|unit| unit["name"] == "sleeper.service"),
        "sleeper missing from list: {response}"
    );

    let response = handle.shutdown(ShutdownAction::PowerOff);
    assert!(matches!(response, Response::Ok));
    manager_thread.join().unwrap();

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn conditions_skip_units() {
    let root = temp_root("conditions");
    write_unit(
        &root,
        "gated.service",
        r#"
[Unit]
ConditionPathExists = "/definitely/not/here"

[Service]
Type = "oneshot"
ExecStart = "/bin/false"
"#,
    );

    let paths = Paths::for_root(&root);
    let logger = logger_for(&paths);
    let mut manager = Manager::new(paths, Config::default(), logger, true);
    manager.boot("gated.service").unwrap();

    let unit = manager.units.get("gated.service").unwrap();
    assert_eq!(unit.state, UnitState::Inactive);
    assert_eq!(unit.sub_state, "condition-failed");
    assert_eq!(unit.result, UnitResult::ConditionFailed);

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn conflicts_stop_the_other_unit() {
    let root = temp_root("conflicts");
    write_unit(
        &root,
        "first.service",
        r#"
[Unit]
Conflicts = ["second.service"]

[Service]
Type = "simple"
ExecStart = "/bin/sleep 30"
"#,
    );
    write_unit(
        &root,
        "second.service",
        r#"
[Service]
Type = "simple"
ExecStart = "/bin/sleep 30"
"#,
    );

    let paths = Paths::for_root(&root);
    let logger = logger_for(&paths);
    let mut manager = Manager::new(paths, Config::default(), logger, true);

    manager.start_unit("first.service", true).unwrap();
    assert_eq!(
        manager.status("first.service").unwrap().active_state,
        "active"
    );

    manager.start_unit("second.service", true).unwrap();
    assert_eq!(
        manager.status("first.service").unwrap().active_state,
        "inactive"
    );
    assert_eq!(
        manager.status("second.service").unwrap().active_state,
        "active"
    );

    manager.stop_unit("second.service", true).unwrap();
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn on_failure_hook_runs() {
    let root = temp_root("onfailure");
    let marker = root.join("hook-ran.txt");

    write_unit(
        &root,
        "broken.service",
        r#"
[Unit]
OnFailure = ["hook.service"]

[Service]
Type = "oneshot"
ExecStart = "/bin/false"
"#,
    );
    write_unit(
        &root,
        "hook.service",
        &format!(
            r#"
[Service]
Type = "oneshot"
ExecStart = "/bin/sh -c 'echo hook > {}'"
"#,
            marker.display()
        ),
    );

    let paths = Paths::for_root(&root);
    let logger = logger_for(&paths);
    let mut manager = Manager::new(paths, Config::default(), logger, true);
    let _ = manager.boot("broken.service");

    assert!(marker.exists(), "OnFailure hook did not run");
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "hook");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn failed_requirement_fails_the_target() {
    let root = temp_root("requires");
    write_unit(
        &root,
        "broken.service",
        r#"
[Service]
Type = "oneshot"
ExecStart = "/bin/false"
"#,
    );
    write_unit(
        &root,
        "app.target",
        r#"
[Unit]
Requires = ["broken.service"]
"#,
    );

    let paths = Paths::for_root(&root);
    let logger = logger_for(&paths);
    let mut manager = Manager::new(paths, Config::default(), logger, true);
    assert!(manager.boot("app.target").is_err());

    assert_eq!(manager.status("app.target").unwrap().active_state, "failed");
    assert_eq!(
        manager.status("broken.service").unwrap().active_state,
        "failed"
    );

    std::fs::remove_dir_all(&root).ok();
}
