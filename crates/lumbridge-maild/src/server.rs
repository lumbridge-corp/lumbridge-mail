//! Starting and stopping the daemon, and the JSON-RPC server on its Unix socket.
//!
//! One daemon per runtime directory (an flock on `maild.lock`). The socket is created in a 0700
//! directory and made 0600; a stale socket left by a crash is removed (we hold the lock, so nobody is
//! listening on it). Connections from another Unix user are closed at once (peer credentials).

use crate::api::{self, Conn};
use crate::config::Config;
use crate::engine::{Shared, triage_loop};
use crate::fsutil;
use futures::{SinkExt, StreamExt};
use lumbridge_mail_ai::Model;
use lumbridge_mail_core::paths::Paths;
use lumbridge_mail_core::rpc::{ErrorObject, code};
use lumbridge_mail_core::*;
use lumbridge_mail_store::Store;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};

/// Longest request line accepted (a draft with inline HTML fits easily).
pub const MAX_LINE: usize = 16 << 20;

/// How to start a daemon. `Options::new(paths)` is the real thing; tests add overrides.
pub struct Options {
    pub paths: Paths,
    /// Use these providers instead of building them from the config (tests: demo providers they hold).
    pub providers: HashMap<AccountId, Arc<dyn Provider>>,
    /// Use this model instead of the configured endpoint (tests: `ScriptedModel`).
    pub model: Option<Arc<dyn Model>>,
    /// Where `accounts.add` keeps passwords (default: the OS keyring; tests: a stand-in).
    pub secrets: Option<Arc<dyn crate::secrets::SecretStore>>,
    /// Run every gog account's calls through this runner (inside the real budget) instead of gog
    /// itself: tests use `lumbridge_mail_gog::fake::FakeRunner`.
    pub gog_runner: Option<Arc<dyn lumbridge_mail_gog::Runner>>,
}

impl Options {
    pub fn new(paths: Paths) -> Self {
        Self { paths, providers: HashMap::new(), model: None, secrets: None, gog_runner: None }
    }
}

pub struct Daemon {
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
    _lock: std::fs::File,
    socket: PathBuf,
}

fn io(path: &Path, e: std::io::Error) -> Error {
    Error::Other(format!("{}: {e}", path.display()))
}

