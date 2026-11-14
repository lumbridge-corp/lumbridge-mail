//! The connection to Zoho's hosted MCP server: JSON-RPC 2.0 `tools/call` over HTTPS POST with a Bearer
//! token. The answer is either a JSON body or an SSE stream whose `data:` lines carry the JSON-RPC
//! response. The tool's result is `content[0].text` holding Zoho Mail's REST answer as JSON, usually
//! `{status: {code, description}, data}`; [`ZohoConn::call`] returns `data`.
//!
//! **Only mail tools.** The same server offers organisation-admin tools (delete users, reset passwords,
//! domains, policies). [`ALLOWED_TOOLS`] lists the tools this crate uses; [`ZohoConn::call`] refuses any
//! other name before anything is sent.
//!
//! One `ZohoConn` per token file and server, shared by every account on it ([`ZohoConn::shared`]): one
//! HTTP client, one token, one budget ([`crate::budget`]).

use crate::budget::Gate;
use crate::config::{Connection, Limits};
use crate::token::TokenStore;
use lumbridge_mail_core::{Error, ProviderLoad, Result};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

/// Every tool this crate may call. Anything else is refused (`Error::Refused`) without a request.
pub const ALLOWED_TOOLS: &[&str] = &[
    "ZohoMail_list_accounts",
    "ZohoMail_list_folders",
    "ZohoMail_list_mails",
    "ZohoMail_get_mail",
    "ZohoMail_get_mail_html",
    "ZohoMail_get_mail_headers",
    "ZohoMail_get_mail_original",
    "ZohoMail_list_mail_attachments",
    "ZohoMail_mark_mails_as_read",
    "ZohoMail_mark_mails_as_unread",
    "ZohoMail_apply_mail_flag",
    "ZohoMail_move_mails",
    "ZohoMail_archive_mails",
    "ZohoMail_unarchive_mails",
    "ZohoMail_move_mails_to_trash",
    "ZohoMail_send_mail",
    "ZohoMail_reply_to_mail",
];

const PROTOCOL_VERSION: &str = "2025-03-26";

/// Whether a call may be repeated after a transport failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// Reads and changes that are the same done twice (mark read, move, flag).
    Safe,
    /// A send: a timeout may mean it went out. The action queue retries it after a duplicate check.
    Never,
}

/// `https://...`, or `http://` to this machine (tests). Anything else would put the token on the wire.
pub(crate) fn check_url(url: &str, what: &str) -> Result<()> {
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(());
    }
    if let Some(rest) = lower.strip_prefix("http://") {
        let host = rest.split(['/', '?']).next().unwrap_or("");
        let host = match host.strip_prefix('[') {
            Some(h) => h.split(']').next().unwrap_or(""),
            None => host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host),
        };
        if matches!(host, "127.0.0.1" | "localhost" | "::1") {
            return Ok(());
        }
    }
    Err(Error::Refused(format!(
        "{what} must be an https:// URL (plain http only to this machine)"
    )))
}

/// A short, printable excerpt of something the server said (never mail; error texts only).
pub(crate) fn excerpt(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(200).collect()
}

/// How one HTTP exchange ended, before retries.
enum Fail {
    /// 401, or an answer that says the token is bad: refresh once and try again.
    Unauthorized,
    /// The server wants an MCP session (`initialize` first).
    NeedSession,
    /// 429: wait this long.
    RateLimited(Duration),
    /// Connection, timeout, 5xx, a cut-off answer: counts against the budget's breaker.
    Transport(Error),
    /// An answer: no retry.
    Answer(Error),
}

pub struct ZohoConn {
    cfg: Connection,
    http: reqwest::Client,
    token: TokenStore,
    gate: Arc<Gate>,
    session: Mutex<Option<String>>,
    next_id: AtomicU64,
    max_bytes: AtomicU64,
    accounts: tokio::sync::Mutex<Option<Vec<Value>>>,
}

static REGISTRY: Mutex<Vec<Weak<ZohoConn>>> = Mutex::new(Vec::new());

/// The budget of every live Zoho connection (for `daemon.info`).
pub fn loads() -> Vec<ProviderLoad> {
    let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    reg.iter()
        .filter_map(Weak::upgrade)
        .map(|c| c.gate.load())
        .collect()
}

