//! Reaching a daemon on another machine over ssh (docs/ARCHITECTURE.md, "Remote daemon"). Pure: what
//! the clients run and how they choose, not running it.
//!
//! A client (the app, `lbmail`) in remote mode runs `ssh -T <host> lbmail pipe` and speaks the same
//! line-delimited JSON-RPC over that process's stdin/stdout as it would over the Unix socket. `lbmail pipe`
//! on the server only copies bytes between its stdio and the daemon's socket: it never reads or adds the
//! person token. A client that should be a person fetches the token itself with a second command
//! (`lbmail person-token`, see [`Remote::token_command`]) and presents it in its own `hello`.
//!
//! Configuration, client side, in `config.toml` (the daemon ignores these keys):
//!
//! ```toml
//! [daemon]
//! remote = "homeserver"              # an ssh destination (a ~/.ssh/config alias); unset = the local daemon
//! # remote_socket = "/run/user/1000/lumbridge-mail/maild.sock"   # default: the server's own default
//! # remote_lbmail = ".local/bin/lbmail"   # lbmail on the server; relative = from the ssh user's home
//! # ssh = "ssh"                           # the ssh program here
//! # remote_multiplex = true               # share one ssh connection (ControlMaster) for 60 s
//! ```
//!
//! `LBMAIL_REMOTE=<host>` (or `lbmail --remote <host>`, `lumbridge-mail --remote <host>`) overrides the
//! file; `local` as the host (`--remote local`, `LBMAIL_REMOTE=local`) forces the local daemon.

use serde::Deserialize;
use std::path::Path;

/// The environment variable that picks the daemon: a host, or `local`.
pub const ENV_VAR: &str = "LBMAIL_REMOTE";
/// Where `deploy/install-server.sh` puts `lbmail`, relative to the ssh user's home (where ssh starts).
pub const DEFAULT_LBMAIL: &str = ".local/bin/lbmail";
/// How long a shared ssh connection outlives its last use.
pub const PERSIST_S: u32 = 60;
/// The longest directory for the shared connection's socket: ssh makes `<dir>/r-<40 hex digits>` plus a
/// 17-character temporary suffix while it starts the master, and all of it must fit macOS's 104-byte
/// `sun_path` (with the NUL). Longer: no sharing, every ssh connects on its own.
pub const MAX_CONTROL_DIR: usize = 104 - 1 - "/r-".len() - 40 - 17;

/// The `[daemon]` keys a client reads. Unknown keys (the daemon's own) are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RemoteSettings {
    pub remote: Option<String>,
    pub remote_socket: Option<String>,
    pub remote_lbmail: Option<String>,
    pub ssh: Option<String>,
    pub remote_multiplex: Option<bool>,
}

/// The part of `config.toml` a client reads: `toml::from_str::<ClientConfig>(text)`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ClientConfig {
    pub daemon: RemoteSettings,
}

/// A daemon on another machine, reached with `ssh <host>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// The ssh destination (`host`, `user@host`, or a `~/.ssh/config` alias).
    pub host: String,
    /// The daemon's socket on the server (None: the server's default, from its own environment).
    pub socket: Option<String>,
    /// `lbmail` on the server. Relative paths start at the ssh user's home; `~/` is expanded there.
    pub lbmail: String,
    /// The ssh program on this machine.
    pub ssh: String,
    /// Share one ssh connection between the pipe and the token fetch (ControlMaster).
    pub multiplex: bool,
}

