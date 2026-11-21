//! Unix control socket (JSON-RPC style) used by `pinectl`.
//!
//! This socket is available before D-Bus is up, including early boot and rescue
//! mode. Each connection accepts newline-delimited JSON requests.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::manager::{ManagerHandle, Response};
use crate::shutdown::ShutdownAction;

#[derive(Debug, Deserialize)]
struct Request {
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

/// Bind the control socket and serve requests on background threads.
pub fn spawn_server(
    socket: impl AsRef<Path>,
    handle: ManagerHandle,
) -> std::io::Result<thread::JoinHandle<()>> {
    let socket = socket.as_ref().to_path_buf();
    if socket.exists() {
        let _ = std::fs::remove_file(&socket);
    }
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(&socket)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
    }
    thread::Builder::new()
        .name("pine-control".to_string())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let handle = handle.clone();
                let _ = thread::Builder::new()
                    .name("pine-control-client".to_string())
                    .spawn(move || {
                        let _ = serve_client(stream, handle);
                    });
            }
        })
}

fn serve_client(stream: UnixStream, handle: ManagerHandle) -> std::io::Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(err) => {
                let payload = json!({
                    "id": Value::Null,
                    "ok": false,
                    "error": format!("invalid request: {err}"),
                });
                writeln!(writer, "{payload}")?;
                writer.flush()?;
                continue;
            }
        };

        let response = dispatch(&handle, &request.method, &request.params);
        let ok = !matches!(response, Response::Error { .. });
        let result = serde_json::to_value(&response).unwrap_or(Value::Null);
        let payload = json!({
            "id": request.id,
            "ok": ok,
            "result": result,
        });
        writeln!(writer, "{payload}")?;
        writer.flush()?;
    }

    Ok(())
}

fn dispatch(handle: &ManagerHandle, method: &str, params: &Value) -> Response {
    let name = || {
        params
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
    };

    match method {
        "ping" => Response::Message {
            text: "pong".to_string(),
        },
        "list" | "list-units" => handle.list(),
        "status" => match name() {
            Some(name) => handle.status(name),
            None => Response::error("missing \"name\" parameter"),
        },
        "start" => match name() {
            Some(name) => handle.start(name),
            None => Response::error("missing \"name\" parameter"),
        },
        "stop" => match name() {
            Some(name) => handle.stop(name),
            None => Response::error("missing \"name\" parameter"),
        },
        "restart" => match name() {
            Some(name) => handle.restart(name),
            None => Response::error("missing \"name\" parameter"),
        },
        "reload" => match name() {
            Some(name) => handle.reload(name),
            None => Response::error("missing \"name\" parameter"),
        },
        "log" => {
            let Some(name) = name() else {
                return Response::error("missing \"name\" parameter");
            };
            let lines = params
                .get("lines")
                .and_then(Value::as_u64)
                .unwrap_or(50)
                .min(10_000) as usize;
            handle.log(name, lines)
        }
        "reset-failed" => handle.reset_failed(name()),
        "daemon-reload" => handle.daemon_reload(),
        "poweroff" | "shutdown" => match parse_action(params) {
            Ok(action) => handle.shutdown(action),
            Err(err) => Response::error(err),
        },
        "reboot" => handle.shutdown(ShutdownAction::Reboot),
        "halt" => handle.shutdown(ShutdownAction::Halt),
        other => Response::error(format!("unknown method {other:?}")),
    }
}

fn parse_action(params: &Value) -> Result<ShutdownAction, String> {
    match params.get("action").and_then(Value::as_str) {
        Some(value) => value.parse(),
        None => Ok(ShutdownAction::PowerOff),
    }
}

/// Root directory for the control socket in tests.
pub fn test_socket_path(dir: impl AsRef<Path>) -> PathBuf {
    dir.as_ref().join("control.sock")
}