fn scope_of(cfg: &Connection) -> String {
    let name = cfg
        .token_file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("zoho:{name}")
}

impl ZohoConn {
    /// The connection for `cfg`: the live one with the same server, token endpoint, client and token
    /// file if there is one (its limits updated), else a new one.
    pub fn shared(
        cfg: &Connection,
        limits: &Limits,
        timeout: Duration,
        max_bytes: u64,
    ) -> Result<Arc<ZohoConn>> {
        let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        reg.retain(|w| w.strong_count() > 0);
        if let Some(c) = reg.iter().filter_map(Weak::upgrade).find(|c| c.cfg == *cfg) {
            if c.gate.limits() != *limits {
                c.gate.set_limits(limits.clone());
            }
            c.max_bytes.store(max_bytes, Ordering::Relaxed);
            return Ok(c);
        }
        let c = Arc::new(Self::new(cfg, limits, timeout, max_bytes)?);
        reg.push(Arc::downgrade(&c));
        Ok(c)
    }

    fn new(
        cfg: &Connection,
        limits: &Limits,
        timeout: Duration,
        max_bytes: u64,
    ) -> Result<ZohoConn> {
        check_url(&cfg.mcp_url, "the Zoho MCP URL")?;
        check_url(&cfg.token_url, "the Zoho token URL")?;
        if cfg.client_id.trim().is_empty() {
            return Err(Error::Invalid(
                "the Zoho connection needs a client id".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(timeout)
            .user_agent("lumbridge-mail")
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| Error::Other(format!("http client: {e}")))?;
        Ok(Self {
            token: TokenStore::new(
                cfg.token_file.clone(),
                cfg.token_url.clone(),
                cfg.client_id.trim().to_owned(),
                cfg.mcp_url.clone(),
            ),
            gate: Gate::new(scope_of(cfg), limits.clone()),
            cfg: cfg.clone(),
            http,
            session: Mutex::new(None),
            next_id: AtomicU64::new(1),
            max_bytes: AtomicU64::new(max_bytes),
            accounts: tokio::sync::Mutex::new(None),
        })
    }

    pub fn load(&self) -> ProviderLoad {
        self.gate.load()
    }

    /// Token refreshes this connection has posted.
    pub fn refreshes(&self) -> u64 {
        self.token.refreshes()
    }

    /// `ZohoMail_list_accounts`, once per connection (the accounts of one sign-in rarely change).
    pub async fn accounts(&self) -> Result<Vec<Value>> {
        let mut cache = self.accounts.lock().await;
        if let Some(a) = cache.as_ref() {
            return Ok(a.clone());
        }
        let v = self
            .call("ZohoMail_list_accounts", json!({}), Retry::Safe)
            .await?;
        let list = crate::parse::accounts(&v)
            .ok_or_else(|| crate::parse::shape_error("list_accounts", &v))?;
        *cache = Some(list.clone());
        Ok(list)
    }

    /// Calls one allowed tool and returns Zoho's innermost `data` (the answer is wrapped twice:
    /// `{headers, status, data: {requestId, status: {code}, data: {...}}}`; a single wrapping is read
    /// too). A plain-text answer is the server's error message and comes back as an error.
    pub async fn call(&self, tool: &str, arguments: Value, retry: Retry) -> Result<Value> {
        self.call_inner(tool, arguments, retry, false).await
    }

    /// Like [`ZohoConn::call`], but returns the tool's JSON answer as it came (for the probe). Still
    /// only allowed tools, still checked against their schema; plain text is still an error.
    pub async fn call_raw(&self, tool: &str, arguments: Value) -> Result<Value> {
        self.call_inner(tool, arguments, Retry::Safe, true).await
    }

    async fn call_inner(
        &self,
        tool: &str,
        arguments: Value,
        retry: Retry,
        raw: bool,
    ) -> Result<Value> {
        if !ALLOWED_TOOLS.contains(&tool) {
            return Err(Error::Refused(format!(
                "{tool} is not a mail tool this client uses"
            )));
        }
        crate::schema::check(tool, &arguments)?;
        let max_tries = 1 + match retry {
            Retry::Safe => self.gate.limits().retries,
            Retry::Never => 0,
        };
        let mut tries = 0u32;
        let mut refreshed = false;
        let mut initialised = false;
        loop {
            tries += 1;
            let slot = self.gate.acquire().await?;
            let token = self.token.access(&self.http).await?;
            let body = json!({
                "jsonrpc": "2.0",
                "id": self.next_id.fetch_add(1, Ordering::Relaxed),
                "method": "tools/call",
                "params": { "name": tool, "arguments": arguments },
            });
            let out = self
                .exchange(&body, &token)
                .await
                .and_then(|r| tool_json(tool, r))
                .and_then(|v| if raw { Ok(v) } else { unwrap_envelope(tool, v) });
            drop(slot);
            match out {
                Ok(v) => {
                    self.gate.succeeded();
                    return Ok(v);
                }
                Err(Fail::Unauthorized) if !refreshed => {
                    self.gate.succeeded();
                    refreshed = true;
                    tries -= 1;
                    self.token.renew(&self.http, Some(&token)).await?;
                }
                Err(Fail::Unauthorized) => {
                    return Err(Error::Auth("Zoho did not accept the sign-in even after a refresh; sign in to Zoho again".into()));
                }
                Err(Fail::NeedSession) if !initialised => {
                    initialised = true;
                    tries -= 1;
                    self.initialise(&token).await?;
                }
                Err(Fail::NeedSession) => {
                    return Err(Error::Other(
                        "Zoho MCP: the server keeps asking for a new session".into(),
                    ));
                }
                Err(Fail::RateLimited(wait)) => {
                    self.gate.hold(wait);
                    if tries >= max_tries {
                        return Err(Error::Offline("Zoho rate limit; trying again later".into()));
                    }
                }
                Err(Fail::Transport(e)) => {
                    self.gate.failed();
                    if tries >= max_tries {
                        return Err(e);
                    }
                }
                Err(Fail::Answer(e)) => {
                    self.gate.succeeded();
                    return Err(e);
                }
            }
        }
    }

    /// `initialize` + `notifications/initialized`, keeping the session id the server hands out.
    async fn initialise(&self, token: &str) -> Result<()> {
        *self.session.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let init = json!({
            "jsonrpc": "2.0",
            "id": self.next_id.fetch_add(1, Ordering::Relaxed),
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "lumbridge-mail", "version": env!("CARGO_PKG_VERSION") },
            },
        });
        let _slot = self.gate.acquire().await?;
        match self.exchange(&init, token).await {
            Ok(_) => {}
            Err(Fail::Transport(e)) => {
                self.gate.failed();
                return Err(e);
            }
            Err(Fail::Answer(e)) => return Err(e),
            Err(_) => return Err(Error::Other("Zoho MCP: initialize failed".into())),
        }
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        let _ = self.exchange(&note, token).await;
        Ok(())
    }