impl Remote {
    /// A remote with the defaults. Refuses a destination ssh could read as an option or that has spaces
    /// or control characters (it comes from a config file or an environment variable).
    pub fn new(host: &str) -> crate::Result<Remote> {
        let host = host.trim();
        if host.is_empty() || host.starts_with('-') || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(crate::Error::Invalid(format!("{host:?} is not an ssh destination")));
        }
        Ok(Remote { host: host.to_owned(), socket: None, lbmail: DEFAULT_LBMAIL.into(), ssh: "ssh".into(), multiplex: true })
    }

    /// Which daemon to use: `choice` (a command-line flag, else `LBMAIL_REMOTE`) wins over the file;
    /// `""` or `local` there means the local daemon. None = local.
    pub fn choose(choice: Option<&str>, settings: &RemoteSettings) -> crate::Result<Option<Remote>> {
        let host = match choice.map(str::trim) {
            Some("") | Some("local") => return Ok(None),
            Some(h) => h.to_owned(),
            None => match settings.remote.as_deref().map(str::trim) {
                None | Some("") | Some("local") => return Ok(None),
                Some(h) => h.to_owned(),
            },
        };
        let mut r = Remote::new(&host)?;
        if let Some(s) = settings.remote_socket.as_deref().filter(|s| !s.trim().is_empty()) {
            r.socket = Some(s.trim().to_owned());
        }
        if let Some(l) = settings.remote_lbmail.as_deref().filter(|s| !s.trim().is_empty()) {
            r.lbmail = l.trim().to_owned();
        }
        if let Some(s) = settings.ssh.as_deref().filter(|s| !s.trim().is_empty()) {
            r.ssh = s.trim().to_owned();
        }
        if let Some(m) = settings.remote_multiplex {
            r.multiplex = m;
        }
        Ok(Some(r))
    }

    /// What clients show: `remote:<host>`.
    pub fn label(&self) -> String {
        format!("remote:{}", self.host)
    }

    /// The command line run on the server for `lbmail <args>`, quoted for the remote shell (ssh hands
    /// the server one string). `~/` at the start of the binary stays unquoted so the shell expands it.
    pub fn remote_command(&self, args: &[&str]) -> String {
        let bin = match self.lbmail.strip_prefix("~/") {
            Some(rest) => format!("~/{}", shell_quote(rest)),
            None => shell_quote(&self.lbmail),
        };
        let mut cmd = bin;
        for a in args {
            cmd.push(' ');
            cmd.push_str(&shell_quote(a));
        }
        if let Some(s) = &self.socket {
            cmd.push_str(" --socket ");
            cmd.push_str(&shell_quote(s));
        }
        cmd
    }

    /// The full ssh command line (program first) for `lbmail <args>` on the server. `control_dir` is a
    /// private directory for the shared connection's socket (used only with `multiplex`).
    pub fn ssh_command(&self, control_dir: Option<&Path>, args: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = vec![self.ssh.clone(), "-T".into(), "-e".into(), "none".into()];
        for o in ["BatchMode=yes", "ConnectTimeout=8", "ServerAliveInterval=10", "ServerAliveCountMax=3", "ClearAllForwardings=yes"] {
            v.push("-o".into());
            v.push(o.into());
        }
        if let (true, Some(dir)) = (self.multiplex, control_dir) {
            v.push("-o".into());
            v.push("ControlMaster=auto".into());
            v.push("-o".into());
            // %C: a hash of the destination, so the path stays short enough for a socket.
            v.push(format!("ControlPath={}/r-%C", dir.display()));
            v.push("-o".into());
            v.push(format!("ControlPersist={PERSIST_S}"));
        }
        v.push("--".into());
        v.push(self.host.clone());
        v.push(self.remote_command(args));
        v
    }

    /// `ssh <host> lbmail pipe`: stdio bridged to the daemon's socket. Never makes a person.
    pub fn pipe_command(&self, control_dir: Option<&Path>) -> Vec<String> {
        self.ssh_command(control_dir, &["pipe"])
    }

    /// `ssh <host> lbmail person-token`: prints the running daemon's person token, for a client that is
    /// a person (the app; `lbmail` at a terminal). Agents never run it.
    pub fn token_command(&self, control_dir: Option<&Path>) -> Vec<String> {
        self.ssh_command(control_dir, &["person-token"])
    }
}

