//! The daemon client: JSON-RPC 2.0, one JSON object per line, over the Unix socket (`core::rpc`).
//!
//! Two plain threads per connection (a reader and a writer) and a table of pending requests, so the
//! futures handed to the app only wait on a oneshot channel: any executor can drive them, and nothing
//! blocks GPUI's thread. Notifications (`method: "event"`) go to [`Backend::events`].
//!
//! The daemon's lifecycle lives in a third thread, the link manager. It dials the socket; when nothing
//! listens there and autostart is on, it starts `lumbridge-maild` (next to this binary, else on `PATH`),
//! waits for its socket, and when a connection drops (the daemon restarted) it dials again with
//! backoff. Requests made while there is no connection wait for the next attempt and fail with
//! `Error::Offline` and a plain reason if it fails. Every change of state is published as a
//! [`LinkState`] so the window can say what is wrong.
//!
//! **Who we are.** The daemon treats a connection as an agent (no Send, sensitive mail as one line)
//! unless it presents the person token in `hello` (docs/ARCHITECTURE.md, "Who is calling"). So every new
//! connection's first line is a `hello` with the token read fresh from `person.token` next to the socket
//! (the daemon writes a new one at every start), after checking that the process on the other end runs
//! as our own user. [`Rpc::agent`] connects without the token (tests; tools that must not send).
//!
//! **Remote.** [`Rpc::remote`] reaches a daemon on another machine through `ssh -T <host> lbmail pipe`
//! (see [`super::ssh`]): the same lines over the child's stdin/stdout. The token comes from
//! `ssh <host> lbmail person-token` before each connection and is kept in memory only. Nothing is ever
//! started here; when ssh drops, the link dials again with backoff and the window shows why meanwhile.

use super::ssh::{self, Stderr};
use super::{Backend, LinkState, Problem, Progress};
use async_trait::async_trait;
use futures::channel::oneshot;
use lumbridge_mail_core::rpc::{self, method, AccountCapabilities, AccountStatus, Act, Answer, AttachmentChunk, AttachmentRead, AttachmentRef, CategoryCount,
                               CountsRequest, DaemonInfo, Discovered,
                               DraftRequest, Event, Hello, HelloParams, MessageRef, NewAccount, ThreadPage, ThreadRef};
use lumbridge_mail_core::remote::Remote;
use lumbridge_mail_core::{Account, AccountId, Body, Draft, Error, Extracted, Mailbox, MessageMeta, Query, Result, Triage};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Weak};
use std::time::{Duration, Instant};

/// The daemon's binary name.
pub const DAEMON: &str = "lumbridge-maild";
/// How long a started daemon has to open its socket.
pub const START_TIMEOUT: Duration = Duration::from_secs(10);
/// Never start the daemon more often than this.
const START_EVERY: Duration = Duration::from_secs(30);
/// A connection that ends sooner than this counts as a failed attempt (backoff grows).
const SHORT_LIVED: Duration = Duration::from_secs(5);

/// Where the daemon listens: `$LUMBRIDGE_MAIL_HOME/maild.sock` when that is set (tests, sandboxes), else
/// `$XDG_RUNTIME_DIR/lumbridge-mail/maild.sock` (docs/ARCHITECTURE.md, "Names"; see [`crate::paths`]).
pub fn default_socket_path() -> PathBuf {
    crate::paths::socket()
}

/// The wait before reconnect attempt `n` (0-based): quick at first, then every 30 s at most.
pub fn backoff(attempt: u32) -> Duration {
    const STEPS_MS: [u64; 8] = [250, 500, 1_000, 2_000, 4_000, 8_000, 15_000, 30_000];
    Duration::from_millis(STEPS_MS[(attempt as usize).min(STEPS_MS.len() - 1)])
}

/// The user id of the process on the other end of a Unix socket.
pub fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `cred` and `len` are valid for writes of the sizes passed; `fd` is an open socket.
        let r = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &mut len) };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(cred.uid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: both out-pointers are valid; `fd` is an open socket.
        let r = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(uid)
    }
}

fn my_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `lumbridge-maild` next to `exe` (an install puts both in one place; on macOS, inside the .app), then
/// on `path` (the `PATH` variable).
pub fn find_daemon(exe: Option<&Path>, path: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(dir) = exe.and_then(Path::parent) {
        let p = dir.join(DAEMON);
        if is_executable(&p) {
            return Some(p);
        }
    }
    std::env::split_paths(path?).map(|d| d.join(DAEMON)).find(|p| is_executable(p))
}

// ---- framing (pure, tested on its own)

/// One request line, newline included.
pub fn encode_request(id: u64, method: &str, params: &Value) -> String {
    let mut line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
    line.push('\n');
    line
}

/// What one line from the daemon means.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    Response { id: u64, result: Result<Value> },
    Event(Box<Event>),
    /// Not for us (an unknown notification) or not JSON-RPC at all.
    Ignored(String),
}

#[derive(Deserialize)]
struct RawMessage {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RawError>,
}

#[derive(Deserialize, Serialize)]
struct RawError {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    /// The daemon puts a `lumbridge_mail_core::Error` here so the app gets the real kind back.
    #[serde(default)]
    data: Option<Value>,
}

pub fn decode_line(line: &str) -> Incoming {
    let line = line.trim();
    let Ok(raw) = serde_json::from_str::<RawMessage>(line) else {
        return Incoming::Ignored("not JSON".into());
    };
    match (raw.id.as_ref().and_then(Value::as_u64), raw.method.as_deref()) {
        (Some(id), None) => {
            let result = match (raw.result, raw.error) {
                (_, Some(err)) => Err(error_from_wire(err)),
                (Some(v), None) => Ok(v),
                (None, None) => Ok(Value::Null),
            };
            Incoming::Response { id, result }
        }
        (None, Some(m)) if m == method::EVENT => match raw.params.map(serde_json::from_value::<Event>) {
            Some(Ok(event)) => Incoming::Event(Box::new(event)),
            // A newer daemon may send event kinds this app does not know yet: skip them.
            _ => Incoming::Ignored("event this app does not know".into()),
        },
        (None, Some(m)) => Incoming::Ignored(format!("notification {m}")),
        _ => Incoming::Ignored("neither a response nor a notification".into()),
    }
}

fn error_from_wire(err: RawError) -> Error {
    if let Some(e) = err.data.and_then(|d| serde_json::from_value::<Error>(d).ok()) {
        return e;
    }
    match err.code {
        -32601 => Error::Unsupported(err.message),
        -32602 | -32600 | -32700 => Error::Invalid(err.message),
        _ => Error::Other(err.message),
    }
}

// ---- one connection

/// Where a response goes: a waiting caller, or a hook the reader runs before it reads the next line (the
/// connection's own `hello`, so `is_person` is settled before any later answer is handed out).
enum Reply {
    Caller(oneshot::Sender<Result<Value>>),
    Hook(Box<dyn FnOnce(Result<Value>) + Send>),
}

impl Reply {
    fn send(self, r: Result<Value>) {
        match self {
            Reply::Caller(tx) => {
                let _ = tx.send(r);
            }
            Reply::Hook(f) => f(r),
        }
    }
}

type Pending = Arc<Mutex<HashMap<u64, Reply>>>;

struct Conn {
    out: mpsc::Sender<String>,
    pending: Pending,
    alive: Arc<AtomicBool>,
    /// Ends the connection at once (the reader then sees the end).
    close: Arc<dyn Fn() + Send + Sync>,
}

enum Wake {
    /// A request is waiting for a connection.
    Request,
    /// The connection dropped (and why, when ssh said).
    Lost(String),
}

/// One connection's two directions, whatever carries them.
struct Link {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    /// Unblocks the reader: shuts the socket down, or kills ssh.
    close: Arc<dyn Fn() + Send + Sync>,
    /// What ssh said, for the reason a remote connection ended.
    stderr: Option<Stderr>,
}

