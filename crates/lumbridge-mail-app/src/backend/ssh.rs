//! A daemon on another machine, reached over ssh (docs/ARCHITECTURE.md, "Remote daemon"; the command
//! lines are `lumbridge_mail_core::remote`).
//!
//! The link runs `ssh -T <host> lbmail pipe` and speaks the same JSON-RPC over the child's stdin and
//! stdout as over the local socket. Before each connection the app (a person) fetches the daemon's
//! person token with `ssh <host> lbmail person-token`; it lives in memory only, for that connection's
//! `hello`. The pipe itself adds nothing, so an agent that can ssh to the server is still an agent.
//!
//! In remote mode the app never starts or stops a daemon: the server's systemd unit owns it.

use crate::cli::Args;
use lumbridge_mail_core::remote::{ClientConfig, Remote, RemoteSettings, ENV_VAR};
use parking_lot::Mutex;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// How long fetching the person token may take, ssh connect included.
pub const TOKEN_TIMEOUT: Duration = Duration::from_secs(20);

/// What the window talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Demo,
    /// This machine's daemon; `autostart`: start `lumbridge-maild` when nothing listens.
    Local { socket: PathBuf, autostart: bool },
    /// A daemon on another machine. Never started or stopped from here.
    Remote(Remote),
}

/// The `[daemon]` remote keys of `config.toml` in `config_dir` (defaults when there is no file).
pub fn settings(config_dir: &Path) -> Result<RemoteSettings, String> {
    let file = config_dir.join("config.toml");
    match std::fs::read_to_string(&file) {
        Ok(t) => toml::from_str::<ClientConfig>(&t).map(|c| c.daemon).map_err(|e| format!("{}: {e}", file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RemoteSettings::default()),
        Err(e) => Err(format!("{}: {e}", file.display())),
    }
}

/// Which daemon: `--demo`; an explicit `--socket` (never started); `--remote`, else `LBMAIL_REMOTE`, else
/// `[daemon] remote`; else this machine's (started unless `--no-start`).
pub fn choose(args: &Args, env: Option<&str>, settings: &RemoteSettings, default_socket: PathBuf) -> Result<Target, String> {
    if args.demo {
        return Ok(Target::Demo);
    }
    if let Some(s) = &args.socket {
        return Ok(Target::Local { socket: s.clone(), autostart: false });
    }
    match Remote::choose(args.remote.as_deref().or(env), settings).map_err(|e| e.to_string())? {
        Some(r) => Ok(Target::Remote(r)),
        None => Ok(Target::Local { socket: default_socket, autostart: !args.no_start }),
    }
}

/// [`choose`] with this process's environment and config file.
pub fn target(args: &Args) -> Result<Target, String> {
    let env = std::env::var(ENV_VAR).ok();
    // A broken config file only matters when it would decide (no flag, no environment variable).
    let s = if args.demo || args.socket.is_some() || args.remote.is_some() || env.is_some() {
        settings(&crate::paths::config_dir()).unwrap_or_default()
    } else {
        settings(&crate::paths::config_dir())?
    };
    choose(args, env.as_deref(), &s, crate::paths::socket())
}

/// A directory for the shared ssh connection's socket: ours, 0700, not a link, and short enough for a
/// socket path. None: every ssh connects on its own (never use a directory someone else could plant a
/// control socket in).
pub fn control_dir() -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let dir = crate::paths::current().ssh_control_dir();
    crate::paths::ensure_private_dir(&dir).ok()?;
    let meta = std::fs::symlink_metadata(&dir).ok()?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    if !meta.is_dir() || meta.uid() != me || meta.permissions().mode() & 0o077 != 0 {
        return None;
    }
    (dir.as_os_str().len() <= lumbridge_mail_core::remote::MAX_CONTROL_DIR).then_some(dir)
}

fn command(argv: &[String]) -> Command {
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..]);
    c
}

/// The tail of a child's stderr, read on its own thread (never waited for: a shared ssh master may keep
/// it open).
#[derive(Clone, Default)]
pub struct Stderr(Arc<Mutex<String>>);

impl Stderr {
    fn collect(mut r: impl Read + Send + 'static) -> Stderr {
        let s = Stderr::default();
        let keep = s.0.clone();
        let _ = std::thread::Builder::new().name("ssh-stderr".into()).spawn(move || {
            let mut buf = [0u8; 1024];
            while let Ok(n) = r.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut k = keep.lock();
                k.push_str(&String::from_utf8_lossy(&buf[..n]));
                if k.len() > 2048 {
                    let cut = (k.len() - 2048..k.len()).find(|i| k.is_char_boundary(*i)).unwrap_or(k.len());
                    k.drain(..cut);
                }
            }
        });
        s
    }

    pub fn text(&self) -> String {
        self.0.lock().split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

/// `ssh <host> lbmail person-token`. The error is what ssh or lbmail said, for the window.
pub fn fetch_token(remote: &Remote, control: Option<&Path>) -> Result<String, String> {
    let mut child = command(&remote.token_command(control))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", remote.ssh))?;
    let err = Stderr::collect(child.stderr.take().expect("piped"));
    let mut out = child.stdout.take().expect("piped");
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new().name("ssh-token".into()).spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        let _ = tx.send(s);
    });
    let token = match rx.recv_timeout(TOKEN_TIMEOUT) {
        Ok(t) => t,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("no answer in {} s", TOKEN_TIMEOUT.as_secs()));
        }
    };
    let status = child.wait();
    let token = token.trim().to_owned();
    if status.as_ref().is_ok_and(|s| s.success()) && !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Ok(token);
    }
    std::thread::sleep(Duration::from_millis(50));
    let said = err.text();
    Err(if said.is_empty() { format!("no person token ({})", status.map(|s| s.to_string()).unwrap_or_default()) } else { said })
}