    /// One POST. Returns the JSON-RPC response object (`Value::Null` for an accepted notification).
    async fn exchange(&self, body: &Value, token: &str) -> std::result::Result<Value, Fail> {
        let session = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut r = self
            .http
            .post(&self.cfg.mcp_url)
            .bearer_auth(token)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(s) = &session {
            r = r
                .header("mcp-session-id", s)
                .header("mcp-protocol-version", PROTOCOL_VERSION);
        }
        let resp = r
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| Fail::Transport(transport(&e)))?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        if let Some(s) = headers.get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *self.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(s.to_owned());
        }
        let sse = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.to_ascii_lowercase().contains("text/event-stream"));
        let bytes = self.read_capped(resp).await?;
        let text = String::from_utf8_lossy(&bytes);
        match status {
            200..=299 => {}
            401 => return Err(Fail::Unauthorized),
            403 => {
                return Err(Fail::Answer(Error::Auth(
                    "Zoho refused: the sign-in may lack a permission this needs".into(),
                )));
            }
            404 if session.is_some() => {
                *self.session.lock().unwrap_or_else(|e| e.into_inner()) = None;
                return Err(Fail::NeedSession);
            }
            400 if mentions_session(&text) => return Err(Fail::NeedSession),
            429 => return Err(Fail::RateLimited(retry_after(&headers))),
            500..=599 => {
                return Err(Fail::Transport(Error::Offline(format!(
                    "Zoho MCP error {status}"
                ))));
            }
            _ => {
                return Err(Fail::Answer(Error::Other(format!(
                    "Zoho MCP answered {status}"
                ))));
            }
        }
        if body.get("id").is_none() || text.trim().is_empty() {
            return Ok(Value::Null);
        }
        let want = body.get("id").cloned().unwrap_or(Value::Null);
        if sse {
            return sse_response(&text, &want).ok_or_else(|| {
                Fail::Transport(Error::Offline(
                    "Zoho MCP: the stream ended without an answer".into(),
                ))
            });
        }
        serde_json::from_str(&text).map_err(|_| {
            Fail::Transport(Error::Offline("Zoho MCP: the answer was not JSON".into()))
        })
    }

    async fn read_capped(&self, mut resp: reqwest::Response) -> std::result::Result<Vec<u8>, Fail> {
        let cap = self.max_bytes.load(Ordering::Relaxed).max(1 << 20);
        if resp.content_length().is_some_and(|n| n > cap) {
            return Err(Fail::Answer(Error::Refused(format!(
                "Zoho's answer is larger than the {} MB limit",
                cap >> 20
            ))));
        }
        let mut out = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| Fail::Transport(transport(&e)))?
        {
            if out.len() as u64 + chunk.len() as u64 > cap {
                return Err(Fail::Answer(Error::Refused(format!(
                    "Zoho's answer is larger than the {} MB limit",
                    cap >> 20
                ))));
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }
}

