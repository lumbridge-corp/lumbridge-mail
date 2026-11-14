//! The remote daemon over ssh, both ends (docs/ARCHITECTURE.md, "Remote daemon"; the pure parts are
//! `lumbridge_mail_core::remote`).
//!
//! - Server end: [`pipe`] (`lbmail pipe`) copies bytes between its stdin/stdout and the daemon's socket.
//!   It never reads the person token, never adds one, never looks inside a line: whoever is on the
//!   other end of ssh is an agent unless its own `hello` carries the token.
//! - Client end: [`connect`] (what `lbmail` does in remote mode) runs `ssh -T <host> lbmail pipe` and
//!   speaks JSON-RPC over the child's stdio. A person client first fetches the token with
//!   `ssh <host> lbmail person-token` ([`fetch_token`]) and keeps it in memory only.

use crate::client::Client;
use lumbridge_mail_core::paths::Paths;
use lumbridge_mail_core::remote::{ClientConfig, ENV_VAR, Remote, RemoteSettings};
use lumbridge_mail_core::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// How long fetching the person token may take (ssh connect included).
pub const TOKEN_TIMEOUT: Duration = Duration::from_secs(20);
/// Most of a failed ssh's stderr kept for the error message.
const STDERR_KEEP: usize = 2048;

/// The wait before reconnect attempt `n` (0-based), the same steps as the app's link.
pub fn backoff(attempt: u32) -> Duration {
    const STEPS_MS: [u64; 8] = [250, 500, 1_000, 2_000, 4_000, 8_000, 15_000, 30_000];
    Duration::from_millis(STEPS_MS[(attempt as usize).min(STEPS_MS.len() - 1)])
}

/// The `[daemon]` remote keys of this machine's `config.toml` (none when there is no file).
pub fn settings(paths: &Paths) -> Result<RemoteSettings> {
    let file = paths.config_file();
    match std::fs::read_to_string(&file) {
        Ok(t) => toml::from_str::<ClientConfig>(&t).map(|c| c.daemon).map_err(|e| Error::Invalid(format!("{}: {e}", file.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RemoteSettings::default()),
        Err(e) => Err(Error::Other(format!("{}: {e}", file.display()))),
    }
}

/// Which daemon a client uses: `flag` (`--remote`), else `LBMAIL_REMOTE`, else `config.toml`.
pub fn choose(flag: Option<&str>, paths: &Paths) -> Result<Option<Remote>> {
    let env = std::env::var(ENV_VAR).ok();
    Remote::choose(flag.or(env.as_deref()), &settings(paths)?)
}

/// A private directory for the shared ssh connection's socket, or None (then every call connects).
pub fn control_dir(paths: &Paths) -> Option<PathBuf> {
    let dir = paths.ssh_control_dir();
    crate::fsutil::private_dir(&dir).ok()?;
    (dir.as_os_str().len() <= lumbridge_mail_core::remote::MAX_CONTROL_DIR).then_some(dir)
}

fn command(argv: &[String]) -> Command {
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..]);
    c
}

/// Keeps the tail of a child's stderr (read in the background; never waited for, since a shared ssh
/// master may hold it open).
#[derive(Clone, Default)]
pub struct Stderr(Arc<Mutex<String>>);

impl Stderr {
    fn collect(r: impl AsyncRead + Unpin + Send + 'static) -> Stderr {
        let s = Stderr::default();
        let keep = s.0.clone();
        tokio::spawn(async move {
            let mut r = r;
            let mut buf = [0u8; 1024];
            while let Ok(n) = r.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut k = keep.lock().unwrap_or_else(|e| e.into_inner());
                k.push_str(&String::from_utf8_lossy(&buf[..n]));
                if k.len() > STDERR_KEEP {
                    let cut = k.len() - STDERR_KEEP;
                    let cut = (cut..k.len()).find(|i| k.is_char_boundary(*i)).unwrap_or(k.len());
                    k.drain(..cut);
                }
            }
        });
        s
    }

    /// What it said so far, on one line.
    pub fn text(&self) -> String {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

/// `ssh <host> lbmail person-token`: the running daemon's person token. Never written anywhere here.
pub async fn fetch_token(remote: &Remote, control_dir: Option<&Path>) -> Result<String> {
    let mut cmd = command(&remote.token_command(control_dir));
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| Error::Offline(format!("{}: could not run {}: {e}", remote.label(), remote.ssh)))?;
    let err = Stderr::collect(child.stderr.take().expect("piped"));
    let mut out = child.stdout.take().expect("piped");
    let run = async {
        let mut token = String::new();
        let _ = out.read_to_string(&mut token).await;
        let status = child.wait().await;
        (token, status)
    };
    let (token, status) = tokio::time::timeout(TOKEN_TIMEOUT, run)
        .await
        .map_err(|_| Error::Offline(format!("{}: no answer in {} s", remote.label(), TOKEN_TIMEOUT.as_secs())))?;
    let ok = status.as_ref().is_ok_and(|s| s.success());
    let token = token.trim().to_owned();
    if ok && !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Ok(token);
    }
    // Give the reader a moment to catch the last words.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let why = err.text();
    let code = status.ok().and_then(|s| s.code());
    Err(match code {
        // lbmail's own exit codes (cli::run): refused, offline.
        Some(4) => Error::Refused(format!("{}: {why}", remote.label())),
        _ => Error::Offline(format!("{}: {}", remote.label(), if why.is_empty() { "no person token".into() } else { why })),
    })
}