impl Daemon {
    /// Takes the instance lock, opens the store, starts every configured account and listens.
    pub async fn start(opts: Options) -> Result<Daemon> {
        let paths = opts.paths;
        fsutil::private_dir(&paths.runtime_dir)?;
        // A long runtime path puts the socket in a short directory under /tmp (core::paths); it must be ours.
        fsutil::private_dir(&paths.socket_dir())?;
        for d in [&paths.config_dir, &paths.data_dir, &paths.cache_dir] {
            fsutil::private_dir(d)?;
        }
        let lock_path = paths.lock_file();
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path).map_err(|e| io(&lock_path, e))?;
        fsutil::chmod(&lock_path, 0o600);
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(Error::Refused(format!("another lumbridge-maild is running for {}", paths.runtime_dir.display())));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(io(&lock_path, e)),
        }

        let config = Config::load(&paths.config_file())?;
        let db_path = paths.database();
        let mut store = tokio::task::spawn_blocking(move || Store::open(db_path)).await.map_err(|e| Error::Other(e.to_string()))??;
        let recovered = store.recover_interrupted()?;
        if !recovered.is_empty() {
            eprintln!("lumbridge-maild: {} action(s) were interrupted; they will be checked and retried", recovered.len());
        }

        // Started on purpose: whatever stopped the last one, clients may start this daemon again.
        let _ = std::fs::remove_file(paths.stopped_marker());
        let paused = paths.paused_file().exists();
        if paused {
            eprintln!("lumbridge-maild: starting paused ({} exists; lbmail resume to sync)", paths.paused_file().display());
        }
        let person_token = fsutil::random_token()?;
        fsutil::write_private(&paths.person_token(), format!("{person_token}\n").as_bytes())?;

        let (events, _) = broadcast::channel(1024);
        let (shutdown, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            paths: paths.clone(),
            store: Arc::new(Mutex::new(store)),
            config: RwLock::new(config.clone()),
            accounts: RwLock::new(Default::default()),
            events,
            ai: RwLock::new(None),
            person_token,
            shutdown,
            triage_wake: tokio::sync::Notify::new(),
            overrides: opts.providers,
            model_override: opts.model,
            triage_skip: Mutex::new(HashMap::new()),
            version: env!("CARGO_PKG_VERSION").into(),
            secrets: opts.secrets.unwrap_or_else(|| Arc::new(crate::secrets::Keyring)),
            gates: Arc::new(lumbridge_mail_gog::Gates::new()),
            ssh: Mutex::new(HashMap::new()),
            paused: watch::channel(paused).0,
            quit: tokio::sync::Notify::new(),
            body_buckets: Mutex::new(HashMap::new()),
            gog_runner: opts.gog_runner,
            mirror: Default::default(),
            pgwork: Default::default(),
        });

        let socket = paths.socket();
        if socket.exists() {
            // We hold the lock, so whatever is there is left over from a daemon that died.
            std::fs::remove_file(&socket).map_err(|e| io(&socket, e))?;
        }
        let listener = UnixListener::bind(&socket).map_err(|e| io(&socket, e))?;
        fsutil::chmod(&socket, 0o600);

        shared.apply_config(config).await?;
        let mut tasks = vec![tokio::spawn(triage_loop(shared.clone())), tokio::spawn(accept_loop(shared.clone(), listener))];
        tasks.push(tokio::spawn(evict_loop(shared.clone())));
        tasks.push(tokio::spawn(crate::engine::budget_log_loop(shared.clone())));
        Ok(Daemon { shared, tasks, _lock: lock, socket })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn paths(&self) -> &Paths {
        &self.shared.paths
    }

    /// Stops accepting, lets running actions finish (up to 10 s each), stops every loop, removes the
    /// socket and the token, and releases the lock.
    pub async fn shutdown(self) {
        self.shared.shutdown.send_replace(true);
        for rt in self.shared.rts() {
            self.shared.stop_account(&rt).await;
        }
        for t in &self.tasks {
            t.abort();
        }
        for t in self.tasks {
            let _ = t.await;
        }
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(self.shared.paths.person_token());
        // Close the shared ssh connections (they would otherwise linger for ControlPersist).
        let ssh: Vec<lumbridge_mail_gog::SshRunner> = self.shared.ssh.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
        for r in ssh {
            r.close().await;
        }
    }

    /// The daemon's shared state, for tests in this crate.
    pub fn is_paused(&self) -> bool {
        self.shared.is_paused()
    }

    /// Resolves when a client asked the daemon to quit (`daemon.quit`).
    pub async fn quit_requested(&self) {
        self.shared.quit.notified().await
    }

    /// Runs until SIGTERM, SIGINT or `daemon.quit`, then shuts down. SIGHUP re-reads the config
    /// (accounts whose entry changed restart; their gog processes in flight are left to finish).
    pub async fn run_until_signal(self) -> Result<()> {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).map_err(|e| Error::Other(e.to_string()))?;
        let mut int = signal(SignalKind::interrupt()).map_err(|e| Error::Other(e.to_string()))?;
        let mut hup = signal(SignalKind::hangup()).map_err(|e| Error::Other(e.to_string()))?;
        loop {
            tokio::select! {
                _ = term.recv() => break,
                _ = int.recv() => break,
                _ = self.shared.quit.notified() => break,
                _ = hup.recv() => {
                    match Config::load(&self.shared.paths.config_file()) {
                        Ok(c) => match self.shared.apply_config(c).await {
                            Ok(n) => eprintln!("lumbridge-maild: config reloaded, {n} account(s)"),
                            Err(e) => eprintln!("lumbridge-maild: reload failed: {e}"),
                        },
                        Err(e) => eprintln!("lumbridge-maild: config not reloaded: {e}"),
                    }
                }
            }
        }
        self.shutdown().await;
        Ok(())
    }
}