fn transport(e: &reqwest::Error) -> Error {
    let what = if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "could not connect"
    } else if e.is_body() || e.is_decode() {
        "the answer was cut off"
    } else {
        "request failed"
    };
    Error::Offline(format!("Zoho: {what}"))
}

fn mentions_session(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.contains("session") || l.contains("initializ") || l.contains("initialis")
}

fn mentions_bad_token(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    [
        "invalid_oauthtoken",
        "invalid oauth",
        "invalid_token",
        "oauth token",
        "unauthorized",
        "unauthorised",
        "token expired",
        "expired token",
    ]
    .iter()
    .any(|p| l.contains(p))
}

fn retry_after(h: &reqwest::header::HeaderMap) -> Duration {
    let s = h
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok());
    match s {
        Some(s) if s.is_finite() && s >= 0.0 => Duration::from_secs_f64(s),
        _ => Duration::from_secs(30),
    }
}

/// The JSON-RPC response in an SSE body: the first event whose JSON has our `id` (or, failing that, the
/// first that carries a `result` or an `error`). Events are separated by blank lines; an event's `data:`
/// lines are joined with newlines.
pub(crate) fn sse_response(text: &str, id: &Value) -> Option<Value> {
    let mut fallback = None;
    let norm = text.replace("\r\n", "\n").replace('\r', "\n");
    for event in norm.split("\n\n") {
        let data: Vec<&str> = event
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(|d| d.strip_prefix(' ').unwrap_or(d))
            .collect();
        if data.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&data.join("\n")) else {
            continue;
        };
        if v.get("id") == Some(id) {
            return Some(v);
        }
        if fallback.is_none() && (v.get("result").is_some() || v.get("error").is_some()) {
            fallback = Some(v);
        }
    }
    fallback
}