/// One running `ssh <host> lbmail pipe`.
pub struct Pipe {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    /// Ends the connection: kills ssh (the reader then sees the end) and reaps it.
    pub close: Arc<dyn Fn() + Send + Sync>,
    pub stderr: Stderr,
}

pub fn spawn_pipe(remote: &Remote, control: Option<&Path>) -> Result<Pipe, String> {
    let mut child: Child = command(&remote.pipe_command(control))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run {}: {e}", remote.ssh))?;
    let writer = child.stdin.take().expect("piped");
    let reader = child.stdout.take().expect("piped");
    let stderr = Stderr::collect(child.stderr.take().expect("piped"));
    let child = Arc::new(Mutex::new(Some(child)));
    let close: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        if let Some(mut c) = child.lock().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    });
    Ok(Pipe { reader: Box::new(reader), writer: Box::new(writer), close, stderr })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(f: impl FnOnce(&mut Args)) -> Args {
        let mut a = match crate::cli::parse(Vec::<String>::new(), 10).unwrap() {
            crate::cli::Parsed::Run(a) => a,
            crate::cli::Parsed::Help => unreachable!(),
        };
        f(&mut a);
        a
    }

    fn remote_in_file() -> RemoteSettings {
        RemoteSettings { remote: Some("homeserver".into()), ..Default::default() }
    }

    #[test]
    fn a_remote_daemon_is_never_started_here() {
        let sock = PathBuf::from("/run/user/1/lumbridge-mail/maild.sock");
        let t = choose(&args(|_| {}), None, &remote_in_file(), sock.clone()).unwrap();
        assert!(matches!(&t, Target::Remote(r) if r.host == "homeserver"), "{t:?}");
        let t = choose(&args(|a| a.remote = Some("other".into())), Some("env"), &remote_in_file(), sock.clone()).unwrap();
        assert!(matches!(&t, Target::Remote(r) if r.host == "other"), "the flag wins: {t:?}");
        let t = choose(&args(|_| {}), Some("env"), &RemoteSettings::default(), sock.clone()).unwrap();
        assert!(matches!(&t, Target::Remote(r) if r.host == "env"), "then the environment: {t:?}");
        // Local only when asked for (or nothing names a remote); only then may the app start a daemon.
        assert_eq!(choose(&args(|a| a.remote = Some("local".into())), None, &remote_in_file(), sock.clone()).unwrap(),
                   Target::Local { socket: sock.clone(), autostart: true });
        assert_eq!(choose(&args(|a| a.no_start = true), None, &RemoteSettings::default(), sock.clone()).unwrap(),
                   Target::Local { socket: sock.clone(), autostart: false });
        assert_eq!(choose(&args(|a| a.socket = Some("/x.sock".into())), None, &remote_in_file(), sock.clone()).unwrap(),
                   Target::Local { socket: "/x.sock".into(), autostart: false });
        assert_eq!(choose(&args(|a| a.demo = true), None, &remote_in_file(), sock.clone()).unwrap(), Target::Demo);
        assert!(choose(&args(|a| a.remote = Some("-oProxyCommand=x".into())), None, &RemoteSettings::default(), sock).is_err());
    }

    #[test]
    fn the_config_file_is_read_for_the_daemon_section_only() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(settings(dir.path()).unwrap(), RemoteSettings::default());
        std::fs::write(dir.path().join("config.toml"), "[daemon]\nremote = \"homeserver\"\nsync_interval_s = 5\n[[accounts]]\nid = \"x\"\n").unwrap();
        assert_eq!(settings(dir.path()).unwrap().remote.as_deref(), Some("homeserver"));
        std::fs::write(dir.path().join("config.toml"), "[daemon\n").unwrap();
        assert!(settings(dir.path()).is_err());
    }

    #[test]
    fn a_failing_ssh_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("ssh");
        std::fs::write(&ssh, "#!/bin/sh\necho 'ssh: connect to host homeserver port 22: No route to host' >&2\nexit 255\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut r = Remote::new("homeserver").unwrap();
        r.ssh = ssh.display().to_string();
        let err = fetch_token(&r, None).unwrap_err();
        assert!(err.contains("No route to host"), "{err}");
        r.ssh = dir.path().join("no-such-ssh").display().to_string();
        assert!(fetch_token(&r, None).unwrap_err().contains("could not run"));
    }
}
