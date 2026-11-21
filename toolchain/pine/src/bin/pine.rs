//! `pine` — PID 1 entry point.

use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use clap::Parser;
use pine::config::{Config, Paths};
use pine::log::Logger;
use pine::manager::{Command, Manager, ManagerHandle};
use pine::{control, process};

#[derive(Debug, Parser)]
#[command(name = "pine", version, about = "Evergreen Linux init system")]
struct Cli {
    /// Path to pine.conf.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Additional unit search directory (repeatable).
    #[arg(long = "unit-dir")]
    unit_dirs: Vec<PathBuf>,

    /// Directory for per-unit logs.
    #[arg(long)]
    log_dir: Option<PathBuf>,

    /// Path to the control socket.
    #[arg(long)]
    control_socket: Option<PathBuf>,

    /// Alternate filesystem root (mainly for testing).
    #[arg(long)]
    root: Option<PathBuf>,

    /// Target to boot instead of the configured default.
    #[arg(long)]
    target: Option<String>,

    /// Run without PID 1 privileges: no mounts, no reboot.
    #[arg(long)]
    test: bool,

    /// Mirror log output to stderr.
    #[arg(long)]
    console: bool,

    /// Parse unit files, print a summary, and exit.
    #[arg(long)]
    dump_units: bool,
}

fn main() {
    // A closed control-socket or log pipe must never kill PID 1.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let cli = Cli::parse();

    let mut paths = match &cli.root {
        Some(root) => Paths::for_root(root),
        None => Paths::system(),
    };
    if let Some(config) = &cli.config {
        paths.config_file = config.clone();
    }
    if let Some(log_dir) = &cli.log_dir {
        paths.log_dir = log_dir.clone();
    }
    if let Some(socket) = &cli.control_socket {
        paths.control_socket = socket.clone();
    }
    if !cli.unit_dirs.is_empty() {
        paths.unit_dirs = cli.unit_dirs.clone();
    }

    if cli.dump_units {
        dump_units(&paths);
        return;
    }

    let test_mode = cli.test || !process::is_pid1();
    if process::is_pid1() && !test_mode {
        for warning in process::prepare_pid1() {
            eprintln!("pine: {warning}");
        }
    }

    let config = match Config::load(&paths.config_file) {
        Ok(config) => config,
        Err(err) => {
            eprintln!(
                "pine: failed to load {}: {err}",
                paths.config_file.display()
            );
            Config::default()
        }
    };

    let logger = match open_logger(&paths, cli.console) {
        Some(logger) => logger,
        None => {
            eprintln!("pine: cannot open any log directory");
            std::process::exit(1);
        }
    };

    let target = cli
        .target
        .clone()
        .unwrap_or_else(|| config.default_target.clone());

    let mut manager = Manager::new(paths.clone(), config, logger, test_mode);

    let (tx, rx) = mpsc::channel();
    let handle = ManagerHandle::new(tx.clone());

    if let Err(err) = control::spawn_server(&paths.control_socket, handle.clone()) {
        eprintln!(
            "pine: cannot bind control socket {}: {err}",
            paths.control_socket.display()
        );
    }

    spawn_signal_thread(tx);

    manager.run(rx, &target);

    // Give in-flight control replies time to flush before the process exits.
    std::thread::sleep(std::time::Duration::from_millis(200));
    handle.terminate();
}

fn open_logger(paths: &Paths, console: bool) -> Option<Arc<Mutex<Logger>>> {
    let candidates = [
        paths.log_dir.clone(),
        paths.run_dir.join("log"),
        PathBuf::from("/tmp/pine-log"),
    ];
    for dir in candidates {
        match Logger::new(&dir, console) {
            Ok(logger) => return Some(Arc::new(Mutex::new(logger))),
            Err(err) => eprintln!("pine: log directory {}: {err}", dir.display()),
        }
    }
    None
}

fn dump_units(paths: &Paths) {
    let config = Config::load(&paths.config_file).unwrap_or_default();
    let report = pine::unit::load_units(paths, &config);
    for name in report.units.keys() {
        println!("{name}");
    }
    for warning in &report.warnings {
        eprintln!("pine: warning: {warning}");
    }
}

fn spawn_signal_thread(tx: Sender<Command>) {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGUSR1, SIGUSR2};
    use signal_hook::iterator::Signals;

    let mut signals = match Signals::new([SIGTERM, SIGINT, SIGHUP, SIGUSR1, SIGUSR2]) {
        Ok(signals) => signals,
        Err(err) => {
            eprintln!("pine: cannot install signal handlers: {err}");
            return;
        }
    };

    std::thread::Builder::new()
        .name("pine-signals".to_string())
        .spawn(move || {
            for signal in signals.forever() {
                if tx.send(Command::Signal(signal)).is_err() {
                    break;
                }
            }
        })
        .ok();
}