impl Link {
    fn unix(stream: UnixStream) -> std::io::Result<Link> {
        let reader = stream.try_clone()?;
        let closer = stream.try_clone()?;
        Ok(Link {
            reader: Box::new(reader),
            writer: Box::new(stream),
            close: Arc::new(move || {
                let _ = closer.shutdown(std::net::Shutdown::Both);
            }),
            stderr: None,
        })
    }
}

impl Conn {
    fn start(link: Link, events: async_channel::Sender<Event>, wake: mpsc::Sender<Wake>) -> std::io::Result<Conn> {
        let Link { reader, mut writer, close, stderr } = link;
        let pending: Pending = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let (out, lines) = mpsc::channel::<String>();

        std::thread::Builder::new().name("maild-writer".into()).spawn({
            let alive = alive.clone();
            let close = close.clone();
            move || {
                for line in lines {
                    if writer.write_all(line.as_bytes()).and_then(|_| writer.flush()).is_err() {
                        alive.store(false, Ordering::SeqCst);
                        break;
                    }
                }
                // Our side is done (dropped or broken): unblock the reader too.
                drop(writer);
                close();
            }
        })?;

        std::thread::Builder::new().name("maild-reader".into()).spawn({
            let pending = pending.clone();
            let alive = alive.clone();
            let close = close.clone();
            move || {
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => match decode_line(&line) {
                            Incoming::Response { id, result } => {
                                // Out of the map first: a hook may take other locks.
                                let reply = pending.lock().remove(&id);
                                if let Some(reply) = reply {
                                    reply.send(result);
                                }
                            }
                            Incoming::Event(e) => {
                                let _ = events.try_send(*e);
                            }
                            Incoming::Ignored(_) => {}
                        },
                    }
                }
                alive.store(false, Ordering::SeqCst);
                // Reap ssh (if any) and keep its last words.
                close();
                let said = stderr.map(|e| {
                    std::thread::sleep(Duration::from_millis(30));
                    e.text()
                });
                let why = match said.filter(|s| !s.is_empty()) {
                    Some(s) => format!("the connection closed: {s}"),
                    None => "the daemon closed the connection".to_string(),
                };
                let left: Vec<Reply> = pending.lock().drain().map(|(_, r)| r).collect();
                for r in left {
                    r.send(Err(Error::Offline(why.clone())));
                }
                let _ = wake.send(Wake::Lost(why));
            }
        })?;
        Ok(Conn { out, pending, alive, close })
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Hand a request to the writer; gives it back if the connection is gone.
    fn send(&self, id: u64, line: String, reply: Reply) -> std::result::Result<(), Reply> {
        if !self.is_alive() {
            return Err(reply);
        }
        self.pending.lock().insert(id, reply);
        if self.out.send(line).is_err() {
            return Err(self.pending.lock().remove(&id).expect("just inserted"));
        }
        Ok(())
    }
}

// ---- how to reach and start the daemon

/// What starts a daemon that is not running. Returns the child to watch, if there is one.
pub type Starter = Arc<dyn Fn(&Path) -> std::io::Result<Option<Child>> + Send + Sync>;

pub enum Autostart {
    /// Never start a daemon (an explicit `--socket`, tests).
    Off,
    /// Look for `lumbridge-maild` next to this binary, then on `PATH`.
    Find,
    /// Run this program.
    Program(PathBuf),
    /// Tests: bring a daemon up some other way.
    Custom(Starter),
}

fn spawn_daemon(program: &Path) -> std::io::Result<Option<Child>> {
    use std::os::unix::process::CommandExt;
    // `lumbridge-maild` takes no arguments: it finds everything from LUMBRIDGE_MAIL_HOME or the XDG
    // directories (core::paths), which it inherits from us, so both agree on the socket.
    let data = crate::paths::data_dir();
    let log = crate::paths::ensure_private_dir(&data).and_then(|_| open_private_log(&crate::paths::daemon_log()));
    let (out, err) = match log {
        Ok(f) => match f.try_clone() {
            Ok(g) => (Stdio::from(f), Stdio::from(g)),
            Err(_) => (Stdio::null(), Stdio::null()),
        },
        Err(_) => (Stdio::null(), Stdio::null()),
    };
    let child = Command::new(program)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        // Its own process group: a Ctrl-C in the terminal that started the app leaves mail syncing.
        .process_group(0)
        .spawn()?;
    Ok(Some(child))
}

