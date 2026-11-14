//! An in-process fake of Resend's API for tests (ours and the daemon's): plain HTTP on 127.0.0.1, one
//! request per connection, made-up mail only. It pages like Resend (newest first, `limit`, `after`,
//! `has_more`), checks the bearer key, honours `Idempotency-Key`, can answer 429 with Retry-After, drop
//! the connection after accepting a send, or act as a sending-only key. Download links point back at it
//! and record whether a request carried an `Authorization` header (it must not).

use crate::api::{ApiKeyInfo, DomainCapabilities, DomainInfo};
use crate::convert::civil;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The key the fake accepts unless a test changes it. Made up.
pub const FAKE_KEY: &str = "re_fake_test_key_0000000000";

#[derive(Debug, Clone, Default)]
pub struct FakeAttachment {
    pub id: String,
    pub filename: String,
    pub content_type: String,
    pub content_id: Option<String>,
    pub inline: bool,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct FakeSent {
    pub id: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub reply_to: Vec<String>,
    pub subject: String,
    pub html: Option<String>,
    pub text: Option<String>,
    pub created_ms: i64,
    pub scheduled_ms: Option<i64>,
    pub last_event: String,
    pub message_id: String,
    /// Custom headers the send asked for.
    pub headers: Map<String, Value>,
    pub attachments: Vec<FakeAttachment>,
}

#[derive(Debug, Clone, Default)]
pub struct FakeReceived {
    pub id: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub text: Option<String>,
    pub html: Option<String>,
    pub headers: Vec<(String, String)>,
    pub created_ms: i64,
    pub message_id: String,
    pub attachments: Vec<FakeAttachment>,
    pub raw: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logged {
    pub method: String,
    /// Path and query, e.g. `/emails?limit=100`.
    pub path: String,
    /// The request carried an `Authorization` header (with any value).
    pub authorization: bool,
    pub idempotency_key: Option<String>,
}

pub struct FakeState {
    pub key: String,
    pub sent: Vec<FakeSent>,
    pub received: Vec<FakeReceived>,
    pub domains: Vec<DomainInfo>,
    pub api_keys: Vec<ApiKeyInfo>,
    pub requests: Vec<Logged>,
    /// Answer the next n API calls with 429 `rate_limit_exceeded` and this Retry-After.
    pub rate_limit_next: u32,
    pub retry_after: String,
    /// Accept the next `POST /emails`, then close the connection without answering.
    pub drop_next_send: bool,
    /// Every list and read answers 401 `restricted_api_key` (a sending-only key); sends work.
    pub sending_only: bool,
    /// The creation time given to the next send (ms); moves forward by a second per send.
    pub clock_ms: i64,
    /// Idempotency key -> (request body, email id).
    pub idempotency: HashMap<String, (Value, String)>,
    next_id: u64,
    addr: Option<SocketAddr>,
}

impl FakeState {
    pub fn new() -> Self {
        Self {
            key: FAKE_KEY.into(),
            sent: Vec::new(),
            received: Vec::new(),
            domains: vec![DomainInfo {
                id: "d0000001-0000-4000-8000-000000000001".into(),
                name: "shop.example".into(),
                status: "verified".into(),
                region: Some("us-east-1".into()),
                capabilities: Some(DomainCapabilities { sending: Some("enabled".into()), receiving: Some("enabled".into()) }),
            }],
            api_keys: vec![ApiKeyInfo {
                id: "k0000001-0000-4000-8000-000000000001".into(),
                name: "shop website".into(),
                created_at: Some("2026-10-01 09:00:00.000000+00".into()),
                last_used_at: None,
            }],
            requests: Vec::new(),
            rate_limit_next: 0,
            retry_after: "1".into(),
            drop_next_send: false,
            sending_only: false,
            clock_ms: 1_794_211_200_000, // 2026-11-09 08:00 UTC
            idempotency: HashMap::new(),
            next_id: 1,
            addr: None,
        }
    }

    /// A new UUID-shaped id.
    pub fn new_id(&mut self) -> String {
        let n = self.next_id;
        self.next_id += 1;
        format!("{n:08x}-0000-4000-8000-{n:012x}")
    }

    /// Adds sent mail (newest last); returns its id.
    pub fn add_sent(&mut self, mut e: FakeSent) -> String {
        if e.id.is_empty() {
            e.id = self.new_id();
        }
        if e.message_id.is_empty() {
            e.message_id = format!("<{}@shop.example>", e.id);
        }
        if e.last_event.is_empty() {
            e.last_event = "delivered".into();
        }
        let id = e.id.clone();
        self.sent.push(e);
        id
    }