async fn evict_loop(shared: Arc<Shared>) {
    let mut down = shared.shutdown.subscribe();
    loop {
        tokio::select! { _ = tokio::time::sleep(Duration::from_secs(3600)) => {}, _ = down.changed() => return }
        let days = shared.config.read().unwrap_or_else(|e| e.into_inner()).daemon.body_cache_days;
        if days > 0 {
            let before = crate::engine::now() - i64::from(days) * 86_400;
            let _ = shared.db(move |s| s.evict_bodies(before)).await;
        }
    }
}

async fn accept_loop(shared: Arc<Shared>, listener: UnixListener) {
    let mut down = shared.shutdown.subscribe();
    let me = fsutil::euid();
    loop {
        let stream = tokio::select! {
            r = listener.accept() => match r {
                Ok((s, _)) => s,
                Err(_) => { tokio::time::sleep(Duration::from_millis(100)).await; continue }
            },
            _ = down.changed() => return,
        };
        // Only our own user (the socket is 0600 anyway; this also covers a socket reached some other way).
        if stream.peer_cred().map(|c| c.uid()).ok() != Some(me) {
            continue;
        }
        tokio::spawn(serve(shared.clone(), stream));
    }
}

fn error_line(id: Value, e: ErrorObject) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": e }).to_string()
}

async fn serve(shared: Arc<Shared>, stream: UnixStream) {
    let framed = Framed::new(stream, LinesCodec::new_with_max_length(MAX_LINE));
    let (mut sink, mut lines) = framed.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(256);
    let mut events = shared.events.subscribe();
    let mut down = shared.shutdown.subscribe();
    let writer = tokio::spawn(async move {
        loop {
            let line = tokio::select! {
                l = out_rx.recv() => match l { Some(l) => l, None => break },
                e = events.recv() => match e {
                    Ok(ev) => json!({ "jsonrpc": "2.0", "method": lumbridge_mail_core::rpc::method::EVENT, "params": ev }).to_string(),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            };
            if sink.send(line).await.is_err() {
                break;
            }
        }
    });
    let conn = Arc::new(Conn::default());
    loop {
        let next = tokio::select! {
            l = lines.next() => l,
            _ = down.changed() => None,
        };
        let line = match next {
            None => break,
            Some(Ok(l)) => l,
            Some(Err(LinesCodecError::MaxLineLengthExceeded)) => {
                let _ = out_tx.send(error_line(Value::Null, ErrorObject::new(code::INVALID_REQUEST, Error::Invalid("line too long".into())))).await;
                break;
            }
            Some(Err(_)) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = out_tx.send(error_line(Value::Null, ErrorObject::new(code::PARSE_ERROR, Error::Invalid(format!("not JSON: {e}"))))).await;
                continue;
            }
        };
        let id = req.get("id").cloned();
        let Some(method) = req.get("method").and_then(Value::as_str).map(str::to_owned) else {
            let _ = out_tx
                .send(error_line(id.unwrap_or(Value::Null), ErrorObject::new(code::INVALID_REQUEST, Error::Invalid("no method".into()))))
                .await;
            continue;
        };
        let p = req.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id.filter(|i| !i.is_null()) else {
            // A notification from a client: nothing to answer; we have none to act on.
            continue;
        };
        let is_hello = method == lumbridge_mail_core::rpc::method::HELLO;
        let (shared, conn, tx) = (shared.clone(), conn.clone(), out_tx.clone());
        let task = async move {
            let line = match api::dispatch(&shared, &conn, &method, p).await {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string(),
                Err(e) => error_line(id, ErrorObject::from(e)),
            };
            let _ = tx.send(line).await;
        };
        if is_hello {
            // Before anything after it: a client may send hello and an act without waiting.
            task.await;
        } else {
            tokio::spawn(task);
        }
    }
    drop(out_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
}
