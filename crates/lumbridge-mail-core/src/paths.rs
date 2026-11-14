//! Where Lumbridge Mail keeps things (docs/ARCHITECTURE.md, "Names"). Pure: the environment is passed
//! in as a lookup, so tests need not touch the process environment. [`Paths::from_env`] reads the real one.
//!
//! | | default | with `LUMBRIDGE_MAIL_HOME=H` |
//! |---|---|---|
//! | config | `$XDG_CONFIG_HOME/lumbridge-mail` (`~/.config/lumbridge-mail`) | `H/config` |
//! | data (the store) | `$XDG_DATA_HOME/lumbridge-mail` (`~/.local/share/lumbridge-mail`) | `H/data` |
//! | cache (attachments) | `$XDG_CACHE_HOME/lumbridge-mail` (`~/.cache/lumbridge-mail`) | `H/cache` |
//! | runtime (socket, lock, person token) | `$XDG_RUNTIME_DIR/lumbridge-mail` (else `<tmp>/lumbridge-mail-<user>`) | `H` |
//!
//! **Socket path length.** A Unix socket path must fit `sockaddr_un.sun_path`: 104 bytes on macOS and the
//! BSDs, 108 on Linux, the terminating NUL included. A deep `LUMBRIDGE_MAIL_HOME` (a test's temp dir, a
//! macOS `$TMPDIR` under `/var/folders/...`) can be longer. When `runtime/maild.sock` would not fit in
//! [`MAX_SOCKET_PATH`] bytes, the socket and the person token move to
//! `/tmp/lumbridge-mail-<16 hex digits of a hash of the runtime directory>/` ([`Paths::socket_dir`]), so
//! the daemon, the app and `lbmail` still agree. That directory is in a shared `/tmp`, so whoever opens it
//! must check it: the daemon refuses one that is not a plain 0700 directory owned by its user, and clients
//! check that the process on the other end of the socket runs as their user before showing the token.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The environment variable that moves everything under one directory (tests and sandboxes set it).
pub const HOME_VAR: &str = "LUMBRIDGE_MAIL_HOME";

/// The longest socket path used as is, in bytes: macOS's `sun_path` is 104 bytes with the NUL, so 103
/// would fit; a few bytes are kept spare. Longer paths use the short fallback (see the module docs).
pub const MAX_SOCKET_PATH: usize = 100;