    pub fn add_received(&mut self, mut e: FakeReceived) -> String {
        if e.id.is_empty() {
            e.id = self.new_id();
        }
        if e.message_id.is_empty() {
            e.message_id = format!("<{}@sender.example>", e.id);
        }
        for a in &mut e.attachments {
            if a.id.is_empty() {
                let n = self.next_id;
                self.next_id += 1;
                a.id = format!("a{n:07x}-0000-4000-8000-{n:012x}");
            }
        }
        let id = e.id.clone();
        self.received.push(e);
        id
    }

    pub fn sent_by_id(&mut self, id: &str) -> Option<&mut FakeSent> {
        self.sent.iter_mut().find(|e| e.id == id)
    }

    /// API calls (downloads excluded) so far.
    pub fn api_calls(&self) -> usize {
        self.requests.iter().filter(|r| !r.path.starts_with("/_download/")).count()
    }
}

impl Default for FakeState {
    fn default() -> Self {
        Self::new()
    }
}

/// `2026-11-09 08:00:00.000000+00` (sent emails) or `2026-11-09T08:00:00.000Z` (received).
pub fn time_string(ms: i64, iso: bool) -> String {
    let (y, m, d, rem) = civil(ms);
    let (h, mi, s, frac) = (rem / 3600, rem % 3600 / 60, rem % 60, ms.rem_euclid(1000));
    if iso {
        format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{frac:03}Z")
    } else {
        format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}.{frac:03}000+00")
    }
}

pub struct FakeResend {
    pub addr: SocketAddr,
    state: Arc<Mutex<FakeState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeResend {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeResend {
    /// Starts the fake on a free loopback port (needs a tokio runtime).
    pub async fn start(state: FakeState) -> FakeResend {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let state = Arc::new(Mutex::new(FakeState { addr: Some(addr), ..state }));
        let s2 = state.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let s = s2.clone();
                tokio::spawn(async move {
                    let _ = serve(stream, s).await;
                });
            }
        });
        FakeResend { addr, state, task }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 || buf.len() > 1 << 20 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_owned();
    let path = first.next().unwrap_or("").to_owned();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())))
        .collect();
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok(Some(Request { method, path, headers, body }))
}

struct Reply {
    status: u16,
    body: Vec<u8>,
    content_type: &'static str,
    extra: Vec<(String, String)>,
    drop: bool,
}

fn ok(v: Value) -> Reply {
    Reply { status: 200, body: v.to_string().into_bytes(), content_type: "application/json", extra: Vec::new(), drop: false }
}

fn err(status: u16, name: &str, message: &str) -> Reply {
    let mut r = ok(json!({ "statusCode": status, "name": name, "message": message }));
    r.status = status;
    r
}

async fn serve(mut stream: TcpStream, state: Arc<Mutex<FakeState>>) -> std::io::Result<()> {
    let Some(req) = read_request(&mut stream).await? else { return Ok(()) };
    let reply = {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        handle(&mut s, &req)
    };
    if reply.drop {
        return Ok(());
    }
    let reason = match reply.status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        _ => "Status",
    };
    let mut head = format!(
        "HTTP/1.1 {} {reason}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
    );
    for (k, v) in &reply.extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&reply.body).await?;
    stream.shutdown().await
}

fn query(path: &str) -> (String, HashMap<String, String>) {
    let (p, q) = path.split_once('?').unwrap_or((path, ""));
    let params = q.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
    (p.to_owned(), params)
}

/// Resend's paging over `items` (newest first).
fn page(items: Vec<(String, Value)>, q: &HashMap<String, String>) -> Reply {
    let limit: usize = match q.get("limit") {
        None => 20,
        Some(l) => match l.parse::<usize>() {
            Ok(n) if (1..=100).contains(&n) => n,
            _ => return err(422, "validation_error", "limit must be between 1 and 100"),
        },
    };
    let start = match q.get("after") {
        None => 0,
        Some(a) => match items.iter().position(|(id, _)| id == a) {
            Some(i) => i + 1,
            None => return err(422, "validation_error", "invalid cursor"),
        },
    };
    let rest: Vec<Value> = items.into_iter().skip(start).map(|(_, v)| v).collect();
    let has_more = rest.len() > limit;
    ok(json!({ "object": "list", "has_more": has_more, "data": rest.into_iter().take(limit).collect::<Vec<_>>() }))
}

fn attachment_json(a: &FakeAttachment, base: &str, with_url: bool) -> Value {
    let mut v = json!({
        "id": a.id, "filename": a.filename, "content_type": a.content_type, "size": a.bytes.len(),
        "content_id": a.content_id, "content_disposition": if a.inline { "inline" } else { "attachment" },
    });
    if with_url {
        v["download_url"] = json!(format!("{base}/_download/att/{}", a.id));
        v["expires_at"] = json!("2026-11-09T09:00:00.000Z");
    }
    v
}

