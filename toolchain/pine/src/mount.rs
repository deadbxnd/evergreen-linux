//! Mount and swap unit primitives.
//!
//! Thin, best-effort wrappers around `mount(2)`, `umount2(2)`, `swapon(2)`,
//! and `swapoff(2)`. Errors are returned to the manager, which logs them and
//! marks the unit failed.

use std::ffi::CString;
use std::io;
use std::path::Path;

/// Split mount options into generic `mount(2)` flags and filesystem-specific
/// data passed through to the kernel.
pub fn split_options(options: &[String]) -> (libc::c_ulong, String) {
    let mut flags = 0;
    let mut data = Vec::new();

    for option in options {
        let option = option.trim();
        if option.is_empty() {
            continue;
        }
        match option {
            "defaults" | "rw" => {}
            "ro" => flags |= libc::MS_RDONLY,
            "nosuid" => flags |= libc::MS_NOSUID,
            "nodev" => flags |= libc::MS_NODEV,
            "noexec" => flags |= libc::MS_NOEXEC,
            "sync" => flags |= libc::MS_SYNCHRONOUS,
            "dirsync" => flags |= libc::MS_DIRSYNC,
            "remount" => flags |= libc::MS_REMOUNT,
            "bind" => flags |= libc::MS_BIND,
            "rbind" => flags |= libc::MS_BIND | libc::MS_REC,
            "relatime" => flags |= libc::MS_RELATIME,
            "noatime" => flags |= libc::MS_NOATIME,
            "nodiratime" => flags |= libc::MS_NODIRATIME,
            "silent" => flags |= libc::MS_SILENT,
            "lazytime" => flags |= libc::MS_LAZYTIME,
            other => data.push(other.to_string()),
        }
    }

    (flags, data.join(","))
}

/// Mount `what` on `where_`.
pub fn mount(what: &str, where_: &Path, fstype: &str, options: &[String]) -> io::Result<()> {
    let (flags, data) = split_options(options);
    let what = CString::new(what)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid mount source"))?;
    let target = CString::new(where_.to_string_lossy().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid mount point"))?;
    let fstype = CString::new(fstype)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid filesystem type"))?;
    let data = CString::new(data)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid mount options"))?;

    let result = unsafe {
        libc::mount(
            what.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            flags,
            data.as_ptr() as *const libc::c_void,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Unmount `where_`.
pub fn unmount(where_: &Path, lazy: bool) -> io::Result<()> {
    let target = CString::new(where_.to_string_lossy().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid mount point"))?;
    let flags = if lazy { libc::MNT_DETACH } else { 0 };
    let result = unsafe { libc::umount2(target.as_ptr(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// `SWAP_FLAG_PREFER` from `linux/swap.h` (the bit that enables priority).
const SWAP_FLAG_PREFER: libc::c_int = 0x8000;

/// Enable a swap device or file.
pub fn swapon(what: &Path, priority: Option<i32>) -> io::Result<()> {
    let path = CString::new(what.to_string_lossy().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid swap path"))?;
    let flags = match priority {
        Some(priority) => {
            let priority = priority.clamp(0, 0x7fff) as libc::c_int;
            SWAP_FLAG_PREFER | priority
        }
        None => 0,
    };
    let result = unsafe { libc::swapon(path.as_ptr(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Disable a swap device or file.
pub fn swapoff(what: &Path) -> io::Result<()> {
    let path = CString::new(what.to_string_lossy().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid swap path"))?;
    let result = unsafe { libc::swapoff(path.as_ptr()) };
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
    fn splits_generic_and_filesystem_options() {
        let options = vec![
            "defaults".to_string(),
            "noatime".to_string(),
            "compress=zstd".to_string(),
            "ro".to_string(),
        ];
        let (flags, data) = split_options(&options);
        assert_ne!(flags & libc::MS_NOATIME, 0);
        assert_ne!(flags & libc::MS_RDONLY, 0);
        assert_eq!(data, "compress=zstd");
    }

    #[test]
    fn missing_mount_point_is_an_error() {
        let result = mount(
            "none",
            Path::new("/definitely/not/a/mountpoint"),
            "tmpfs",
            &[],
        );
        assert!(result.is_err());
    }
}
