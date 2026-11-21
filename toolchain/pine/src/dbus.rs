//! D-Bus interface (`org.evergreen.Pine1`).
//!
//! The control socket in [`crate::control`] is the primary management
//! interface for 1.0. A D-Bus facade is planned for the next milestone; the
//! constants here define the intended names so clients can be written against
//! them early.

/// Well-known bus name for the manager.
pub const BUS_NAME: &str = "org.evergreen.Pine1";

/// Object path of the manager interface.
pub const MANAGER_PATH: &str = "/org/evergreen/Pine1";

/// Path to the JSON-RPC control socket available before D-Bus.
pub const CONTROL_SOCKET: &str = "/run/pine/control.sock";

/// Prefix for per-unit object paths.
pub const UNIT_PATH_PREFIX: &str = "/org/evergreen/Pine1/unit/";

/// Escape a unit name for use as a D-Bus object path element.
pub fn escape_unit_name(name: &str) -> String {
    let mut escaped = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() {
            escaped.push(byte as char);
        } else {
            escaped.push_str(&format!("_{byte:02x}"));
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_unit_names() {
        assert_eq!(escape_unit_name("sshd.service"), "sshd_2eservice");
        assert_eq!(escape_unit_name("a-b_1"), "a_2db_5f1");
    }
}