/// The daemon's log, for appending: created 0600, and an older one made 0600 too (it names accounts and
/// servers, and may carry error text from them).
pub fn open_private_log(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let f = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
    if f.metadata()?.permissions().mode() & 0o077 != 0 {
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(f)
}

enum Dial {
    Socket(PathBuf),
    /// Tests hand in one end of a socket pair; it cannot be dialled again.
    Once(Mutex<Option<UnixStream>>),
    /// `ssh <host> lbmail pipe`, a new one per connection.
    Ssh(Remote),
}

struct Inner {
    dial: Dial,
    autostart: Autostart,
    conn: Mutex<Option<Conn>>,
    /// Requests waiting for a connection: (id, line, reply).
    waiting: Mutex<Vec<(u64, String, Reply)>>,
    next_id: AtomicU64,
    events_tx: async_channel::Sender<Event>,
    events_rx: async_channel::Receiver<Event>,
    state: Mutex<LinkState>,
    link_tx: async_channel::Sender<LinkState>,
    link_rx: async_channel::Receiver<LinkState>,
    wake: mpsc::Sender<Wake>,
    /// The manager runs from the first request on.
    started: AtomicBool,
    manager_rx: Mutex<Option<mpsc::Receiver<Wake>>>,
    /// Present the person token on every connection (false: an agent connection).
    person: bool,
    /// What the daemon said to this connection's first `hello` (None until it answered).
    hello: Mutex<Option<Result<Hello>>>,
    /// A remote daemon's person token, fetched over ssh for the current connection (memory only).
    token: Mutex<Option<String>>,
    /// For the status bar: `maild`, or `maild on <host>`.
    name: String,
    /// Where attachments from a daemon on another machine are copied (None = the cache's default).
    files_dir: Mutex<Option<PathBuf>>,
}

pub struct Rpc {
    inner: Arc<Inner>,
}

impl Rpc {
    /// A client for the daemon at `path`. Nothing is dialled until the first call; no daemon is started.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with(Dial::Socket(path.into()), Autostart::Off, true)
    }

    /// A client that starts the daemon when nothing listens at `path`.
    pub fn with_autostart(path: impl Into<PathBuf>, autostart: Autostart) -> Self {
        Self::with(Dial::Socket(path.into()), autostart, true)
    }

    /// A client that never shows the person token: the daemon treats it as an agent (it cannot send, and
    /// sees sensitive mail only as its one line). Never starts a daemon.
    pub fn agent(path: impl Into<PathBuf>) -> Self {
        Self::with(Dial::Socket(path.into()), Autostart::Off, false)
    }

    /// A client over an already-connected stream (tests, or a socket reached some other way). It has no
    /// token file to read, so it is an agent unless the stream's daemon says otherwise.
    pub fn from_stream(stream: UnixStream) -> Self {
        Self::with(Dial::Once(Mutex::new(Some(stream))), Autostart::Off, false)
    }

    /// A client for the daemon on another machine, over ssh, as a person (the token is fetched over
    /// ssh for each connection). Never starts or stops a daemon anywhere.
    pub fn remote(remote: Remote) -> Self {
        Self::with(Dial::Ssh(remote), Autostart::Off, true)
    }

    /// The same over ssh, without ever fetching the token: an agent.
    pub fn remote_agent(remote: Remote) -> Self {
        Self::with(Dial::Ssh(remote), Autostart::Off, false)
    }

    /// The daemon on another machine this client talks to, if it is one.
    pub fn remote_target(&self) -> Option<&Remote> {
        match &self.inner.dial {
            Dial::Ssh(r) => Some(r),
            _ => None,
        }
    }

    fn with(dial: Dial, autostart: Autostart, person: bool) -> Self {
        let (events_tx, events_rx) = async_channel::bounded(1024);
        let (link_tx, link_rx) = async_channel::bounded(64);
        let (wake, manager_rx) = mpsc::channel();
        let name = match &dial {
            Dial::Ssh(r) => format!("maild on {}", r.host),
            _ => "maild".to_string(),
        };
        Self {
            inner: Arc::new(Inner {
                dial,
                autostart,
                conn: Mutex::new(None),
                waiting: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(1),
                events_tx,
                events_rx,
                state: Mutex::new(LinkState::Connecting),
                link_tx,
                link_rx,
                wake,
                started: AtomicBool::new(false),
                manager_rx: Mutex::new(Some(manager_rx)),
                person,
                hello: Mutex::new(None),
                token: Mutex::new(None),
                name,
                files_dir: Mutex::new(None),
            }),
        }
    }

    /// Copy attachments from a remote daemon under `dir` instead of the cache's default
    /// (`<cache>/attachments/remote`).
    pub fn set_files_dir(&self, dir: impl Into<PathBuf>) {
        *self.inner.files_dir.lock() = Some(dir.into());
    }

    fn files_dir(&self) -> PathBuf {
        self.inner.files_dir.lock().clone().unwrap_or_else(|| crate::paths::current().attachments_dir().join("remote"))
    }

    /// Copies an attachment from the daemon into `root` on this machine, in pieces of at most
    /// `rpc::ATTACHMENT_CHUNK_MAX` (`message.attachment.read`): one directory per attachment (a hash of
    /// where it came from), the mail's file name made safe, 0600 in 0700 directories, written under a
    /// temporary name and renamed when complete. A copy already there with the right size is reused.
    /// Refused above `files::MAX_REMOTE_BYTES`, after the first piece says the size.
    pub async fn copy_attachment(&self, a: AttachmentRef, root: &Path, origin: &str, progress: &Progress) -> Result<PathBuf> {
        use crate::files;
        use base64::Engine as _;
        let read = |offset: u64| AttachmentRead {
            account: a.account.clone(),
            message: a.message.clone(),
            attachment: a.attachment.clone(),
            offset,
            max: 0,
        };
        let mut chunk: AttachmentChunk = self.call(method::ATTACHMENT_READ, read(0)).await?;
        let size = chunk.size;
        let name = lumbridge_mail_core::files::safe_file_name(&chunk.filename);
        if size > files::MAX_REMOTE_BYTES {
            return Err(Error::Refused(format!(
                "{name} is {}; the app copies attachments of up to {} from {origin}",
                files::megabytes(size),
                files::megabytes(files::MAX_REMOTE_BYTES)
            )));
        }
        let dir = root.join(files::hashed(&[origin, a.account.as_str(), a.message.as_str(), a.attachment.as_str()]));
        files::private_dir(root).and_then(|_| files::private_dir(&dir)).map_err(Error::Other)?;
        let dest = dir.join(&name);
        if std::fs::metadata(&dest).is_ok_and(|m| m.is_file() && m.len() == size) {
            progress(size, size);
            return Ok(dest);
        }
        let mut out = files::Incoming::create(&dir).map_err(Error::Other)?;
        progress(0, size);
        loop {
            if chunk.offset != out.written || chunk.size != size {
                return Err(Error::Other(format!("{name} changed on {origin} while it was copied; try again")));
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(chunk.data.as_bytes())
                .map_err(|e| Error::Invalid(format!("{name}: a piece was not base64 ({e})")))?;
            if out.written + bytes.len() as u64 > size {
                return Err(Error::Invalid(format!("{name}: more bytes than its size")));
            }
            out.write(&bytes).map_err(Error::Other)?;
            progress(out.written, size);
            if chunk.eof || out.written == size {
                break;
            }
            if bytes.is_empty() {
                return Err(Error::Invalid(format!("{name}: the daemon sent an empty piece before the end")));
            }
            chunk = self.call(method::ATTACHMENT_READ, read(out.written)).await?;
        }
        if out.written != size {
            return Err(Error::Invalid(format!("{name}: {} of {size} bytes arrived", out.written)));
        }
        out.finish(&dest).map_err(Error::Other)?;
        Ok(dest)
    }

    pub fn socket_path(&self) -> Option<&Path> {
        match &self.inner.dial {
            Dial::Socket(p) => Some(p),
            Dial::Once(_) | Dial::Ssh(_) => None,
        }
    }

    pub fn state(&self) -> LinkState {
        self.inner.state.lock().clone()
    }

    /// The daemon accepted this connection as a person's (its first `hello` showed the token).
    pub fn is_person(&self) -> bool {
        matches!(&*self.inner.hello.lock(), Some(Ok(h)) if h.person)
    }

    /// What the daemon said to the current connection's `hello`, once it answered.
    pub fn last_hello(&self) -> Option<Result<Hello>> {
        self.inner.hello.lock().clone()
    }

    fn ensure_manager(&self) {
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(rx) = self.inner.manager_rx.lock().take() else { return };
        let weak = Arc::downgrade(&self.inner);
        let _ = std::thread::Builder::new().name("maild-link".into()).spawn(move || manage(weak, rx));
    }

    /// Send one request; the returned receiver gets its result.
    fn send(&self, method: &str, params: Value) -> oneshot::Receiver<Result<Value>> {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let line = encode_request(id, method, &params);
        let (tx, rx) = oneshot::channel();
        let tx = Reply::Caller(tx);
        let conn = self.inner.conn.lock();
        let mut waiting = self.inner.waiting.lock();
        // Straight to the daemon when connected (and nothing older is still queued, to keep the order).
        let back = match conn.as_ref() {
            Some(c) if waiting.is_empty() => c.send(id, line.clone(), tx).err(),
            _ => Some(tx),
        };
        if let Some(tx) = back {
            // No live connection: wait for the manager's next attempt.
            waiting.push((id, line, tx));
            drop(waiting);
            drop(conn);
            self.ensure_manager();
            let _ = self.inner.wake.send(Wake::Request);
        }
        rx
    }

    pub async fn call<P: Serialize, R: DeserializeOwned>(&self, method: &str, params: P) -> Result<R> {
        let params = serde_json::to_value(params).map_err(|e| Error::Invalid(e.to_string()))?;
        let rx = self.send(method, params);
        let value = rx.await.map_err(|_| Error::Offline("the daemon connection is closed".into()))??;
        serde_json::from_value(value).map_err(|e| Error::Invalid(format!("{method}: unexpected reply: {e}")))
    }
}

impl Inner {
    fn set_state(&self, s: LinkState) {
        let mut cur = self.state.lock();
        if *cur != s {
            *cur = s.clone();
            let _ = self.link_tx.try_send(s);
        }
    }

    fn fail_waiting(&self, why: &str) {
        for (_, _, tx) in self.waiting.lock().drain(..) {
            tx.send(Err(Error::Offline(why.to_string())));
        }
    }

    /// The `hello` that opens every connection: with the person token when we are a person and the
    /// token file is readable (it is new at every daemon start, so it is read each time).
    fn hello_params(&self) -> HelloParams {
        let token = match (&self.dial, self.person) {
            (Dial::Socket(path), true) => std::fs::read_to_string(lumbridge_mail_core::paths::person_token_for(path))
                .ok()
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty()),
            (Dial::Ssh(_), true) => self.token.lock().clone(),
            _ => None,
        };
        HelloParams { person_token: token, client: Some(concat!("lumbridge-mail ", env!("CARGO_PKG_VERSION")).into()) }
    }

    /// Install a new connection and send what waited for it, before anyone else can send. The first
    /// line is always `hello` (the daemon answers it before reading the next line).
    fn install(self: &Arc<Self>, conn: Conn) {
        let mut guard = self.conn.lock();
        let c = guard.insert(conn);
        *self.hello.lock() = None;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let params = serde_json::to_value(self.hello_params()).unwrap_or_else(|_| json!({}));
        // Recorded by the reader itself, before it hands out any later answer on this connection.
        let weak = Arc::downgrade(self);
        let hook = Reply::Hook(Box::new(move |answer: Result<Value>| {
            let answer = answer.and_then(|v| serde_json::from_value::<Hello>(v).map_err(|e| Error::Invalid(format!("hello: {e}"))));
            if let Some(inner) = weak.upgrade() {
                // A remote token from a daemon that restarted since: drop this connection, so the
                // next one fetches the new token (it never goes on as an agent behind our back).
                let stale = matches!((&inner.dial, &answer), (Dial::Ssh(_), Err(Error::Refused(_))));
                *inner.hello.lock() = Some(answer);
                if stale {
                    // Not on the reader: whoever holds the connection lock may be waiting on `pending`.
                    let _ = std::thread::Builder::new().name("maild-stale".into()).spawn(move || {
                        if let Some(c) = inner.conn.lock().as_ref() {
                            (c.close)();
                        }
                    });
                }
            }
        }));
        let _ = c.send(id, encode_request(id, method::HELLO, &params), hook);
        let waiting: Vec<_> = self.waiting.lock().drain(..).collect();
        for (id, line, tx) in waiting {
            if let Err(tx) = c.send(id, line, tx) {
                tx.send(Err(Error::Offline("the daemon closed the connection".into())));
            }
        }
    }

    fn dial(&self) -> std::io::Result<UnixStream> {
        match &self.dial {
            Dial::Socket(path) => UnixStream::connect(path),
            Dial::Once(slot) => slot.lock().take().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotConnected, "connection closed")),
            Dial::Ssh(_) => Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "a remote daemon has no socket here")),
        }
    }

    fn socket(&self) -> PathBuf {
        match &self.dial {
            Dial::Socket(p) => p.clone(),
            Dial::Once(_) => PathBuf::from("(test stream)"),
            Dial::Ssh(r) => PathBuf::from(r.label()),
        }
    }

    /// The problem a lost connection is, for this kind of link.
    fn lost(&self, detail: String) -> Problem {
        match &self.dial {
            Dial::Ssh(r) => Problem::Remote { host: r.host.clone(), detail },
            _ => Problem::Lost { detail },
        }
    }
}

