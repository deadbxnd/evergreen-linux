//! `pinectl` — command-line control for pine.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::{json, Value};

#[derive(Debug, Parser)]
#[command(name = "pinectl", version, about = "Control the pine init system")]
struct Cli {
    /// Path to the pine control socket.
    #[arg(long, global = true, default_value = "/run/pine/control.sock")]
    socket: PathBuf,

    /// Print raw JSON responses.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List all loaded units.
    List,
    /// Show the status of a unit.
    Status { name: String },
    /// Start a unit.
    Start { name: String },
    /// Stop a unit.
    Stop { name: String },
    /// Restart a unit.
    Restart { name: String },
    /// Reload a unit.
    Reload { name: String },
    /// Show a unit's log.
    Log {
        name: String,
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
    },
    /// Reset the failed state of one unit or all units.
    ResetFailed { name: Option<String> },
    /// Reload unit files.
    DaemonReload,
    /// Power off the machine.
    Poweroff,
    /// Reboot the machine.
    Reboot,
    /// Halt the machine.
    Halt,
    /// Check whether the manager is alive.
    Ping,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let (method, params) = match &cli.command {
        Command::List => ("list", json!({})),
        Command::Status { name } => ("status", json!({ "name": name })),
        Command::Start { name } => ("start", json!({ "name": name })),
        Command::Stop { name } => ("stop", json!({ "name": name })),
        Command::Restart { name } => ("restart", json!({ "name": name })),
        Command::Reload { name } => ("reload", json!({ "name": name })),
        Command::Log { name, lines } => ("log", json!({ "name": name, "lines": lines })),
        Command::ResetFailed { name } => ("reset-failed", json!({ "name": name })),
        Command::DaemonReload => ("daemon-reload", json!({})),
        Command::Poweroff => ("poweroff", json!({ "action": "poweroff" })),
        Command::Reboot => ("reboot", json!({})),
        Command::Halt => ("halt", json!({})),
        Command::Ping => ("ping", json!({})),
    };

    match call(&cli.socket, method, params) {
        Ok(result) => {
            if cli.json {
                match serde_json::to_string_pretty(&result) {
                    Ok(text) => println!("{text}"),
                    Err(err) => eprintln!("pinectl: cannot format response: {err}"),
                }
            } else {
                render(&result);
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("pinectl: {err}");
            ExitCode::FAILURE
        }
    }
}

fn call(socket: &PathBuf, method: &str, params: Value) -> Result<Value, String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|err| format!("cannot connect to {}: {err}", socket.display()))?;
    let request = json!({ "id": 1, "method": method, "params": params });
    let line = serde_json::to_string(&request).map_err(|err| err.to_string())?;
    writeln!(stream, "{line}").map_err(|err| err.to_string())?;
    stream.flush().map_err(|err| err.to_string())?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .map_err(|err| err.to_string())?;

    let response: Value =
        serde_json::from_str(&response).map_err(|err| format!("invalid response: {err}"))?;
    if response.get("ok").and_then(Value::as_bool) == Some(false) {
        let message = response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("command failed");
        return Err(message.to_string());
    }
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

fn render(result: &Value) {
    match result.get("type").and_then(Value::as_str) {
        Some("unit") => {
            let unit = &result["unit"];
            println!("{} - {}", text(unit, "name"), text(unit, "description"));
            println!("   Loaded: {}", text(unit, "load_state"));
            println!(
                "   Active: {} ({})",
                text(unit, "active_state"),
                text(unit, "sub_state")
            );
            if let Some(pid) = unit.get("main_pid").and_then(Value::as_u64) {
                println!(" Main PID: {pid}");
            }
            println!("   Result: {}", text(unit, "result"));
        }
        Some("units") => render_units(result),
        Some("lines") => {
            if let Some(lines) = result.get("lines").and_then(Value::as_array) {
                for line in lines {
                    if let Some(line) = line.as_str() {
                        println!("{line}");
                    }
                }
            }
        }
        Some("message") => println!(
            "{}",
            result.get("text").and_then(Value::as_str).unwrap_or("")
        ),
        _ => println!("OK"),
    }
}

fn render_units(result: &Value) {
    let Some(units) = result.get("units").and_then(Value::as_array) else {
        return;
    };
    println!(
        "{:<32} {:<8} {:<10} {:<14} {:<8} DESCRIPTION",
        "UNIT", "LOAD", "ACTIVE", "SUB", "PID"
    );
    for unit in units {
        let pid = unit
            .get("main_pid")
            .and_then(Value::as_u64)
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<32} {:<8} {:<10} {:<14} {:<8} {}",
            text(unit, "name"),
            text(unit, "load_state"),
            text(unit, "active_state"),
            text(unit, "sub_state"),
            pid,
            text(unit, "description")
        );
    }
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}