/// The tool's answer as JSON, not yet unwrapped: JSON-RPC result -> MCP content text (or
/// `structuredContent`) -> JSON. A text that is not JSON is the server's error message (Zoho sends
/// those with `isError: false`, e.g. "Invalid keys found in path variable ...").
fn tool_json(tool: &str, rpc: Value) -> std::result::Result<Value, Fail> {
    if let Some(err) = rpc.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        let msg = err.get("message").and_then(Value::as_str).unwrap_or("");
        if mentions_bad_token(msg) {
            return Err(Fail::Unauthorized);
        }
        if mentions_session(msg) && !msg.to_ascii_lowercase().contains("argument") {
            return Err(Fail::NeedSession);
        }
        return Err(Fail::Answer(match code {
            -32601 => Error::Unsupported(format!("Zoho MCP has no {tool}")),
            -32602 => Error::Invalid(format!(
                "Zoho MCP: bad arguments for {tool}: {}",
                excerpt(msg)
            )),
            _ => Error::Other(format!("Zoho MCP error {code} on {tool}: {}", excerpt(msg))),
        }));
    }
    let Some(result) = rpc.get("result") else {
        return Err(Fail::Transport(Error::Offline(
            "Zoho MCP: an answer without a result".into(),
        )));
    };
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text: String = crate::parse::list_of(result, &["content"])
        .unwrap_or_default()
        .iter()
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    let payload = match result.get("structuredContent") {
        Some(v @ Value::Object(_)) => Some(v.clone()),
        _ => serde_json::from_str::<Value>(text.trim())
            .ok()
            .filter(|v| v.is_object() || v.is_array()),
    };
    match payload {
        Some(v) if is_error => Err(classify(tool, Some(&v), &text)),
        Some(v) => Ok(v),
        None if text.trim().is_empty() && !is_error => Ok(Value::Null),
        None => Err(classify(tool, None, &text)),
    }
}

/// The status code of one layer: `status.code`, or `code`.
fn layer_code(v: &Value) -> Option<i64> {
    crate::parse::int(
        v.get("status")
            .and_then(|s| s.get("code"))
            .or_else(|| v.get("code")),
    )
}

/// Peels Zoho's wrappers: each layer with a `data` and a `status` / `requestId` / `headers` (or only a
/// `data`) is checked (a non-2xx `status.code`, or a `status` word other than success, is an error)
/// and replaced by its `data`. What is left is the tool's own answer (`{messages, nextPageToken}`, ...).
fn unwrap_envelope(tool: &str, v: Value) -> std::result::Result<Value, Fail> {
    let mut cur = v;
    for _ in 0..6 {
        let Value::Object(m) = &cur else { break };
        if layer_code(&cur).is_some_and(|c| !(200..300).contains(&c)) {
            return Err(classify(tool, Some(&cur), ""));
        }
        let wrapper = m.contains_key("data")
            && (m.contains_key("status")
                || m.contains_key("requestId")
                || m.contains_key("headers")
                || m.len() == 1);
        if !wrapper {
            break;
        }
        if let Some(word) = m.get("status").and_then(Value::as_str)
            && !matches!(word.to_ascii_lowercase().as_str(), "success" | "ok" | "200")
        {
            return Err(classify(tool, Some(&cur), word));
        }
        let Value::Object(mut m) = cur else { break };
        cur = m.remove("data").unwrap_or(Value::Null);
    }
    Ok(cur)
}

/// An error answer to a failure kind. `v` is the JSON answer, if it was JSON; `text` the server's words.
fn classify(tool: &str, v: Option<&Value>, text: &str) -> Fail {
    // The deepest status code and description in the wrappers.
    let mut code = None;
    let mut desc: Option<String> = None;
    let mut cur = v;
    for _ in 0..6 {
        let Some(c) = cur else { break };
        if let Some(x) = layer_code(c) {
            code = Some(x);
        }
        let d = c
            .get("status")
            .and_then(|s| s.get("description"))
            .or_else(|| c.get("moreInfo"))
            .or_else(|| c.get("message"))
            .and_then(Value::as_str);
        if let Some(d) = d {
            desc = Some(d.to_owned());
        } else if desc.is_none()
            && let Some(e) = c.get("errorCode").and_then(Value::as_str)
        {
            desc = Some(e.to_owned());
        }
        cur = c.get("data").filter(|d| d.is_object());
    }
    let details = v.map(error_details).unwrap_or_default();
    let mut described = excerpt(desc.as_deref().unwrap_or(text));
    if !details.is_empty() {
        let d: Vec<String> = details.iter().map(|(k, v)| format!("{k} {v}")).collect();
        described = format!("{described} ({})", d.join(", "));
    }
    let all = format!("{text} {described}");
    if code == Some(401) || mentions_bad_token(&all) {
        return Fail::Unauthorized;
    }
    let l = all.to_ascii_lowercase();
    match code {
        Some(429) => Fail::RateLimited(Duration::from_secs(30)),
        _ if l.contains("too many") || l.contains("rate limit") => {
            Fail::RateLimited(Duration::from_secs(30))
        }
        Some(404) => Fail::Answer(Error::NotFound(format!("Zoho: {described}"))),
        Some(c) if c >= 500 => Fail::Transport(Error::Offline(format!("Zoho error {c} on {tool}"))),
        _ if l.contains("not found") || l.contains("does not exist") => {
            Fail::Answer(Error::NotFound(format!("Zoho: {described}")))
        }
        // Zoho named what in the call it does not take (EXTRA_PARAM_FOUND at includeArchived, ...): the
        // same call will never work, so it is `Unsupported` (never retried as is).
        Some(400) if details.iter().any(|(k, _)| *k == "errorCode") => Fail::Answer(Error::Unsupported(
            format!("Zoho said 400 to {tool}: {described}"),
        )),
        Some(c) => Fail::Answer(Error::Other(format!(
            "Zoho said {c} to {tool}: {described}"
        ))),
        None => Fail::Answer(Error::Other(format!(
            "Zoho could not do {tool}: {described}"
        ))),
    }
}