/// A remote connection: the token first (when we are a person), then the pipe. Never starts anything.
fn attempt_ssh(inner: &Inner, remote: &Remote) -> std::result::Result<Link, Problem> {
    if !matches!(*inner.state.lock(), LinkState::Down { .. }) {
        inner.set_state(LinkState::Connecting);
    }
    let problem = |detail: String| Problem::Remote { host: remote.host.clone(), detail };
    let control = ssh::control_dir();
    if inner.person {
        // Also proves the daemon is up there before a pipe is opened.
        let token = ssh::fetch_token(remote, control.as_deref()).map_err(problem)?;
        *inner.token.lock() = Some(token);
    }
    let p = ssh::spawn_pipe(remote, control.as_deref()).map_err(problem)?;
    Ok(Link { reader: p.reader, writer: p.writer, close: p.close, stderr: Some(p.stderr) })
}

/// What the manager remembers about the daemon it started.
#[derive(Default)]
struct Started {
    at: Option<Instant>,
    child: Option<Child>,
    /// Why the last start did not work, reported until the next start is allowed.
    failure: Option<Problem>,
}

/// One go at getting a connection: dial (checking the peer is our own user), maybe start the daemon and
/// wait for it; or, for a remote daemon, ssh.
fn attempt(inner: &Inner, started: &mut Started) -> std::result::Result<Link, Problem> {
    if let Dial::Ssh(remote) = &inner.dial {
        return attempt_ssh(inner, remote);
    }
    let r = attempt_inner(inner, started).and_then(|stream| match peer_uid(&stream) {
        // Never show the person token to a socket another user serves (a shared /tmp fallback).
        Ok(uid) if uid == my_uid() => Link::unix(stream).map_err(|e| Problem::Lost { detail: e.to_string() }),
        Ok(uid) => Err(Problem::Unreachable { socket: inner.socket(), detail: format!("it is served by another user (uid {uid})") }),
        Err(e) => Err(Problem::Unreachable { socket: inner.socket(), detail: format!("no peer credentials: {e}") }),
    });
    if let Err(p @ Problem::StartFailed { .. }) = &r {
        started.failure = Some(p.clone());
    }
    r
}

