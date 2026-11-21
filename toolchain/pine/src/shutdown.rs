//! Shutdown, reboot, and emergency handling.

use std::io;

/// Requested shutdown action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownAction {
    PowerOff,
    Reboot,
    Halt,
    KExec,
}

impl std::str::FromStr for ShutdownAction {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "poweroff" | "power-off" | "shutdown" => Ok(Self::PowerOff),
            "reboot" | "restart" => Ok(Self::Reboot),
            "halt" => Ok(Self::Halt),
            "kexec" => Ok(Self::KExec),
            other => Err(format!("unknown shutdown action {other:?}")),
        }
    }
}

impl std::fmt::Display for ShutdownAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::PowerOff => "poweroff",
            Self::Reboot => "reboot",
            Self::Halt => "halt",
            Self::KExec => "kexec",
        };
        f.write_str(text)
    }
}

const RB_AUTOBOOT: libc::c_int = 0x0123_4567u32 as libc::c_int;
const RB_HALT_SYSTEM: libc::c_int = 0xcdef_0123u32 as libc::c_int;
const RB_POWER_OFF: libc::c_int = 0x4321_fedcu32 as libc::c_int;
const RB_KEXEC: libc::c_int = 0x4558_4543u32 as libc::c_int;

/// Flush filesystems and invoke the kernel reboot syscall.
///
/// On a real system this does not return. In an unprivileged process it
/// returns an error instead of touching the host.
pub fn perform(action: ShutdownAction) -> io::Result<()> {
    unsafe {
        libc::sync();
    }

    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "shutdown requires root privileges",
        ));
    }

    let command = match action {
        ShutdownAction::PowerOff => RB_POWER_OFF,
        ShutdownAction::Reboot => RB_AUTOBOOT,
        ShutdownAction::Halt => RB_HALT_SYSTEM,
        ShutdownAction::KExec => RB_KEXEC,
    };

    let result = unsafe { libc::reboot(command) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_actions() {
        assert_eq!(
            "poweroff".parse::<ShutdownAction>().unwrap(),
            ShutdownAction::PowerOff
        );
        assert_eq!(
            "reboot".parse::<ShutdownAction>().unwrap(),
            ShutdownAction::Reboot
        );
        assert!("nope".parse::<ShutdownAction>().is_err());
    }

    #[test]
    fn unprivileged_shutdown_is_refused() {
        if unsafe { libc::geteuid() } != 0 {
            assert!(perform(ShutdownAction::PowerOff).is_err());
        }
    }
}