fn sent_json(e: &FakeSent, full: bool) -> Value {
    let mut v = json!({
        "object": "email", "id": e.id, "message_id": e.message_id, "from": e.from, "to": e.to, "cc": e.cc,
        "bcc": e.bcc, "reply_to": e.reply_to, "subject": e.subject, "created_at": time_string(e.created_ms, false),
        "scheduled_at": e.scheduled_ms.map(|t| time_string(t, false)), "last_event": e.last_event,
    });
    if full {
        v["html"] = json!(e.html);
        v["text"] = json!(e.text);
        v["tags"] = json!([]);
    }
    v
}

fn received_json(e: &FakeReceived, base: &str, full: bool) -> Value {
    let mut v = json!({
        "object": "email", "id": e.id, "message_id": e.message_id, "from": e.from, "to": e.to, "cc": e.cc,
        "bcc": [], "reply_to": [], "subject": e.subject, "created_at": time_string(e.created_ms, true),
        "attachments": e.attachments.iter().map(|a| attachment_json(a, base, false)).collect::<Vec<_>>(),
    });
    if full {
        let mut headers = Map::new();
        for (k, val) in &e.headers {
            let k = k.to_ascii_lowercase();
            match headers.get_mut(&k) {
                Some(Value::Array(a)) => a.push(json!(val)),
                Some(old) => *old = json!([old.clone(), val]),
                None => {
                    headers.insert(k, json!(val));
                }
            }
        }
        v["html"] = json!(e.html);
        v["text"] = json!(e.text);
        v["headers"] = Value::Object(headers);
        v["html_format"] = json!("cid");
        v["received_for"] = json!(e.to);
        v["authentication"] = json!({ "spf": "pass", "dkim": "pass", "dmarc": "pass" });
        v["raw"] = match &e.raw {
            Some(_) => json!({ "download_url": format!("{base}/_download/raw/{}", e.id), "expires_at": "2026-11-09T09:00:00.000Z" }),
            None => Value::Null,
        };
    }
    v
}

fn handle(s: &mut FakeState, req: &Request) -> Reply {
    let authorization = req.headers.get("authorization").cloned();
    s.requests.push(Logged {
        method: req.method.clone(),
        path: req.path.clone(),
        authorization: authorization.is_some(),
        idempotency_key: req.headers.get("idempotency-key").cloned(),
    });
    let base = s.addr.map(|a| format!("http://{a}")).unwrap_or_default();
    let (path, q) = query(&req.path);
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();

    if parts.first() == Some(&"_download") {
        let bytes = match parts.as_slice() {
            ["_download", "raw", id] => s.received.iter().find(|e| e.id == *id).and_then(|e| e.raw.clone()),
            ["_download", "att", id] => s
                .received
                .iter()
                .flat_map(|e| &e.attachments)
                .chain(s.sent.iter().flat_map(|e| &e.attachments))
                .find(|a| a.id == *id)
                .map(|a| a.bytes.clone()),
            _ => None,
        };
        return match bytes {
            Some(b) => Reply { status: 200, body: b, content_type: "application/octet-stream", extra: Vec::new(), drop: false },
            None => err(404, "not_found", "no such download"),
        };
    }

    if authorization.as_deref() != Some(&format!("Bearer {}", s.key)) {
        return if authorization.is_none() {
            err(401, "missing_api_key", "Missing API key in the authorization header")
        } else {
            err(401, "validation_error", "API key is invalid")
        };
    }
    if s.rate_limit_next > 0 {
        s.rate_limit_next -= 1;
        let mut r = err(429, "rate_limit_exceeded", "Too many requests. You can only make 10 requests per second.");
        r.extra.push(("retry-after".into(), s.retry_after.clone()));
        r.extra.push(("ratelimit-limit".into(), "10".into()));
        r.extra.push(("ratelimit-remaining".into(), "0".into()));
        return r;
    }
    let restricted = || err(401, "restricted_api_key", "This API key is restricted to only send emails");

    match (req.method.as_str(), parts.as_slice()) {
        ("POST", ["emails"]) => send(s, req),
        _ if s.sending_only => restricted(),
        ("GET", ["emails"]) => {
            let mut items: Vec<&FakeSent> = s.sent.iter().collect();
            items.sort_by_key(|e| std::cmp::Reverse(e.created_ms));
            page(items.into_iter().map(|e| (e.id.clone(), sent_json(e, false))).collect(), &q)
        }
        ("GET", ["emails", "receiving"]) => {
            let mut items: Vec<&FakeReceived> = s.received.iter().collect();
            items.sort_by_key(|e| std::cmp::Reverse(e.created_ms));
            page(items.into_iter().map(|e| (e.id.clone(), received_json(e, &base, false))).collect(), &q)
        }
        ("GET", ["emails", "receiving", id]) => match s.received.iter().find(|e| e.id == *id) {
            Some(e) => ok(received_json(e, &base, true)),
            None => err(404, "not_found", "Email not found"),
        },
        ("GET", ["emails", "receiving", id, "attachments", aid]) => {
            match s.received.iter().find(|e| e.id == *id).and_then(|e| e.attachments.iter().find(|a| a.id == *aid)) {
                Some(a) => ok(attachment_json(a, &base, true)),
                None => err(404, "not_found", "Attachment not found"),
            }
        }
        ("GET", ["emails", id]) => match s.sent.iter().find(|e| e.id == *id) {
            Some(e) => ok(sent_json(e, true)),
            None => err(404, "not_found", "Email not found"),
        },
        ("GET", ["emails", id, "attachments"]) => match s.sent.iter().find(|e| e.id == *id) {
            Some(e) => {
                let items = e.attachments.iter().map(|a| (a.id.clone(), attachment_json(a, &base, true))).collect();
                page(items, &q)
            }
            None => err(404, "not_found", "Email not found"),
        },
        ("GET", ["emails", id, "attachments", aid]) => {
            match s.sent.iter().find(|e| e.id == *id).and_then(|e| e.attachments.iter().find(|a| a.id == *aid)) {
                Some(a) => ok(attachment_json(a, &base, true)),
                None => err(404, "not_found", "Attachment not found"),
            }
        }
        ("POST", ["emails", id, "cancel"]) => match s.sent_by_id(id) {
            Some(e) if e.last_event == "scheduled" => {
                e.last_event = "canceled".into();
                ok(json!({ "object": "email", "id": e.id }))
            }
            Some(_) => err(422, "validation_error", "Only scheduled emails can be canceled"),
            None => err(404, "not_found", "Email not found"),
        },
        ("GET", ["domains"]) => ok(json!({ "object": "list", "has_more": false, "data": s.domains })),
        ("GET", ["api-keys"]) => ok(json!({ "object": "list", "has_more": false, "data": s.api_keys })),
        _ => err(404, "not_found", "The requested endpoint does not exist."),
    }
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect(),
        _ => Vec::new(),
    }
}