/// FNV-1a, 64 bits: a stable, dependency-free hash for the fallback directory's name (not a secret).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl Paths {
    /// Resolve with `env` as the environment (`|k| std::env::var_os(k)` for the real one).
    pub fn resolve(env: impl Fn(&str) -> Option<OsString>) -> Self {
        let get = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        if let Some(h) = get(HOME_VAR) {
            return Self::under(&h);
        }
        let home = get("HOME").unwrap_or_else(|| PathBuf::from("/"));
        let xdg = |var: &str, rel: &str| get(var).unwrap_or_else(|| home.join(rel)).join("lumbridge-mail");
        let runtime = match get("XDG_RUNTIME_DIR") {
            Some(r) => r.join("lumbridge-mail"),
            None => {
                let user = env("USER").or_else(|| env("LOGNAME")).map(|u| u.to_string_lossy().into_owned());
                let tmp = get("TMPDIR").unwrap_or_else(|| PathBuf::from("/tmp"));
                tmp.join(format!("lumbridge-mail-{}", user.unwrap_or_else(|| "user".into())))
            }
        };
        Self {
            config_dir: xdg("XDG_CONFIG_HOME", ".config"),
            data_dir: xdg("XDG_DATA_HOME", ".local/share"),
            cache_dir: xdg("XDG_CACHE_HOME", ".cache"),
            runtime_dir: runtime,
        }
    }

    /// The process environment.
    pub fn from_env() -> Self {
        Self::resolve(|k| std::env::var_os(k))
    }

    /// Everything under one directory, as `LUMBRIDGE_MAIL_HOME` does.
    pub fn under(home: &Path) -> Self {
        Self {
            config_dir: home.join("config"),
            data_dir: home.join("data"),
            cache_dir: home.join("cache"),
            runtime_dir: home.to_path_buf(),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    /// The store (the same path as `lumbridge_mail_store::Store::default_path`).
    pub fn database(&self) -> PathBuf {
        self.data_dir.join("mail.db")
    }
    /// Where the sockets and the person token live: the runtime directory, or a short directory under
    /// `/tmp` when the socket path would be too long for `sun_path` (see the module docs).
    pub fn socket_dir(&self) -> PathBuf {
        let wanted = self.runtime_dir.join("maild.sock");
        if wanted.as_os_str().len() <= MAX_SOCKET_PATH {
            return self.runtime_dir.clone();
        }
        let hash = fnv1a(self.runtime_dir.as_os_str().as_encoded_bytes());
        PathBuf::from("/tmp").join(format!("lumbridge-mail-{hash:016x}"))
    }
    pub fn socket(&self) -> PathBuf {
        self.socket_dir().join("maild.sock")
    }
    /// The app's single-instance socket (`lumbridge-mail` hands a second launch to the first over it).
    pub fn app_socket(&self) -> PathBuf {
        self.socket_dir().join("app.sock")
    }
    /// Held (flock) by the running daemon: one daemon per runtime directory.
    pub fn lock_file(&self) -> PathBuf {
        self.runtime_dir.join("maild.lock")
    }
    /// The per-start secret that marks a connection as a person's (see `rpc::HelloParams`).
    /// Next to the socket, so a client given only a socket path finds it too ([`person_token_for`]).
    pub fn person_token(&self) -> PathBuf {
        person_token_for(&self.socket())
    }
    pub fn attachments_dir(&self) -> PathBuf {
        self.cache_dir.join("attachments")
    }
    /// Written when the daemon was stopped on purpose (`lbmail quit`): clients must not start it
    /// again while it exists. A daemon removes it when it starts. Next to the socket.
    pub fn stopped_marker(&self) -> PathBuf {
        stopped_marker_for(&self.socket())
    }
    /// Present while the daemon is paused (`lbmail pause`). In the data directory, so it survives a
    /// reboot: a daemon started while it exists starts paused, and clients do not start one.
    pub fn paused_file(&self) -> PathBuf {
        self.data_dir.join("paused")
    }
    /// Private directory for ssh control sockets (one shared connection per gog host).
    pub fn ssh_control_dir(&self) -> PathBuf {
        self.socket_dir().join("ssh")
    }
}

/// The "stopped on purpose" marker of the daemon listening at `socket` (see [`Paths::stopped_marker`]).
pub fn stopped_marker_for(socket: &Path) -> PathBuf {
    socket.with_file_name("maild.stopped")
}

/// The person-token file of the daemon listening at `socket` (it sits next to the socket).
pub fn person_token_for(socket: &Path) -> PathBuf {
    socket.with_file_name("person.token")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| m.get(k).map(OsString::from)
    }

    #[test]
    fn home_moves_everything() {
        let p = Paths::resolve(env(&[(HOME_VAR, "/t/h"), ("HOME", "/home/x")]));
        assert_eq!(p.config_file(), PathBuf::from("/t/h/config/config.toml"));
        assert_eq!(p.database(), PathBuf::from("/t/h/data/mail.db"));
        assert_eq!(p.socket(), PathBuf::from("/t/h/maild.sock"));
        assert_eq!(p.attachments_dir(), PathBuf::from("/t/h/cache/attachments"));
        assert_eq!(p.stopped_marker(), PathBuf::from("/t/h/maild.stopped"));
        assert_eq!(stopped_marker_for(&p.socket()), p.stopped_marker());
        assert_eq!(p.paused_file(), PathBuf::from("/t/h/data/paused"));
        assert_eq!(p.ssh_control_dir(), PathBuf::from("/t/h/ssh"));
    }

    #[test]
    fn xdg_defaults() {
        let p = Paths::resolve(env(&[("HOME", "/home/x"), ("XDG_RUNTIME_DIR", "/run/user/7")]));
        assert_eq!(p.config_dir, PathBuf::from("/home/x/.config/lumbridge-mail"));
        assert_eq!(p.data_dir, PathBuf::from("/home/x/.local/share/lumbridge-mail"));
        assert_eq!(p.cache_dir, PathBuf::from("/home/x/.cache/lumbridge-mail"));
        assert_eq!(p.socket(), PathBuf::from("/run/user/7/lumbridge-mail/maild.sock"));
        let p = Paths::resolve(env(&[("HOME", "/home/x"), ("USER", "sam"), ("XDG_DATA_HOME", "/d")]));
        assert_eq!(p.runtime_dir, PathBuf::from("/tmp/lumbridge-mail-sam"));
        assert_eq!(p.data_dir, PathBuf::from("/d/lumbridge-mail"));
        assert_eq!(p.app_socket(), PathBuf::from("/tmp/lumbridge-mail-sam/app.sock"));
        assert_eq!(p.person_token(), PathBuf::from("/tmp/lumbridge-mail-sam/person.token"));
    }

    #[test]
    fn long_socket_paths_fall_back_to_a_short_stable_directory() {
        // A macOS-style temp dir, deep enough that `maild.sock` would overflow sun_path.
        let deep = format!("/var/folders/zz/{}/T/.tmpAbC123/lumbridge-mail-test-home", "x".repeat(60));
        let p = Paths::resolve(env(&[(HOME_VAR, &deep)]));
        assert!(PathBuf::from(&deep).join("maild.sock").as_os_str().len() > MAX_SOCKET_PATH);
        let sock = p.socket();
        assert!(sock.as_os_str().len() <= MAX_SOCKET_PATH, "{}", sock.display());
        assert!(sock.starts_with("/tmp"), "{}", sock.display());
        assert_eq!(sock, Paths::resolve(env(&[(HOME_VAR, &deep)])).socket(), "stable: every client computes the same");
        assert_ne!(sock, Paths::resolve(env(&[(HOME_VAR, &format!("{deep}2"))])).socket(), "different homes never share");
        assert_eq!(p.person_token().parent(), sock.parent(), "the token sits next to the socket");
        assert_eq!(person_token_for(&sock), p.person_token());
        // The store and config stay where they were asked to be.
        assert_eq!(p.config_file(), PathBuf::from(&deep).join("config/config.toml"));
        // Short paths are used as they are.
        assert_eq!(Paths::resolve(env(&[(HOME_VAR, "/t/h")])).socket_dir(), PathBuf::from("/t/h"));
    }
}
