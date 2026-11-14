//! A small async client for the daemon (the `lbmail` CLI and the tests use it): over its Unix socket,
//! or over `ssh <host> lbmail pipe` ([`crate::remote::connect`]). One request at a time per call;
//! events that arrive in between are kept for [`Client::next_event`].

use futures::{SinkExt, StreamExt};
use lumbridge_mail_core::rpc::{ErrorObject, Event, Hello, HelloParams, method};
use lumbridge_mail_core::{Error, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::Path;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LinesCodec};

/// Whatever carries the lines: the socket, or an ssh child's stdout + stdin.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub struct Client {
    framed: Framed<Box<dyn Io>, LinesCodec>,
    next_id: u64,
    events: VecDeque<Event>,
    hello: Option<Hello>,
    closed: bool,
    /// Which daemon: `local:<socket>` or `remote:<host>`.
    via: String,
    /// A remote connection's ssh process (killed when the client goes) and what it said on stderr.
    ssh: Option<(tokio::process::Child, crate::remote::Stderr)>,
}

impl Client {
    /// Connects and says hello. With `person_token` the connection may act as a person.
    pub async fn connect(socket: &Path, person_token: Option<String>, client: &str) -> Result<Client> {
        let stream = UnixStream::connect(socket)
            .await
            .map_err(|e| Error::Offline(format!("no daemon at {} ({e}); start lumbridge-maild", socket.display())))?;
        // The socket may sit in a shared /tmp (core::paths, long-path fallback): never show the person
        // token to a process that is not our own user.
        let peer = stream.peer_cred().map_err(|e| Error::Other(format!("{}: peer credentials: {e}", socket.display())))?;
        if peer.uid() != crate::fsutil::euid() {
            return Err(Error::Refused(format!("{} is served by another user (uid {}); not talking to it", socket.display(), peer.uid())));
        }
        let mut c = Client::over(Box::new(stream), format!("local:{}", socket.display()), None);
        c.hello(person_token, client).await?;
        Ok(c)
    }

    /// A client over any line transport (no hello yet).
    pub(crate) fn over(io: Box<dyn Io>, via: String, ssh: Option<(tokio::process::Child, crate::remote::Stderr)>) -> Client {
        Client {
            framed: Framed::new(io, LinesCodec::new_with_max_length(crate::server::MAX_LINE)),
            next_id: 1,
            events: VecDeque::new(),
            hello: None,
            closed: false,
            via,
            ssh,
        }
    }

    /// Says hello (with the token, when given) and keeps the answer.
    pub(crate) async fn hello(&mut self, person_token: Option<String>, client: &str) -> Result<()> {
        let h: Hello = self.call(method::HELLO, HelloParams { person_token, client: Some(client.into()) }).await?;
        self.hello = Some(h);
        Ok(())
    }

    /// The daemon took this connection for a person's (its hello carried the token).
    pub fn is_person(&self) -> bool {
        self.hello.as_ref().is_some_and(|h| h.person)
    }

    /// The daemon's answer to this connection's hello.
    pub fn daemon_hello(&self) -> Option<&Hello> {
        self.hello.as_ref()
    }

    /// `local:<socket>` or `remote:<host>`.
    pub fn via(&self) -> &str {
        &self.via
    }

    /// The connection ended (the daemon or the ssh link went away); calls fail from now on.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Why the connection ended, with what ssh said when it was a remote one.
    async fn gone(&mut self, what: String) -> Error {
        self.closed = true;
        let Some((child, err)) = self.ssh.as_mut() else { return Error::Offline(what) };
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), child.wait()).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let said = err.text();
        Error::Offline(if said.is_empty() { format!("{}: {what}", self.via) } else { format!("{}: {what}: {said}", self.via) })
    }

    /// The token file's contents, if this user can read it.
    pub fn read_person_token(path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok().map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
    }

    pub async fn call<P: Serialize, R: DeserializeOwned>(&mut self, method: &str, params: P) -> Result<R> {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        if self.closed {
            return Err(self.gone("the connection is closed".into()).await);
        }
        if let Err(e) = self.framed.send(line).await {
            return Err(self.gone(format!("daemon connection: {e}")).await);
        }
        loop {
            let line = match self.framed.next().await {
                None => return Err(self.gone("the daemon closed the connection".into()).await),
                Some(Err(e)) => return Err(self.gone(format!("daemon connection: {e}")).await),
                Some(Ok(l)) => l,
            };
            let v: Value = serde_json::from_str(&line).map_err(|e| Error::Other(format!("daemon sent bad JSON: {e}")))?;
            if v.get("method").and_then(Value::as_str) == Some(method::EVENT) {
                if let Some(Ok(e)) = v.get("params").cloned().map(serde_json::from_value::<Event>) {
                    self.events.push_back(e);
                }
                continue;
            }
            if v.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = v.get("error") {
                let obj: ErrorObject = serde_json::from_value(err.clone()).unwrap_or(ErrorObject { code: 0, message: err.to_string(), data: None });
                return Err(obj.into_error());
            }
            let result = v.get("result").cloned().unwrap_or(Value::Null);
            return serde_json::from_value(result).map_err(|e| Error::Other(format!("{method}: unexpected reply: {e}")));
        }
    }

    /// The next event (buffered or from the socket), waiting at most `timeout`. None: none came, or the
    /// connection ended ([`Client::is_closed`]).
    pub async fn next_event(&mut self, timeout: std::time::Duration) -> Option<Event> {
        if let Some(e) = self.events.pop_front() {
            return Some(e);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let line = match tokio::time::timeout_at(deadline, self.framed.next()).await {
                Err(_) => return None,
                Ok(Some(Ok(l))) => l,
                Ok(_) => {
                    self.closed = true;
                    return None;
                }
            };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if v.get("method").and_then(Value::as_str) == Some(method::EVENT)
                && let Some(Ok(e)) = v.get("params").cloned().map(serde_json::from_value::<Event>)
            {
                return Some(e);
            }
        }
    }

    /// Events received so far without waiting.
    pub fn drain_events(&mut self) -> Vec<Event> {
        self.events.drain(..).collect()
    }
}