fn send(s: &mut FakeState, req: &Request) -> Reply {
    let Ok(body) = serde_json::from_slice::<Value>(&req.body) else {
        return err(422, "validation_error", "Invalid JSON");
    };
    for f in ["from", "to", "subject"] {
        if body.get(f).is_none() {
            return err(422, "missing_required_field", &format!("Missing `{f}` field."));
        }
    }
    let key = req.headers.get("idempotency-key").cloned();
    if let Some(k) = &key
        && let Some((prev, id)) = s.idempotency.get(k)
    {
        return if *prev == body {
            ok(json!({ "id": id }))
        } else {
            err(409, "invalid_idempotent_request", "This idempotency key has already been used on a request that had a different payload.")
        };
    }
    let id = s.new_id();
    s.clock_ms += 1000;
    let attachments = body
        .get("attachments")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .enumerate()
                .map(|(i, a)| {
                    use base64::Engine;
                    let bytes = base64::engine::general_purpose::STANDARD.decode(a["content"].as_str().unwrap_or("")).unwrap_or_default();
                    FakeAttachment {
                        id: format!("{}{i:x}", &id[..id.len() - 1]),
                        filename: a["filename"].as_str().unwrap_or("").into(),
                        content_type: a["content_type"].as_str().unwrap_or("").into(),
                        bytes,
                        ..FakeAttachment::default()
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let e = FakeSent {
        id: id.clone(),
        from: body["from"].as_str().unwrap_or("").into(),
        to: strings(&body["to"]),
        cc: strings(&body["cc"]),
        bcc: strings(&body["bcc"]),
        reply_to: strings(&body["reply_to"]),
        subject: body["subject"].as_str().unwrap_or("").into(),
        html: body["html"].as_str().map(str::to_owned),
        text: body["text"].as_str().map(str::to_owned),
        created_ms: s.clock_ms,
        scheduled_ms: None,
        last_event: "sent".into(),
        message_id: format!("<{id}@shop.example>"),
        headers: body.get("headers").and_then(Value::as_object).cloned().unwrap_or_default(),
        attachments,
    };
    s.sent.push(e);
    if let Some(k) = key {
        s.idempotency.insert(k, (body, id.clone()));
    }
    let mut r = ok(json!({ "id": id }));
    if s.drop_next_send {
        s.drop_next_send = false;
        r.drop = true;
    }
    r
}
