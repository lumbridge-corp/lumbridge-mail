//! One window per user: the first `lumbridge-mail` listens on a Unix socket next to the daemon's
//! (`app.sock`, see [`crate::paths`]); a second one hands its request (a `mailto:` URL, or just "come to
//! the front") to the first over that socket and exits.
//!
//! The protocol is one line each way: `activate` or `open <url>`, answered by `ok`. A socket file nobody
//! answers on is stale (the first instance crashed) and is replaced.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Bring the window to the front.
    Activate,
    /// Open a URL (a `mailto:`) in the running app.
    Open(String),
}

impl Request {
    fn line(&self) -> String {
        match self {
            Request::Activate => "activate\n".into(),
            Request::Open(url) => format!("open {}\n", url.replace(['\r', '\n'], "")),
        }
    }

    fn parse(line: &str) -> Option<Self> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line == "activate" {
            Some(Request::Activate)
        } else {
            line.strip_prefix("open ").map(|u| Request::Open(u.to_string()))
        }
    }
}

/// The first instance: holds the socket until dropped.
pub struct Primary {
    listener: Option<UnixListener>,
    path: PathBuf,
}

pub enum Acquired {
    Primary(Primary),
    /// Another instance took the request.
    Handed,
}

const MAX_LINE: u64 = 64 * 1024;

fn hand_over(path: &Path, req: &Request) -> std::io::Result<()> {
    let mut s = UnixStream::connect(path)?;
    s.set_read_timeout(Some(Duration::from_secs(3)))?;
    s.set_write_timeout(Some(Duration::from_secs(3)))?;
    s.write_all(req.line().as_bytes())?;
    let mut ack = String::new();
    BufReader::new(s.take(16)).read_line(&mut ack)?;
    if ack.trim() == "ok" { Ok(()) } else { Err(std::io::Error::other("the running instance did not answer")) }
}

/// Become the primary instance at `path`, or hand `req` to the one already running.
pub fn acquire(path: &Path, req: &Request) -> std::io::Result<Acquired> {
    for _ in 0..2 {
        if hand_over(path, req).is_ok() {
            return Ok(Acquired::Handed);
        }
        if let Some(dir) = path.parent() {
            crate::paths::ensure_private_dir(dir)?;
        }
        // Nobody answered: a leftover file from a crash, or nothing at all.
        let _ = std::fs::remove_file(path);
        match UnixListener::bind(path) {
            Ok(listener) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                }
                return Ok(Acquired::Primary(Primary { listener: Some(listener), path: path.to_owned() }));
            }
            // Another instance bound it between our two steps: hand over to it instead.
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("could not become or reach the running instance"))
}

impl Primary {
    /// Answer other instances on a thread; their requests arrive on the returned channel.
    pub fn serve(&mut self) -> async_channel::Receiver<Request> {
        let (tx, rx) = async_channel::unbounded();
        if let Some(listener) = self.listener.take() {
            let _ = std::thread::Builder::new().name("lumbridge-mail-instance".into()).spawn(move || {
                for conn in listener.incoming() {
                    let Ok(mut conn) = conn else { continue };
                    let _ = conn.set_read_timeout(Some(Duration::from_secs(3)));
                    let mut line = String::new();
                    let Ok(reader) = conn.try_clone() else { continue };
                    if BufReader::new(reader.take(MAX_LINE)).read_line(&mut line).is_err() {
                        continue;
                    }
                    if let Some(req) = Request::parse(&line) {
                        let _ = conn.write_all(b"ok\n");
                        if tx.send_blocking(req).is_err() {
                            break;
                        }
                    }
                }
            });
        }
        rx
    }
}

impl Drop for Primary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_second_instance_hands_its_mailto_to_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rt/app.sock");
        let Acquired::Primary(mut first) = acquire(&path, &Request::Activate).unwrap() else { panic!("first must be primary") };
        let requests = first.serve();
        let url = "mailto:maya.chen@postbox.example?subject=Hi";
        assert!(matches!(acquire(&path, &Request::Open(url.into())).unwrap(), Acquired::Handed));
        assert_eq!(requests.recv_blocking().unwrap(), Request::Open(url.into()));
        assert!(matches!(acquire(&path, &Request::Activate).unwrap(), Acquired::Handed));
        assert_eq!(requests.recv_blocking().unwrap(), Request::Activate);
        drop(first);
        assert!(!path.exists(), "the socket goes with the primary");
    }

    #[test]
    fn a_stale_socket_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.sock");
        // A socket file with nobody listening, as a crash leaves it.
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        assert!(matches!(acquire(&path, &Request::Activate).unwrap(), Acquired::Primary(_)));
    }

    #[test]
    fn lines_parse() {
        assert_eq!(Request::parse("open mailto:a@b.example\n"), Some(Request::Open("mailto:a@b.example".into())));
        assert_eq!(Request::parse("activate"), Some(Request::Activate));
        assert_eq!(Request::parse("rm -rf"), None);
        assert_eq!(Request::Open("x\ny".into()).line(), "open xy\n");
    }
}
