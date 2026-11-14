//! Where things live: exactly `lumbridge_mail_core::paths`, so the app, `lumbridge-maild` and `lbmail`
//! agree (docs/ARCHITECTURE.md, "Names"). `LUMBRIDGE_MAIL_HOME` moves everything under one directory,
//! which tests and sandboxes must set:
//!
//! | What | Default | With `LUMBRIDGE_MAIL_HOME=H` |
//! |---|---|---|
//! | daemon socket | `$XDG_RUNTIME_DIR/lumbridge-mail/maild.sock` | `H/maild.sock` |
//! | person token | next to the socket: `person.token` | `H/person.token` |
//! | app instance socket | next to the socket: `app.sock` | `H/app.sock` |
//! | config (`config.toml`, app settings) | `$XDG_CONFIG_HOME/lumbridge-mail` (`~/.config/lumbridge-mail`) | `H/config` |
//! | data (store, `maild.log`) | `$XDG_DATA_HOME/lumbridge-mail` (`~/.local/share/lumbridge-mail`) | `H/data` |
//!
//! macOS uses the same XDG-style paths. `XDG_RUNTIME_DIR` is not set there, so the runtime directory is
//! `$TMPDIR/lumbridge-mail-<user>` (per user, 0700). A socket path longer than macOS's 104-byte
//! `sun_path` (a deep `LUMBRIDGE_MAIL_HOME`) moves to a short per-home directory under `/tmp`
//! (`core::paths::Paths::socket_dir`); the client then checks that the daemon on the other end is our own
//! user before it shows the person token (`backend::rpc`).

use lumbridge_mail_core::paths::Paths;
use std::path::PathBuf;

/// The paths for this process's environment.
pub fn current() -> Paths {
    Paths::from_env()
}

pub fn socket() -> PathBuf {
    current().socket()
}

pub fn instance_socket() -> PathBuf {
    current().app_socket()
}

pub fn config_dir() -> PathBuf {
    current().config_dir
}

pub fn data_dir() -> PathBuf {
    current().data_dir
}

/// The daemon's log when the app started it.
pub fn daemon_log() -> PathBuf {
    data_dir().join("maild.log")
}

/// Create a private directory (0700 on Unix) if it is missing.
pub fn ensure_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumbridge_mail_core::paths::HOME_VAR;
    use std::ffi::OsString;

    fn resolve(pairs: &[(&str, &str)]) -> Paths {
        let pairs: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Paths::resolve(move |k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| OsString::from(v)))
    }

    #[test]
    fn the_app_uses_the_daemons_paths() {
        let p = resolve(&[("HOME", "/home/sam"), ("XDG_RUNTIME_DIR", "/run/user/1000")]);
        assert_eq!(p.socket(), PathBuf::from("/run/user/1000/lumbridge-mail/maild.sock"));
        assert_eq!(p.app_socket(), PathBuf::from("/run/user/1000/lumbridge-mail/app.sock"));
        assert_eq!(p.person_token(), PathBuf::from("/run/user/1000/lumbridge-mail/person.token"));
        assert_eq!(p.config_dir, PathBuf::from("/home/sam/.config/lumbridge-mail"));
        assert_eq!(p.data_dir, PathBuf::from("/home/sam/.local/share/lumbridge-mail"));
    }

    #[test]
    fn lumbridge_mail_home_moves_everything() {
        let p = resolve(&[(HOME_VAR, "/sandbox"), ("XDG_RUNTIME_DIR", "/run/user/1000")]);
        assert_eq!(p.socket(), PathBuf::from("/sandbox/maild.sock"));
        assert_eq!(p.app_socket(), PathBuf::from("/sandbox/app.sock"));
        assert_eq!(p.config_dir, PathBuf::from("/sandbox/config"));
        assert_eq!(p.data_dir, PathBuf::from("/sandbox/data"));
    }

    #[test]
    fn no_runtime_dir_is_per_user_under_tmp() {
        let p = resolve(&[("HOME", "/Users/sam"), ("USER", "sam"), ("TMPDIR", "/var/folders/ab/xyz/T/")]);
        assert_eq!(p.socket(), PathBuf::from("/var/folders/ab/xyz/T/lumbridge-mail-sam/maild.sock"));
    }
}