/// Zoho's own words about a refused call, wherever in the answer they are (a nested `data`, a list,
/// a JSON string): `errorCode`, `location`, `locationType`. Server vocabulary, never mail; kept to plain
/// identifier characters and 64 of them.
fn error_details(v: &Value) -> Vec<(&'static str, String)> {
    fn find(v: &Value, key: &str, depth: u32) -> Option<String> {
        if depth > 8 {
            return None;
        }
        match v {
            Value::Object(m) => {
                if let Some(x) = m.get(key) {
                    let s = match x {
                        Value::String(s) => Some(s.clone()),
                        Value::Number(n) => Some(n.to_string()),
                        _ => None,
                    };
                    if let Some(s) = s.filter(|s| !s.trim().is_empty()) {
                        return Some(s);
                    }
                }
                m.values().find_map(|x| find(x, key, depth + 1))
            }
            Value::Array(a) => a.iter().find_map(|x| find(x, key, depth + 1)),
            Value::String(s) if s.trim_start().starts_with('{') || s.trim_start().starts_with('[') => {
                serde_json::from_str::<Value>(s).ok().and_then(|x| find(&x, key, depth + 1))
            }
            _ => None,
        }
    }
    let word = |s: String| -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/'))
            .take(64)
            .collect()
    };
    ["errorCode", "location", "locationType"]
        .into_iter()
        .filter_map(|k| find(v, k, 0).map(word).filter(|w| !w.is_empty()).map(|w| (k, w)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_bodies() {
        let id = json!(7);
        let body = "event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\r\n\r\nevent: message\r\ndata: {\"jsonrpc\":\"2.0\",\r\ndata: \"id\":7,\"result\":{}}\r\n\r\n";
        assert_eq!(sse_response(body, &id).unwrap()["id"], 7);
        assert!(sse_response("data: nope\n\n", &id).is_none());
    }

    fn text(t: &str) -> Value {
        json!({"result": {"content": [{"type": "text", "text": t}]}})
    }

    fn payload(rpc: Value) -> std::result::Result<Value, Fail> {
        tool_json("t", rpc).and_then(|v| unwrap_envelope("t", v))
    }

    #[test]
    fn payloads_unwrap_and_errors_map() {
        // The real double wrapping.
        let real = json!({"headers": {"content-type": "application/json"}, "status": "success",
                          "data": {"requestId": "r", "status": {"code": 200, "description": "OK"},
                                   "data": {"messages": [], "nextPageToken": "x"}}});
        assert_eq!(
            payload(text(&real.to_string())).ok().unwrap(),
            json!({"messages": [], "nextPageToken": "x"})
        );
        // The single wrapping.
        let one = json!({"status": {"code": 200, "description": "success"}, "data": [1, 2]});
        assert_eq!(payload(text(&one.to_string())).ok().unwrap(), json!([1, 2]));
        // A row's own `status` ("1" = read) is not a wrapper.
        let row = json!({"status": "1", "messageId": "5"});
        assert_eq!(payload(text(&row.to_string())).ok().unwrap(), row);
        // Plain text is the server's error, even with isError false.
        let plain = text(
            "Invalid keys found in path variable. Kindly check allowed keys in tool schema and try again.",
        );
        match payload(plain) {
            Err(Fail::Answer(Error::Other(m))) => assert!(m.contains("Invalid keys found"), "{m}"),
            _ => panic!("plain text must be an error"),
        }
        let bad = json!({"result": {"isError": true, "content": [{"type": "text", "text": "{\"status\":{\"code\":401,\"description\":\"Invalid OAuth token\"}}"}]}});
        assert!(matches!(payload(bad), Err(Fail::Unauthorized)));
        let inner404 = json!({"status": "success", "data": {"requestId": "r", "status": {"code": 404, "description": "No such mail"}, "data": {}}});
        assert!(matches!(
            payload(text(&inner404.to_string())),
            Err(Fail::Answer(Error::NotFound(_)))
        ));
        let failed = json!({"status": "failure", "data": {"errorCode": "INVALID_DATA"}});
        assert!(matches!(
            payload(text(&failed.to_string())),
            Err(Fail::Answer(Error::Other(_)))
        ));
        // A parameter Zoho does not take: its errorCode, location and locationType are in the message,
        // and the call is Unsupported (never worth repeating as is), wherever Zoho put the details.
        for details in [
            json!([{"errorCode": "EXTRA_PARAM_FOUND", "location": "includeArchived", "locationType": "queryParam"}]),
            json!({"errorCode": "EXTRA_PARAM_FOUND", "location": "includeArchived", "locationType": "queryParam"}),
            json!(r#"{"errorCode":"EXTRA_PARAM_FOUND","location":"includeArchived","locationType":"queryParam"}"#),
        ] {
            let refused = json!({"headers": {}, "status": "failure", "data": {"requestId": "r",
                                 "status": {"code": 400, "description": "Bad Request"}, "data": details}});
            match payload(text(&refused.to_string())) {
                Err(Fail::Answer(Error::Unsupported(m))) => assert_eq!(
                    m,
                    "Zoho said 400 to t: Bad Request (errorCode EXTRA_PARAM_FOUND, location includeArchived, locationType queryParam)"
                ),
                _ => panic!("a refused parameter is Unsupported with its details"),
            }
        }
        // A 400 without details stays a plain error; odd characters never get through.
        let bare = json!({"status": {"code": 400, "description": "Bad Request"}, "data": {}});
        assert!(matches!(payload(text(&bare.to_string())), Err(Fail::Answer(Error::Other(_)))));
        assert_eq!(error_details(&json!({"errorCode": "A<b>\n c"})), vec![("errorCode", "Abc".to_string())]);
        let busy = json!({"result": {"isError": true, "content": [{"type": "text", "text": "{\"status\":{\"code\":500}}"}]}});
        assert!(matches!(payload(busy), Err(Fail::Transport(_))));
        let rpc = json!({"error": {"code": -32602, "message": "bad"}});
        assert!(matches!(payload(rpc), Err(Fail::Answer(Error::Invalid(_)))));
        assert_eq!(
            payload(json!({"result": {"content": null}})).ok().unwrap(),
            Value::Null
        );
        let unauth_text = text("Invalid OAuth token");
        assert!(matches!(payload(unauth_text), Err(Fail::Unauthorized)));
    }

    #[test]
    fn only_https_or_loopback() {
        assert!(check_url("https://mcp.example.test/x", "x").is_ok());
        assert!(check_url("http://127.0.0.1:9/x", "x").is_ok());
        assert!(check_url("http://mcp.example.test/x", "x").is_err());
        assert!(check_url("http://127.0.0.1.evil.example.test/", "x").is_err());
    }

    #[test]
    fn the_allowlist_has_no_admin_tools() {
        for t in ALLOWED_TOOLS {
            let l = t.to_ascii_lowercase();
            assert!(t.starts_with("ZohoMail_"), "{t}");
            for bad in [
                "user",
                "password",
                "domain",
                "policy",
                "group",
                "restriction",
                "delete",
                "empty",
                "org",
                "forward",
                "dkim",
            ] {
                assert!(!l.contains(bad), "{t} looks like an admin tool");
            }
        }
    }
}