fn attempt_inner(inner: &Inner, started: &mut Started) -> std::result::Result<UnixStream, Problem> {
    let Started { at: last_start, child, failure } = started;
    // While down, a retry keeps showing why (no flicker through "Connecting").
    if !matches!(*inner.state.lock(), LinkState::Down { .. }) {
        inner.set_state(LinkState::Connecting);
    }
    let err = match inner.dial() {
        Ok(s) => return Ok(s),
        Err(e) => e,
    };
    let nobody_there = matches!(err.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused);
    if !nobody_there {
        return Err(match &inner.dial {
            Dial::Socket(p) => Problem::Unreachable { socket: p.clone(), detail: err.to_string() },
            _ => Problem::Lost { detail: err.to_string() },
        });
    }
    let socket = inner.socket();
    // Stopped or paused on purpose: never start it behind the person's back.
    if let (Dial::Socket(path), false) = (&inner.dial, matches!(inner.autostart, Autostart::Off)) {
        if lumbridge_mail_core::paths::stopped_marker_for(path).exists() {
            return Err(Problem::Stopped { socket, paused: false });
        }
        if *path == crate::paths::socket() && crate::paths::current().paused_file().exists() {
            return Err(Problem::Stopped { socket, paused: true });
        }
    }
    // A daemon we started that has since exited tells us why it is not there.
    if let Some(c) = child.as_mut()
        && let Ok(Some(status)) = c.try_wait()
    {
        *child = None;
        if last_start.is_some_and(|t| t.elapsed() < START_EVERY) {
            return Err(Problem::StartFailed { detail: format!("lumbridge-maild exited ({status}); see its log") });
        }
    }
    let starter: Option<(String, Starter)> = match &inner.autostart {
        Autostart::Off => None,
        Autostart::Find => {
            let exe = std::env::current_exe().ok();
            match find_daemon(exe.as_deref(), std::env::var_os("PATH").as_deref()) {
                Some(p) => Some((p.display().to_string(), Arc::new(move |_: &Path| spawn_daemon(&p)) as Starter)),
                None => return Err(Problem::NotInstalled { socket }),
            }
        }
        Autostart::Program(p) => {
            let p = p.clone();
            Some((p.display().to_string(), Arc::new(move |_: &Path| spawn_daemon(&p)) as Starter))
        }
        Autostart::Custom(f) => Some(("daemon".into(), f.clone())),
    };
    let Some((program, start)) = starter else {
        return Err(Problem::NoDaemon { socket });
    };
    if last_start.is_some_and(|t| t.elapsed() < START_EVERY) || child.is_some() {
        // Started recently (or still starting): wait for it, and keep saying why the start failed.
        return Err(failure.clone().unwrap_or(Problem::NoDaemon { socket }));
    }
    *failure = None;
    inner.set_state(LinkState::Starting { program: program.clone() });
    *last_start = Some(Instant::now());
    match start(&socket) {
        Ok(c) => *child = c,
        Err(e) => return Err(Problem::StartFailed { detail: format!("could not run {program}: {e}") }),
    }
    let until = Instant::now() + START_TIMEOUT;
    while Instant::now() < until {
        if let Ok(s) = inner.dial() {
            return Ok(s);
        }
        if let Some(c) = child.as_mut()
            && let Ok(Some(status)) = c.try_wait()
        {
            *child = None;
            return Err(Problem::StartFailed { detail: format!("lumbridge-maild exited ({status}) before it opened {}", socket.display()) });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(Problem::StartFailed { detail: format!("lumbridge-maild did not open {} within {} s", socket.display(), START_TIMEOUT.as_secs()) })
}

/// The link manager: keeps a connection while the client lives.
fn manage(weak: Weak<Inner>, wake: mpsc::Receiver<Wake>) {
    let mut failures: u32 = 0;
    let mut started = Started::default();
    // Back off only between timed retries; a burst of requests while down must not push the next
    // retry minutes away.
    let mut escalate = true;
    // When the current connection came up: one that drops within seconds (ssh up, daemon not) counts
    // as a failure, so a flapping link backs off instead of redialling four times a second.
    let mut connected_at: Option<Instant> = None;
    loop {
        let Some(inner) = weak.upgrade() else { return };
        let live = inner.conn.lock().as_ref().is_some_and(Conn::is_alive);
        if !live {
            match attempt(&inner, &mut started) {
                Ok(link) => match Conn::start(link, inner.events_tx.clone(), inner.wake.clone()) {
                    Ok(conn) => {
                        connected_at = Some(Instant::now());
                        started.failure = None;
                        inner.set_state(LinkState::Connected);
                        inner.install(conn);
                    }
                    Err(e) => {
                        let p = Problem::Lost { detail: e.to_string() };
                        let why = p.message();
                        if escalate || failures == 0 {
                            failures += 1;
                        }
                        // State first, so a caller woken by the failure already sees why.
                        inner.set_state(LinkState::Down { problem: p, retry_in_s: backoff(failures).as_secs().max(1) });
                        inner.fail_waiting(&why);
                    }
                },
                Err(p) => {
                    *inner.conn.lock() = None;
                    let why = p.message();
                    let wait = backoff(failures);
                    if escalate || failures == 0 {
                        failures += 1;
                    }
                    inner.set_state(LinkState::Down { problem: p, retry_in_s: wait.as_secs().max(1) });
                    inner.fail_waiting(&why);
                }
            }
        }
        let connected = inner.conn.lock().as_ref().is_some_and(Conn::is_alive);
        let once = matches!(inner.dial, Dial::Once(_));
        drop(inner);
        // Connected: sleep until something happens. Down: retry after the backoff, or at once when a
        // request comes in. A test stream cannot be redialled, so there we only wait for requests.
        let msg = if connected || once {
            wake.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            wake.recv_timeout(backoff(failures.saturating_sub(1)))
        };
        escalate = matches!(msg, Err(mpsc::RecvTimeoutError::Timeout));
        match msg {
            Ok(Wake::Lost(why)) => {
                let lived = connected_at.take().map(|t| t.elapsed()).unwrap_or_default();
                if lived >= SHORT_LIVED {
                    failures = 0;
                } else {
                    failures += 1;
                }
                let wait = backoff(failures);
                if let Some(inner) = weak.upgrade() {
                    *inner.conn.lock() = None;
                    let p = inner.lost(why);
                    inner.set_state(LinkState::Down { problem: p, retry_in_s: wait.as_secs().max(1) });
                }
                // Give a restarting daemon a moment before the first redial.
                std::thread::sleep(wait);
            }
            Ok(Wake::Request) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[derive(Serialize)]
struct AccountParam<'a> {
    account: Option<&'a AccountId>,
}

#[derive(Deserialize)]
struct Queued {
    queued: u64,
}

#[derive(Deserialize)]
struct Summary {
    summary: String,
}

#[derive(Deserialize)]
struct Saved {
    path: String,
}

#[derive(Serialize)]
struct AskParams {
    question: String,
    accounts: Option<Vec<AccountId>>,
}

#[derive(Deserialize)]
struct Empty {}

#[async_trait]
impl Backend for Rpc {
    fn name(&self) -> &str {
        &self.inner.name
    }

    async fn hello(&self) -> Result<Hello> {
        // The connection's own hello (with the token) went first; this one only reads the answer again.
        let hello: Hello = self.call(method::HELLO, json!({})).await?;
        if hello.protocol != rpc::PROTOCOL_VERSION {
            return Err(Error::Unsupported(format!(
                "the daemon speaks protocol {}, this app speaks {}",
                hello.protocol,
                rpc::PROTOCOL_VERSION
            )));
        }
        Ok(hello)
    }
    async fn accounts(&self) -> Result<Vec<Account>> {
        self.call(method::ACCOUNTS, json!({})).await
    }
    async fn mailboxes(&self, account: Option<AccountId>) -> Result<Vec<Mailbox>> {
        self.call(method::MAILBOXES, AccountParam { account: account.as_ref() }).await
    }
    async fn threads(&self, query: Query) -> Result<ThreadPage> {
        self.call(method::THREADS, query).await
    }
    async fn thread(&self, thread: ThreadRef) -> Result<Vec<MessageMeta>> {
        self.call(method::THREAD, thread).await
    }
    async fn body(&self, message: MessageRef) -> Result<Body> {
        self.call(method::BODY, message).await
    }
    async fn attachment(&self, attachment: AttachmentRef) -> Result<PathBuf> {
        self.attachment_progress(attachment, Arc::new(|_, _| {})).await
    }
    async fn attachment_progress(&self, attachment: AttachmentRef, progress: Progress) -> Result<PathBuf> {
        if let Dial::Ssh(r) = &self.inner.dial {
            // The daemon's cache is on the other machine: copy the bytes here over the same connection.
            let origin = r.host.clone();
            return self.copy_attachment(attachment, &self.files_dir(), &origin, &progress).await;
        }
        Ok(PathBuf::from(self.call::<_, Saved>(method::ATTACHMENT, attachment).await?.path))
    }
    async fn act(&self, act: Act) -> Result<u64> {
        Ok(self.call::<_, Queued>(method::ACT, act).await?.queued)
    }
    async fn sync(&self, account: Option<AccountId>) -> Result<()> {
        self.call::<_, Empty>(method::SYNC, AccountParam { account: account.as_ref() }).await.map(|_| ())
    }
    async fn status(&self) -> Result<Vec<AccountStatus>> {
        self.call(method::STATUS, json!({})).await
    }
    async fn triage(&self, thread: ThreadRef) -> Result<Triage> {
        self.call(method::AI_TRIAGE, thread).await
    }
    async fn summarise(&self, thread: ThreadRef) -> Result<String> {
        Ok(self.call::<_, Summary>(method::AI_SUMMARISE, thread).await?.summary)
    }
    async fn draft(&self, request: DraftRequest) -> Result<Draft> {
        self.call(method::AI_DRAFT, request).await
    }
    async fn ask(&self, question: String, accounts: Option<Vec<AccountId>>) -> Result<Answer> {
        self.call(method::AI_ASK, AskParams { question, accounts }).await
    }
    async fn extract(&self, thread: ThreadRef, _triage: Option<Triage>) -> Result<Extracted> {
        self.call(method::AI_EXTRACT, thread).await
    }
    async fn counts(&self, request: CountsRequest) -> Result<Vec<CategoryCount>> {
        self.call(method::COUNTS, request).await
    }
    async fn add_account(&self, account: NewAccount) -> Result<Account> {
        self.call(method::ACCOUNTS_ADD, account).await
    }
    async fn daemon_info(&self) -> Result<DaemonInfo> {
        self.call(method::DAEMON_INFO, json!({})).await
    }
    async fn set_paused(&self, paused: bool) -> Result<DaemonInfo> {
        self.call(if paused { method::DAEMON_PAUSE } else { method::DAEMON_RESUME }, json!({})).await
    }
    fn can_quit_daemon(&self) -> bool {
        !matches!(self.inner.dial, Dial::Ssh(_))
    }
    async fn quit_daemon(&self) -> Result<()> {
        if let Dial::Ssh(r) = &self.inner.dial {
            return Err(Error::Refused(format!("the daemon on {} is run by that machine; stop it there", r.host)));
        }
        self.call::<_, Value>(method::DAEMON_QUIT, json!({})).await.map(|_| ())
    }
    async fn capabilities(&self) -> Result<Vec<AccountCapabilities>> {
        self.call(method::ACCOUNTS_CAPABILITIES, json!({})).await
    }
    async fn discover(&self, email: String) -> Result<Discovered> {
        self.call(method::ACCOUNTS_DISCOVER, json!({ "email": email })).await
    }
    fn is_person(&self) -> bool {
        Rpc::is_person(self)
    }
    fn events(&self) -> async_channel::Receiver<Event> {
        self.inner.events_rx.clone()
    }
    fn link(&self) -> Option<async_channel::Receiver<LinkState>> {
        Some(self.inner.link_rx.clone())
    }
    fn link_state(&self) -> Option<LinkState> {
        Some(self.state())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use lumbridge_mail_core::rpc::Caller;
    use lumbridge_mail_core::{Action, ThreadId};
    use std::os::unix::net::UnixListener;

    /// A daemon stand-in: reads request lines, lets `answer` reply (or not), can push events.
    fn fake_daemon(
        answer: impl Fn(&Value) -> Option<Value> + Send + 'static,
        events_first: Vec<Value>,
    ) -> (Rpc, std::thread::JoinHandle<Vec<Value>>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let handle = std::thread::spawn(move || serve_stream(theirs, &answer, events_first));
        (Rpc::from_stream(ours), handle)
    }

    fn serve_stream(theirs: UnixStream, answer: &dyn Fn(&Value) -> Option<Value>, events_first: Vec<Value>) -> Vec<Value> {
        let mut seen = Vec::new();
        let mut w = theirs.try_clone().unwrap();
        for e in events_first {
            writeln!(w, "{e}").unwrap();
        }
        for line in BufReader::new(theirs).lines() {
            let Ok(line) = line else { break };
            let req: Value = serde_json::from_str(&line).unwrap();
            if let Some(reply) = answer(&req) {
                let _ = writeln!(w, "{reply}");
            }
            let stop = req["method"] == "test.stop";
            seen.push(req);
            if stop {
                break;
            }
        }
        seen
    }

    /// A fake daemon listening on a real socket path; serves connections until told to stop.
    struct Listening {
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
        path: PathBuf,
    }

    impl Listening {
        fn at(path: &Path, version: &'static str) -> Self {
            let _ = std::fs::remove_file(path);
            let listener = UnixListener::bind(path).unwrap();
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let handle = std::thread::spawn(move || {
                let mut conns = Vec::new();
                while !flag.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((s, _)) => {
                            s.set_nonblocking(false).unwrap();
                            let keep = s.try_clone().unwrap();
                            conns.push(keep);
                            std::thread::spawn(move || {
                                serve_stream(s, &move |req| Some(ok(req, match req["method"].as_str() {
                                    Some("hello") => json!({"protocol": 1, "version": version, "features": ["new"]}),
                                    _ => json!([]),
                                })), vec![json!({"jsonrpc":"2.0","method":"event","params":{"type":"changed","account":"p","threads":[]}})]);
                            });
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(10)),
                    }
                }
                // "The daemon stops": close every connection and the socket.
                for c in conns {
                    let _ = c.shutdown(std::net::Shutdown::Both);
                }
            });
            Self { stop, handle: Some(handle), path: path.to_owned() }
        }

        fn stop(mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(h) = self.handle.take() {
                h.join().unwrap();
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn ok(req: &Value, result: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": req["id"], "result": result})
    }

    fn wait_for(rpc: &Rpc, want: impl Fn(&LinkState) -> bool) -> LinkState {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let s = rpc.state();
            if want(&s) {
                return s;
            }
            assert!(Instant::now() < until, "timed out in state {s:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn requests_are_one_json_rpc_object_per_line() {
        let line = encode_request(7, "threads.list", &json!({"limit": 5}));
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "threads.list");
        assert_eq!(v["params"]["limit"], 5);
    }

    #[test]
    fn lines_decode_into_responses_errors_and_events() {
        assert_eq!(decode_line(r#"{"jsonrpc":"2.0","id":3,"result":{"queued":2}}"#),
                   Incoming::Response { id: 3, result: Ok(json!({"queued": 2})) });
        let refused = serde_json::to_value(Error::Refused("sending needs a person".into())).unwrap();
        let line = json!({"jsonrpc":"2.0","id":4,"error":{"code":-32000,"message":"refused","data":refused}});
        assert_eq!(decode_line(&line.to_string()),
                   Incoming::Response { id: 4, result: Err(Error::Refused("sending needs a person".into())) });
        assert_eq!(decode_line(r#"{"jsonrpc":"2.0","id":5,"error":{"code":-32601,"message":"no such method"}}"#),
                   Incoming::Response { id: 5, result: Err(Error::Unsupported("no such method".into())) });
        let ev = json!({"jsonrpc":"2.0","method":"event","params":{"type":"changed","account":"p","threads":["t1"]}});
        assert_eq!(decode_line(&ev.to_string()),
                   Incoming::Event(Box::new(Event::Changed { account: AccountId::new("p"), threads: vec![ThreadId::new("t1")] })));
        assert!(matches!(decode_line("garbage"), Incoming::Ignored(_)));
        assert!(matches!(decode_line(r#"{"jsonrpc":"2.0","method":"other","params":{}}"#), Incoming::Ignored(_)));
        // A newer daemon's event kind is skipped, not an error.
        assert!(matches!(decode_line(r#"{"jsonrpc":"2.0","method":"event","params":{"type":"future_thing","x":1}}"#), Incoming::Ignored(_)));
    }

    #[test]
    fn calls_round_trip_through_a_fake_daemon() {
        let (rpc, daemon) = fake_daemon(
            |req| match req["method"].as_str()? {
                "hello" => Some(ok(req, json!({"protocol": 1, "version": "0.0.1-test"}))),
                "act" => Some(ok(req, json!({"queued": 3}))),
                "ai.summarise" => Some(ok(req, json!({"summary": "Lunch moved to Friday.", "model": "extra fields are fine"}))),
                "message.attachment" => Some(ok(req, json!({"path": "/cache/a1.pdf"}))),
                "sync.now" => Some(ok(req, json!({}))),
                "test.stop" => Some(ok(req, json!(null))),
                _ => Some(json!({"jsonrpc":"2.0","id":req["id"],"error":{"code":-32601,"message":"nope"}})),
            },
            vec![],
        );
        let hello = block_on(rpc.hello()).unwrap();
        assert_eq!(hello.version, "0.0.1-test");
        assert_eq!(rpc.state(), LinkState::Connected);
        let draft = Draft { local_id: "d1".into(), subject: "Hi".into(), ..Default::default() };
        let queued = block_on(rpc.act(Act {
            account: AccountId::new("personal"),
            action: Action::Send { draft },
            caller: Caller::Person,
        }))
        .unwrap();
        assert_eq!(queued, 3);
        let summary = block_on(rpc.summarise(ThreadRef { account: AccountId::new("personal"), thread: ThreadId::new("t1") })).unwrap();
        assert_eq!(summary, "Lunch moved to Friday.");
        let path = block_on(rpc.attachment(AttachmentRef { account: AccountId::new("p"), message: lumbridge_mail_core::MessageId::new("m"),
                                                           attachment: lumbridge_mail_core::AttachmentId::new("a1") })).unwrap();
        assert_eq!(path, PathBuf::from("/cache/a1.pdf"));
        block_on(rpc.sync(None)).unwrap();
        let err = block_on(rpc.status()).unwrap_err();
        assert_eq!(err, Error::Unsupported("nope".into()));
        let _ = block_on(rpc.call::<_, Value>("test.stop", json!({})));
        let seen = daemon.join().unwrap();
        let act = seen.iter().find(|r| r["method"] == "act").unwrap();
        assert_eq!(act["params"]["caller"], "person", "the app sends as a person");
        assert_eq!(act["params"]["action"]["op"], "send");
        let ids: Vec<u64> = seen.iter().map(|r| r["id"].as_u64().unwrap()).collect();
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(ids.len(), unique.len(), "every request has its own id");
    }

    /// A daemon serving one made-up attachment in 5-byte pieces; `size` may lie (to test the checks).
    fn chunk_daemon(name: &'static str, bytes: &'static [u8], size: u64, change_at: Option<u64>) -> (Rpc, std::thread::JoinHandle<Vec<Value>>) {
        use base64::Engine as _;
        fake_daemon(
            move |req| match req["method"].as_str()? {
                "message.attachment.read" => {
                    let off = req["params"]["offset"].as_u64().unwrap();
                    let end = (off as usize + 5).min(bytes.len());
                    let size = if change_at.is_some_and(|c| off >= c) { size + 1 } else { size };
                    let data = base64::engine::general_purpose::STANDARD.encode(&bytes[off as usize..end]);
                    Some(ok(req, json!({"filename": name, "mime": "application/octet-stream", "size": size, "offset": off,
                                        "data": data, "eof": end == bytes.len()})))
                }
                "test.stop" => Some(ok(req, json!(null))),
                _ => Some(json!({"jsonrpc":"2.0","id":req["id"],"error":{"code":-32601,"message":"nope"}})),
            },
            vec![],
        )
    }

    fn aref() -> AttachmentRef {
        AttachmentRef { account: AccountId::new("p"), message: lumbridge_mail_core::MessageId::new("m1"),
                        attachment: lumbridge_mail_core::AttachmentId::new("a1") }
    }

    #[test]
    fn remote_attachments_are_copied_in_pieces_into_a_private_cache() {
        use std::os::unix::fs::PermissionsExt;
        const BYTES: &[u8] = b"#!/bin/sh\necho made-up attachment\n";
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("remote");
        let (rpc, daemon) = chunk_daemon("../../setup.sh", BYTES, BYTES.len() as u64, None);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let progress: Progress = { let seen = seen.clone(); Arc::new(move |done, all| seen.lock().push((done, all))) };
        let path = block_on(rpc.copy_attachment(aref(), &cache, "fakehost", &progress)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), BYTES);
        assert_eq!(path.file_name().unwrap(), "setup.sh", "no directories from the mail's name");
        assert!(path.starts_with(&cache), "{}", path.display());
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "private, never executable");
        assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1, "no temporary file left");
        let p = seen.lock().clone();
        assert_eq!(p.first(), Some(&(0, BYTES.len() as u64)));
        assert_eq!(p.last(), Some(&(BYTES.len() as u64, BYTES.len() as u64)));
        assert!(p.windows(2).all(|w| w[0].0 <= w[1].0), "progress only grows: {p:?}");
        // Asked again: the copy is reused (one read for the size, nothing else).
        let again = block_on(rpc.copy_attachment(aref(), &cache, "fakehost", &progress)).unwrap();
        assert_eq!(again, path);
        let _ = block_on(rpc.call::<_, Value>("test.stop", json!({})));
        let reads = daemon.join().unwrap().iter().filter(|r| r["method"] == "message.attachment.read").count();
        assert_eq!(reads, BYTES.len().div_ceil(5) + 1);
    }

    #[test]
    fn remote_attachments_over_the_cap_or_changing_are_refused_and_leave_nothing() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("remote");
        let none: Progress = Arc::new(|_, _| {});
        let (rpc, _d) = chunk_daemon("big.iso", b"0123456789", crate::files::MAX_REMOTE_BYTES + 1, None);
        let err = block_on(rpc.copy_attachment(aref(), &cache, "fakehost", &none)).unwrap_err();
        assert!(matches!(&err, Error::Refused(m) if m.contains("big.iso") && m.contains("256 MB") && m.contains("fakehost")), "{err:?}");
        assert!(!cache.exists(), "nothing written for a refused file");
        let (rpc, _d) = chunk_daemon("notes.txt", b"0123456789abcdef", 16, Some(5));
        let err = block_on(rpc.copy_attachment(aref(), &cache, "fakehost", &none)).unwrap_err();
        assert!(err.to_string().contains("changed"), "{err:?}");
        let leftovers: Vec<_> = walk(&cache);
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if e.path().is_dir() { out.extend(walk(&e.path())) } else { out.push(e.path()) }
        }
        out
    }

    #[test]
    fn replies_may_arrive_out_of_order() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let rpc = Arc::new(Rpc::from_stream(ours));
        let daemon = std::thread::spawn(move || {
            let mut w = theirs.try_clone().unwrap();
            // Skip the connection's opening hello.
            let mut lines = BufReader::new(theirs).lines().map(|l| serde_json::from_str::<Value>(&l.unwrap()).unwrap())
                .filter(|v| v["method"] != "hello");
            let a = lines.next().unwrap();
            let b = lines.next().unwrap();
            writeln!(w, "{}", ok(&b, json!({"summary": "second"}))).unwrap();
            writeln!(w, "{}", ok(&a, json!({"summary": "first"}))).unwrap();
        });
        let t = |id: &str| ThreadRef { account: AccountId::new("p"), thread: ThreadId::new(id) };
        let first = rpc.send(method::AI_SUMMARISE, serde_json::to_value(t("a")).unwrap());
        let second = rpc.send(method::AI_SUMMARISE, serde_json::to_value(t("b")).unwrap());
        let second = block_on(second).unwrap().unwrap();
        let first = block_on(first).unwrap().unwrap();
        assert_eq!(first["summary"], "first");
        assert_eq!(second["summary"], "second");
        daemon.join().unwrap();
    }

    #[test]
    fn events_are_delivered_and_a_closed_socket_fails_pending_calls() {
        let event = json!({"jsonrpc":"2.0","method":"event","params":{"type":"new_mail","account":"w",
                           "thread":"t9","from":"Ops","subject":"Deploy done"}});
        // The fake answers nothing and hangs up after `test.stop`.
        let (rpc, daemon) = fake_daemon(|_| None, vec![event]);
        let events = rpc.events();
        let pending = rpc.send(method::STATUS, json!({}));
        let got = block_on(events.recv()).unwrap();
        assert!(matches!(got, Event::NewMail { ref subject, .. } if subject == "Deploy done"));
        drop(rpc.send("test.stop", json!({})));
        daemon.join().unwrap();
        let result = block_on(pending).unwrap();
        assert!(matches!(result, Err(Error::Offline(_))));
        // A pair cannot be redialled: the next call says offline instead of hanging.
        let next = block_on(rpc.accounts());
        assert!(matches!(next, Err(Error::Offline(_))), "{next:?}");
        assert!(matches!(rpc.state(), LinkState::Down { problem: Problem::Lost { .. }, .. }), "{:?}", rpc.state());
    }

    #[test]
    fn a_missing_socket_is_offline_with_a_reason_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let rpc = Rpc::new(dir.path().join("maild.sock"));
        let err = block_on(rpc.accounts()).unwrap_err();
        assert!(err.is_retryable(), "{err:?}");
        assert!(err.to_string().contains("not running"), "{err}");
        assert!(matches!(rpc.state(), LinkState::Down { problem: Problem::NoDaemon { .. }, .. }));
    }

    #[test]
    fn nothing_listening_starts_the_daemon_and_waits_for_its_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("maild.sock");
        let started = Arc::new(Mutex::new(Vec::<Listening>::new()));
        let starts = Arc::new(AtomicU64::new(0));
        let (s2, n2) = (started.clone(), starts.clone());
        let starter: Starter = Arc::new(move |path: &Path| {
            n2.fetch_add(1, Ordering::SeqCst);
            let path = path.to_owned();
            let s3 = s2.clone();
            // Like a real daemon: the socket appears a little later.
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                s3.lock().push(Listening::at(&path, "started"));
            });
            Ok(None)
        });
        let rpc = Rpc::with_autostart(&sock, Autostart::Custom(starter));
        let link = rpc.link().unwrap();
        let hello = block_on(rpc.hello()).unwrap();
        assert_eq!(hello.version, "started", "tolerates fields the app does not know yet");
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        let mut seen = Vec::new();
        while let Ok(s) = link.try_recv() {
            seen.push(s);
        }
        assert!(seen.iter().any(|s| matches!(s, LinkState::Starting { .. })), "{seen:?}");
        assert_eq!(seen.last(), Some(&LinkState::Connected));
        for l in started.lock().drain(..) {
            l.stop();
        }
    }

    #[test]
    fn a_restarted_daemon_is_found_again() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("maild.sock");
        let first = Listening::at(&sock, "one");
        let rpc = Rpc::new(&sock);
        let events = rpc.events();
        assert_eq!(block_on(rpc.hello()).unwrap().version, "one");
        assert!(block_on(events.recv()).is_ok(), "events flow");
        first.stop();
        wait_for(&rpc, |s| matches!(s, LinkState::Down { .. }));
        let second = Listening::at(&sock, "two");
        wait_for(&rpc, |s| *s == LinkState::Connected);
        assert_eq!(block_on(rpc.hello()).unwrap().version, "two");
        assert!(block_on(events.recv()).is_ok(), "events flow again after the restart");
        second.stop();
    }

    #[test]
    fn no_daemon_binary_is_reported_as_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let p = Problem::NotInstalled { socket: dir.path().join("maild.sock") };
        assert!(p.message().contains("lumbridge-maild"), "{}", p.message());
        // find_daemon: next to the app first, then PATH; only executables count.
        let bin = dir.path().join("bin");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let app = bin.join("lumbridge-mail");
        std::fs::write(&app, "").unwrap();
        assert_eq!(find_daemon(Some(&app), Some(other.as_os_str())), None);
        let on_path = other.join(DAEMON);
        std::fs::write(&on_path, "#!/bin/sh\n").unwrap();
        assert_eq!(find_daemon(Some(&app), Some(other.as_os_str())), None, "not executable");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&on_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_daemon(Some(&app), Some(other.as_os_str())), Some(on_path.clone()));
        let beside = bin.join(DAEMON);
        std::fs::write(&beside, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&beside, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_daemon(Some(&app), Some(other.as_os_str())), Some(beside), "next to the app wins");
    }

    #[test]
    fn a_daemon_that_exits_at_once_is_a_start_failure() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("maild.sock");
        let starter: Starter = Arc::new(|_: &Path| Command::new("/bin/sh").args(["-c", "exit 3"]).spawn().map(Some));
        let rpc = Rpc::with_autostart(&sock, Autostart::Custom(starter));
        let err = block_on(rpc.accounts()).unwrap_err();
        assert!(err.to_string().contains("exited"), "{err}");
        assert!(matches!(rpc.state(), LinkState::Down { problem: Problem::StartFailed { .. }, .. }), "{:?}", rpc.state());
    }

    #[test]
    fn a_daemon_stopped_on_purpose_is_not_started_again() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("maild.sock");
        std::fs::write(lumbridge_mail_core::paths::stopped_marker_for(&sock), "stopped on purpose\n").unwrap();
        let started = Arc::new(AtomicBool::new(false));
        let s2 = started.clone();
        let starter: Starter = Arc::new(move |_: &Path| {
            s2.store(true, Ordering::SeqCst);
            Command::new("/bin/sh").args(["-c", "exit 0"]).spawn().map(Some)
        });
        let rpc = Rpc::with_autostart(&sock, Autostart::Custom(starter));
        let err = block_on(rpc.accounts()).unwrap_err();
        assert!(matches!(err, Error::Offline(_)), "{err}");
        assert!(matches!(rpc.state(), LinkState::Down { problem: Problem::Stopped { paused: false, .. }, .. }), "{:?}", rpc.state());
        assert!(!started.load(Ordering::SeqCst), "the starter must not run while the marker is there");
        assert!(Problem::Stopped { socket: sock.clone(), paused: false }.message().contains("stopped on purpose"));
    }

    #[test]
    fn the_daemon_log_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let new = d.path().join("maild.log");
        drop(open_private_log(&new).unwrap());
        assert_eq!(std::fs::metadata(&new).unwrap().permissions().mode() & 0o777, 0o600);
        // A log left 0644 by an older app is tightened, and appended to, not truncated.
        let old = d.path().join("old.log");
        std::fs::write(&old, b"before\n").unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o644)).unwrap();
        use std::io::Write;
        open_private_log(&old).unwrap().write_all(b"after\n").unwrap();
        assert_eq!(std::fs::metadata(&old).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&old).unwrap(), b"before\nafter\n");
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(0), Duration::from_millis(250));
        assert!(backoff(3) > backoff(2));
        assert_eq!(backoff(50), Duration::from_secs(30));
    }

    #[test]
    fn mailbox_params_carry_the_account() {
        let (rpc, daemon) = fake_daemon(|req| Some(ok(req, json!([]))), vec![]);
        block_on(rpc.mailboxes(Some(AccountId::new("work")))).unwrap();
        block_on(rpc.mailboxes(None)).unwrap();
        let _ = block_on(rpc.call::<_, Value>("test.stop", json!({})));
        let seen: Vec<Value> = daemon.join().unwrap().into_iter().filter(|r| r["method"] != "hello").collect();
        assert_eq!(seen[0]["params"], json!({"account": "work"}));
        assert_eq!(seen[1]["params"], json!({"account": null}));
    }

    /// A fake daemon on a real socket path that records each connection's lines and answers `hello`
    /// with `person` when the right token came.
    fn token_daemon(path: &Path, token: &'static str) -> std::thread::JoinHandle<Vec<Value>> {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            // Like the daemon: a connection that once showed the token stays a person's.
            let person = AtomicBool::new(false);
            serve_stream(s, &move |req| Some(match req["method"].as_str() {
                Some("hello") => {
                    if req["params"]["person_token"] == token {
                        person.store(true, Ordering::SeqCst);
                    }
                    ok(req, json!({"protocol": 1, "version": "t", "person": person.load(Ordering::SeqCst)}))
                }
                _ => ok(req, json!([])),
            }), vec![])
        })
    }

    #[test]
    fn every_connection_opens_with_hello_and_the_person_token() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("maild.sock");
        std::fs::write(dir.path().join("person.token"), "made-up-token\n").unwrap();
        let daemon = token_daemon(&sock, "made-up-token");
        let rpc = Rpc::new(&sock);
        block_on(rpc.accounts()).unwrap();
        let hello = block_on(rpc.hello()).unwrap();
        assert!(hello.person);
        assert!(rpc.is_person());
        let _ = block_on(rpc.call::<_, Value>("test.stop", json!({})));
        let seen = daemon.join().unwrap();
        assert_eq!(seen[0]["method"], "hello", "the first line on the connection");
        assert_eq!(seen[0]["params"]["person_token"], "made-up-token", "read from the file next to the socket");
        assert!(seen[0]["params"]["client"].as_str().unwrap().starts_with("lumbridge-mail"));
        assert_eq!(seen[1]["method"], "accounts.list");
    }

    #[test]
    fn an_agent_connection_never_shows_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("maild.sock");
        std::fs::write(dir.path().join("person.token"), "made-up-token\n").unwrap();
        let daemon = token_daemon(&sock, "made-up-token");
        let rpc = Rpc::agent(&sock);
        assert!(!block_on(rpc.hello()).unwrap().person);
        assert!(!rpc.is_person());
        let _ = block_on(rpc.call::<_, Value>("test.stop", json!({})));
        let seen = daemon.join().unwrap();
        assert_eq!(seen[0]["method"], "hello");
        assert!(seen[0]["params"].get("person_token").is_none(), "{}", seen[0]);
    }

    #[test]
    fn the_peer_is_checked_before_the_token_is_shown() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(&a).unwrap(), my_uid());
    }
}