/// `s` as one word for a POSIX shell (and fish): bare when it is plainly safe, else in single quotes.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | '@' | '%' | '+' | ',')) {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(remote: Option<&str>) -> RemoteSettings {
        RemoteSettings { remote: remote.map(Into::into), ..Default::default() }
    }

    #[test]
    fn the_flag_beats_the_file_and_local_means_local() {
        assert_eq!(Remote::choose(None, &settings(None)).unwrap(), None);
        assert_eq!(Remote::choose(None, &settings(Some("homeserver"))).unwrap().unwrap().host, "homeserver");
        assert_eq!(Remote::choose(Some("other"), &settings(Some("homeserver"))).unwrap().unwrap().host, "other");
        assert_eq!(Remote::choose(Some("local"), &settings(Some("homeserver"))).unwrap(), None);
        assert_eq!(Remote::choose(Some(""), &settings(Some("homeserver"))).unwrap(), None);
        assert_eq!(Remote::choose(None, &settings(Some(" "))).unwrap(), None);
    }

    #[test]
    fn destinations_that_look_like_options_are_refused() {
        for bad in ["-oProxyCommand=sh", "two words", "a\nb", ""] {
            assert!(Remote::new(bad).is_err(), "{bad:?}");
        }
        assert!(Remote::new("sam@homeserver").is_ok());
        assert!(Remote::choose(Some("-x"), &RemoteSettings::default()).is_err());
    }

    #[test]
    fn the_settings_fill_in_the_rest() {
        let s = RemoteSettings {
            remote: Some("homeserver".into()),
            remote_socket: Some("/run/user/7/lumbridge-mail/maild.sock".into()),
            remote_lbmail: Some("/opt/lb mail/lbmail".into()),
            ssh: Some("/usr/bin/ssh".into()),
            remote_multiplex: Some(false),
        };
        let r = Remote::choose(None, &s).unwrap().unwrap();
        assert_eq!(r.label(), "remote:homeserver");
        let cmd = r.pipe_command(Some(Path::new("/run/user/1/lumbridge-mail/ssh")));
        assert_eq!(cmd[0], "/usr/bin/ssh");
        assert!(!cmd.iter().any(|a| a.starts_with("ControlPath")), "multiplex off: {cmd:?}");
        assert_eq!(cmd.last().unwrap(), "'/opt/lb mail/lbmail' pipe --socket /run/user/7/lumbridge-mail/maild.sock");
        assert_eq!(&cmd[cmd.len() - 3..cmd.len() - 1], ["--", "homeserver"]);
    }

    #[test]
    fn the_ssh_line_is_batch_mode_without_a_tty_and_shares_a_connection() {
        let r = Remote::new("homeserver").unwrap();
        let cmd = r.pipe_command(Some(Path::new("/run/user/1/lumbridge-mail/ssh")));
        let line = cmd.join(" ");
        for want in ["ssh -T -e none", "BatchMode=yes", "ServerAliveInterval=", "ConnectTimeout=", "ControlMaster=auto",
                     "ControlPath=/run/user/1/lumbridge-mail/ssh/r-%C", "ControlPersist=60", "-- homeserver .local/bin/lbmail pipe"] {
            assert!(line.contains(want), "{want}: {line}");
        }
        assert!(!line.contains("person-token") && !line.contains("--socket"), "{line}");
        assert!(r.token_command(None).last().unwrap().ends_with("lbmail person-token"));
        assert!(!r.token_command(None).iter().any(|a| a.starts_with("ControlPath")), "no directory, no sharing");
    }

    #[test]
    fn the_control_socket_fits_while_ssh_creates_it() {
        assert_eq!(MAX_CONTROL_DIR, 43);
        assert!("/run/user/1000/lumbridge-mail/ssh".len() <= MAX_CONTROL_DIR);
        assert!("/tmp/lumbridge-mail-0123456789abcdef/ssh".len() <= MAX_CONTROL_DIR, "the short /tmp fallback fits");
    }

    #[test]
    fn quoting_survives_the_remote_shell() {
        assert_eq!(shell_quote("plain-path/x.sock"), "plain-path/x.sock");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
        assert_eq!(shell_quote(""), "''");
        let mut r = Remote::new("h").unwrap();
        r.lbmail = "~/bin/lb mail".into();
        assert_eq!(r.remote_command(&["pipe"]), "~/'bin/lb mail' pipe");
    }

    #[test]
    fn config_files_parse_with_the_daemons_own_keys_around() {
        let c: ClientConfig = toml_like(r#"{"daemon": {"remote": "homeserver", "sync_interval_s": 300}, "accounts": []}"#);
        assert_eq!(c.daemon.remote.as_deref(), Some("homeserver"));
        let c: ClientConfig = toml_like("{}");
        assert_eq!(c, ClientConfig::default());
    }

    /// Core has no toml; the shape is the same through serde_json.
    fn toml_like(json: &str) -> ClientConfig {
        serde_json::from_str(json).unwrap()
    }
}