/// Connects to the daemon behind `remote` and says hello; with `person`, as a person (the token is
/// fetched first, and once more if the daemon restarted in between).
pub async fn connect(remote: &Remote, control_dir: Option<&Path>, person: bool, client: &str) -> Result<Client> {
    let token = if person { Some(fetch_token(remote, control_dir).await?) } else { None };
    let mut cmd = command(&remote.pipe_command(control_dir));
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| Error::Offline(format!("{}: could not run {}: {e}", remote.label(), remote.ssh)))?;
    let stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let err = Stderr::collect(child.stderr.take().expect("piped"));
    let mut c = Client::over(Box::new(tokio::io::join(stdout, stdin)), remote.label(), Some((child, err)));
    match c.hello(token, client).await {
        Err(Error::Refused(_)) if person => {
            // A token from a daemon that has since restarted: ask again (same connection).
            let t = fetch_token(remote, control_dir).await?;
            c.hello(Some(t), client).await?;
        }
        r => r?,
    }
    Ok(c)
}

/// `lbmail pipe`: copies stdin to the daemon's socket and the socket to stdout until either side
/// closes. Bytes only: the person token is never read or added here.
pub async fn pipe(socket: &Path) -> Result<()> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| Error::Offline(format!("no daemon at {} ({e}); start it there: systemctl --user start lumbridge-maild", socket.display())))?;
    let peer = stream.peer_cred().map_err(|e| Error::Other(format!("{}: peer credentials: {e}", socket.display())))?;
    if peer.uid() != crate::fsutil::euid() {
        return Err(Error::Refused(format!("{} is served by another user (uid {})", socket.display(), peer.uid())));
    }
    let (mut from_daemon, mut to_daemon) = stream.into_split();
    let up = async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = stdin.read(&mut buf).await {
            if n == 0 || to_daemon.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
        // The client is done: the daemon answers what it has, then closes.
        let _ = to_daemon.shutdown().await;
    };
    let down = async move {
        let mut stdout = tokio::io::stdout();
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = from_daemon.read(&mut buf).await {
            if n == 0 || stdout.write_all(&buf[..n]).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    };
    tokio::pin!(up, down);
    tokio::select! {
        _ = &mut down => {}
        _ = &mut up => { let _ = tokio::time::timeout(Duration::from_secs(10), &mut down).await; }
    }
    Ok(())
}

/// `lbmail person-token`: prints the running daemon's token, into a pipe only (a remote person client
/// reads it over ssh). The daemon must be up and our own user's, so a stale file is never handed out.
pub async fn print_person_token(socket: &Path) -> Result<()> {
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() {
        return Err(Error::Refused(
            "the person token is never shown on a screen; the app (or lbmail at a terminal) on another machine reads it over ssh".into(),
        ));
    }
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| Error::Offline(format!("no daemon at {} ({e})", socket.display())))?;
    let peer = stream.peer_cred().map_err(|e| Error::Other(format!("{}: peer credentials: {e}", socket.display())))?;
    if peer.uid() != crate::fsutil::euid() {
        return Err(Error::Refused(format!("{} is served by another user (uid {})", socket.display(), peer.uid())));
    }
    drop(stream);
    let path = lumbridge_mail_core::paths::person_token_for(socket);
    let token = Client::read_person_token(&path).ok_or_else(|| Error::Offline(format!("no person token at {}", path.display())))?;
    let mut out = tokio::io::stdout();
    out.write_all(format!("{token}\n").as_bytes()).await.map_err(|e| Error::Other(e.to_string()))?;
    out.flush().await.map_err(|e| Error::Other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(0), Duration::from_millis(250));
        assert!(backoff(4) > backoff(3));
        assert_eq!(backoff(99), Duration::from_secs(30));
    }

    #[test]
    fn the_config_file_names_the_remote() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        assert_eq!(settings(&paths).unwrap(), RemoteSettings::default(), "no file: local");
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.config_file(), "[daemon]\nsync_interval_s = 30\nremote = \"homeserver\"\nremote_multiplex = false\n\n[ai]\nenabled = false\n").unwrap();
        let s = settings(&paths).unwrap();
        assert_eq!(s.remote.as_deref(), Some("homeserver"));
        let r = Remote::choose(None, &s).unwrap().unwrap();
        assert!(!r.multiplex);
        // The daemon reads the same file and ignores the client's keys.
        assert!(crate::Config::load(&paths.config_file()).is_ok());
        std::fs::write(paths.config_file(), "[daemon]\nremote = 5\n").unwrap();
        assert!(settings(&paths).is_err());
    }
}
