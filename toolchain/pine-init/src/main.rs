//! `pine-init` — static initramfs `/init`.
//!
//! Mounts the root filesystem (handling LUKS/LVM) and hands off to `/sbin/pine`.

use std::process::ExitCode;

fn main() -> ExitCode {
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("pine-init {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    eprintln!("pine-init: initramfs init not yet implemented");
    ExitCode::FAILURE
}
