//! An in-process fake of Zoho's MCP server and token endpoint for tests (ours and the daemon's): plain
//! HTTP on 127.0.0.1, one request per connection, made-up mail only.
//!
//! - `POST /mcp`: JSON-RPC `tools/call` for the mail tools this crate uses, answered as Zoho Mail's REST
//!   JSON (`{status, data}`, ids and times as strings) inside MCP `content[0].text`, either as a JSON
//!   body or as an SSE stream ([`ReplyMode`]). Any other tool is a JSON-RPC error and is logged.
//!   Checks the Bearer token; can require an MCP session (`initialize` first), answer 429, 500, or 401
//!   (as HTTP or inside the tool result), and answer `null` for empty lists and missing fields.
//! - `POST /oauth/token`: the refresh grant; checks the refresh token, client id and `resource`, can
//!   rotate the refresh token.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Made up.
pub const FAKE_CLIENT_ID: &str = "fake-client-0001";
pub const FAKE_ACCESS: &str = "fake-access-0";
pub const FAKE_REFRESH: &str = "fake-refresh-0";

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyMode {
    Json,
    Sse,
}

#[derive(Debug, Clone, Default)]
pub struct FakeAttachment {
    pub id: String,
    pub name: String,
    pub bytes: Vec<u8>,
    pub cid: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct FakeMail {
    pub id: String,
    /// Zoho account id.
    pub account: String,
    pub thread: Option<String>,
    pub folder: String,
    pub archived: bool,
    pub subject: String,
    pub from: String,
    pub sender: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub received_ms: i64,
    pub read: bool,
    /// `flag_not_set`, `info`, `important`, `followup`.
    pub flag: String,
    pub summary: String,
    pub html: String,
    /// Extra header lines (`Name: value`).
    pub headers: Vec<String>,
    pub attachments: Vec<FakeAttachment>,
}

#[derive(Debug, Clone)]
pub struct FakeAccount {
    pub id: String,
    pub email: String,
    /// Other addresses of the same mailbox (`emailAlias`).
    pub aliases: Vec<String>,
    /// (id, name, folderType)
    pub folders: Vec<(String, String, String)>,
}

impl FakeAccount {
    /// An account with Inbox, Drafts, Sent, Spam, Trash, Outbox and a user folder "Projects". Folder ids
    /// are `<n>01` .. `<n>07` for account id `<n>`.
    pub fn standard(id: &str, email: &str) -> Self {
        let f =
            |k: u32, name: &str, ty: &str| (format!("{id}0{k}"), name.to_owned(), ty.to_owned());
        Self {
            id: id.into(),
            email: email.into(),
            aliases: Vec::new(),
            folders: vec![
                f(1, "Inbox", "Inbox"),
                f(2, "Drafts", "Drafts"),
                f(3, "Sent", "Sent"),
                f(4, "Spam", "Spam"),
                f(5, "Trash", "Trash"),
                f(6, "Outbox", "Outbox"),
                f(7, "Projects", "Inbox"),
            ],
        }
    }
    pub fn folder(&self, ty_or_name: &str) -> String {
        self.folders
            .iter()
            .find(|(_, n, t)| {
                n.eq_ignore_ascii_case(ty_or_name)
                    || (t.eq_ignore_ascii_case(ty_or_name) && n.eq_ignore_ascii_case(t))
            })
            .map(|(i, _, _)| i.clone())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Logged {
    pub tool: String,
    pub account: Option<String>,
    pub args: Value,
}

pub struct FakeState {
    pub accounts: Vec<FakeAccount>,
    pub mails: Vec<FakeMail>,
    pub mode: ReplyMode,
    pub access_token: String,
    pub refresh_token: String,
    pub client_id: String,
    /// Give out a new refresh token with each refresh.
    pub rotate_refresh: bool,
    pub expires_in: u64,
    /// The token endpoint refuses (`{"error": "invalid_code"}` with 200, as Zoho does).
    pub refuse_refresh: bool,
    /// Tool calls (allowed or not) in order.
    pub calls: Vec<Logged>,
    /// Form fields of each token request.
    pub token_requests: Vec<HashMap<String, String>>,
    /// Answer the next n tool calls with 429 / 500 / 401.
    pub rate_limit_next: u32,
    pub retry_after: String,
    pub fail_next: u32,
    /// With `fail_next`: only calls of this tool fail.
    pub fail_only: Option<String>,
    pub unauthorized_next: u32,
    /// A bad token is reported inside the tool result (200, isError) instead of as HTTP 401.
    pub auth_error_in_body: bool,
    /// Empty lists and missing fields come back as `null`.
    pub null_lists: bool,
    /// Answer the next tool call with this plain text (and `isError: false`), as Zoho does for errors.
    pub plain_error_next: Option<String>,
    /// Answer with one `{status, data}` wrapping instead of Zoho's two.
    pub single_wrap: bool,
    /// Requests counted, for `requestId`s.
    pub requests: u64,
    /// tools/call needs an `mcp-session-id` from `initialize`.
    pub require_session: bool,
    /// Query parameters `list_mails` refuses as live Zoho does `includeArchived`: a 400 whose details
    /// (`errorCode` EXTRA_PARAM_FOUND, `location`, `locationType`) sit in a list under the inner `data`.
    pub refuse_params: Vec<String>,
    pub session: Option<String>,
    pub sends: Vec<Value>,
    next_id: u64,
    next_token: u64,
    addr: Option<SocketAddr>,
}

impl FakeState {
    pub fn new(accounts: Vec<FakeAccount>) -> Self {
        Self {
            accounts,
            mails: Vec::new(),
            mode: ReplyMode::Json,
            access_token: FAKE_ACCESS.into(),
            refresh_token: FAKE_REFRESH.into(),
            client_id: FAKE_CLIENT_ID.into(),
            rotate_refresh: false,
            expires_in: 3600,
            refuse_refresh: false,
            calls: Vec::new(),
            token_requests: Vec::new(),
            rate_limit_next: 0,
            retry_after: "0".into(),
            fail_next: 0,
            fail_only: None,
            unauthorized_next: 0,
            auth_error_in_body: false,
            null_lists: false,
            plain_error_next: None,
            single_wrap: false,
            requests: 0,
            require_session: false,
            refuse_params: Vec::new(),
            session: None,
            sends: Vec::new(),
            next_id: 1,
            next_token: 1,
            addr: None,
        }
    }

    /// Adds a mail; fills in id (`17942...` digits), thread, flag and html when empty. Returns the id.
    pub fn add(&mut self, mut m: FakeMail) -> String {
        if m.id.is_empty() {
            m.id = format!("{}{:06}", 1_794_000_000_000u64, self.next_id);
            self.next_id += 1;
        }
        if m.flag.is_empty() {
            m.flag = "flag_not_set".into();
        }
        if m.html.is_empty() {
            m.html = format!("<div>{}</div>", m.summary);
        }
        for a in &mut m.attachments {
            if a.id.is_empty() {
                a.id = format!("{}{:04}", 5_000_000, self.next_id);
                self.next_id += 1;
            }
        }
        let id = m.id.clone();
        self.mails.push(m);
        id
    }

    pub fn mail(&mut self, id: &str) -> Option<&mut FakeMail> {
        self.mails.iter_mut().find(|m| m.id == id)
    }

    /// Tool calls so far, by name.
    pub fn count(&self, tool: &str) -> usize {
        self.calls.iter().filter(|c| c.tool == tool).count()
    }

    fn raw(&self, m: &FakeMail) -> String {
        let mut s = format!(
            "From: {} <{}>\r\nTo: {}\r\nSubject: {}\r\nMessage-ID: <{}@fake.example.test>\r\nDate: Mon, 9 Nov 2026 08:00:00 +0000\r\n",
            m.sender,
            m.from,
            m.to.join(", "),
            m.subject,
            m.id
        );
        for h in &m.headers {
            s.push_str(h);
            s.push_str("\r\n");
        }
        if m.attachments.is_empty() {
            s.push_str("Content-Type: text/html; charset=utf-8\r\n\r\n");
            s.push_str(&m.html);
            return s;
        }
        s.push_str("MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b0\"\r\n\r\n--b0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n");
        s.push_str(&m.html);
        s.push_str("\r\n");
        for a in &m.attachments {
            let disp = if a.cid.is_some() {
                "inline"
            } else {
                "attachment"
            };
            s.push_str(&format!(
                "--b0\r\nContent-Type: application/octet-stream; name=\"{n}\"\r\nContent-Disposition: {disp}; filename=\"{n}\"\r\nContent-Transfer-Encoding: base64\r\n",
                n = a.name
            ));
            if let Some(c) = &a.cid {
                s.push_str(&format!("Content-ID: <{c}>\r\n"));
            }
            s.push_str("\r\n");
            s.push_str(&b64(&a.bytes));
            s.push_str("\r\n");
        }
        s.push_str("--b0--\r\n");
        s
    }
}

fn b64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub struct FakeZoho {
    pub addr: SocketAddr,
    state: Arc<Mutex<FakeState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeZoho {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeZoho {
    pub async fn start(state: FakeState) -> FakeZoho {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let state = Arc::new(Mutex::new(FakeState {
            addr: Some(addr),
            ..state
        }));
        let s2 = state.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let s = s2.clone();
                tokio::spawn(async move {
                    let _ = serve(stream, s).await;
                });
            }
        });
        FakeZoho { addr, state, task }
    }

    pub fn mcp_url(&self) -> String {
        format!("http://{}/mcp/message", self.addr)
    }

    pub fn token_url(&self) -> String {
        format!("http://{}/oauth/token", self.addr)
    }

    pub fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Writes a token file for this fake (mode 0600) and returns its path.
    pub fn write_token_file(&self, dir: &std::path::Path, expires_in: u64) -> std::path::PathBuf {
        let p = dir.join("zoho_tokens.json");
        let s = self.lock();
        let v = json!({
            "access_token": s.access_token, "refresh_token": s.refresh_token, "expires_in": expires_in,
            "token_type": "Bearer", "scope": "made-up.scope", "api_domain": "https://api.example.test",
        });
        std::fs::write(&p, serde_json::to_vec_pretty(&v).unwrap_or_default()).expect("token file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
        p
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
        .filter_map(|l| {
            l.split_once(':')
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        })
        .collect();
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok(Some(Request {
        method,
        path,
        headers,
        body,
    }))
}

struct Reply {
    status: u16,
    body: String,
    content_type: &'static str,
    extra: Vec<(String, String)>,
}

fn reply(status: u16, v: Value) -> Reply {
    Reply {
        status,
        body: v.to_string(),
        content_type: "application/json",
        extra: Vec::new(),
    }
}

async fn serve(mut stream: TcpStream, state: Arc<Mutex<FakeState>>) -> std::io::Result<()> {
    let Some(req) = read_request(&mut stream).await? else {
        return Ok(());
    };
    let r = {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        handle(&mut s, &req)
    };
    let mut head = format!(
        "HTTP/1.1 {} X\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n",
        r.status,
        r.content_type,
        r.body.len()
    );
    for (k, v) in &r.extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(r.body.as_bytes()).await?;
    stream.shutdown().await
}

fn form(body: &[u8]) -> HashMap<String, String> {
    let dec = |s: &str| {
        let s = s.replace('+', " ");
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%'
                && i + 2 < b.len()
                && let Ok(x) = u8::from_str_radix(&s[i + 1..i + 3], 16)
            {
                out.push(x);
                i += 3;
                continue;
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    String::from_utf8_lossy(body)
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (dec(k), dec(v)))
        .collect()
}

fn handle(s: &mut FakeState, req: &Request) -> Reply {
    let base = s.addr.map(|a| format!("http://{a}")).unwrap_or_default();
    if req.method == "POST" && req.path == "/oauth/token" {
        let f = form(&req.body);
        s.token_requests.push(f.clone());
        let good = f.get("grant_type").map(String::as_str) == Some("refresh_token")
            && f.get("refresh_token") == Some(&s.refresh_token)
            && f.get("client_id") == Some(&s.client_id)
            && f.get("resource").map(String::as_str)
                == Some(format!("{base}/mcp/message").as_str());
        if !good || s.refuse_refresh {
            return reply(200, json!({ "error": "invalid_code" }));
        }
        let n = s.next_token;
        s.next_token += 1;
        s.access_token = format!("fake-access-{n}");
        let mut v = json!({
            "access_token": s.access_token, "expires_in": s.expires_in, "token_type": "Bearer",
            "api_domain": "https://api.example.test", "scope": "made-up.scope",
        });
        if s.rotate_refresh {
            s.refresh_token = format!("fake-refresh-{n}");
            v["refresh_token"] = json!(s.refresh_token);
        }
        return reply(200, v);
    }
    if req.method != "POST" || req.path != "/mcp/message" {
        return reply(404, json!({ "error": "no such endpoint" }));
    }
    let Ok(rpc) = serde_json::from_slice::<Value>(&req.body) else {
        return reply(
            400,
            json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "Parse error" } }),
        );
    };
    let id = rpc.get("id").cloned().unwrap_or(Value::Null);
    let method = rpc.get("method").and_then(Value::as_str).unwrap_or("");
    let bearer = req
        .headers
        .get("authorization")
        .cloned()
        .unwrap_or_default();
    let forced = s.unauthorized_next > 0 && method == "tools/call";
    if bearer != format!("Bearer {}", s.access_token) || forced {
        if forced {
            s.unauthorized_next -= 1;
        }
        if s.auth_error_in_body && method == "tools/call" {
            return wrap(
                s.mode,
                &id,
                json!({ "isError": true, "content": [{ "type": "text", "text": "{\"status\":{\"code\":401,\"description\":\"INVALID_OAUTHTOKEN\"}}" }] }),
            );
        }
        let mut r = reply(401, json!({ "error": "invalid_token" }));
        r.extra.push((
            "www-authenticate".into(),
            "Bearer error=\"invalid_token\"".into(),
        ));
        return r;
    }
    match method {
        "initialize" => {
            s.session = Some("fake-session-1".into());
            let mut r = wrap(
                s.mode,
                &id,
                json!({ "protocolVersion": "2025-03-26", "capabilities": { "tools": {} }, "serverInfo": { "name": "fake-zoho" } }),
            );
            r.extra
                .push(("mcp-session-id".into(), "fake-session-1".into()));
            return r;
        }
        "notifications/initialized" => {
            return Reply {
                status: 202,
                body: String::new(),
                content_type: "application/json",
                extra: Vec::new(),
            };
        }
        "tools/call" => {}
        _ => {
            return reply(
                200,
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found" } }),
            );
        }
    }
    if s.require_session && req.headers.get("mcp-session-id") != s.session.as_ref() {
        return reply(
            400,
            json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32000, "message": "Bad Request: No valid session ID provided" } }),
        );
    }
    let params = rpc.get("params").cloned().unwrap_or(Value::Null);
    let tool = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let account = args
        .pointer("/path_variables/accountId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    s.calls.push(Logged {
        tool: tool.clone(),
        account: account.clone(),
        args: args.clone(),
    });
    if s.rate_limit_next > 0 {
        s.rate_limit_next -= 1;
        let mut r = reply(429, json!({ "error": "too many requests" }));
        r.extra.push(("retry-after".into(), s.retry_after.clone()));
        return r;
    }
    if s.fail_next > 0 && s.fail_only.as_ref().is_none_or(|t| *t == tool) {
        s.fail_next -= 1;
        return reply(503, json!({ "error": "unavailable" }));
    }
    if let Err(e) = crate::schema::check(&tool, &args)
        && crate::mcp::ALLOWED_TOOLS.contains(&tool.as_str())
    {
        // Zoho answers bad arguments in plain text, with isError false.
        let text = if e.to_string().contains("path_variables") {
            "Invalid keys found in path variable. Kindly check allowed keys in tool schema and try again."
        } else {
            "Invalid input found. Kindly check the tool schema and try again."
        };
        return wrap(
            s.mode,
            &id,
            json!({ "isError": false, "content": [{ "type": "text", "text": text }] }),
        );
    }
    if let Some(text) = s.plain_error_next.take() {
        return wrap(
            s.mode,
            &id,
            json!({ "isError": false, "content": [{ "type": "text", "text": text }] }),
        );
    }
    let result = tool_call(s, &tool, account.as_deref(), &args);
    s.requests += 1;
    let n = s.requests;
    let single = s.single_wrap;
    match result {
        Some(Out::Data(data)) => {
            let inner = json!({ "requestId": format!("fake-request-{n}"), "status": { "code": 200, "description": "OK" }, "data": data });
            let body = if single {
                json!({ "status": { "code": 200, "description": "success" }, "data": data })
            } else {
                let len = inner.to_string().len();
                json!({ "headers": { "content-length": len.to_string(), "content-type": "application/json;charset=UTF-8" }, "status": "success", "data": inner })
            };
            wrap(s.mode, &id, text_result(body))
        }
        Some(Out::Raw(r)) => wrap(s.mode, &id, r),
        None => reply(
            200,
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("Unknown tool: {tool}") } }),
        ),
    }
}

/// A JSON-RPC response with `result`, as JSON or as SSE (after a progress notification).
fn wrap(mode: ReplyMode, id: &Value, result: Value) -> Reply {
    let msg = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    match mode {
        ReplyMode::Json => reply(200, msg),
        ReplyMode::Sse => {
            let note = json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": { "progress": 1 } });
            let body = format!(
                "event: message\r\ndata: {note}\r\n\r\nid: 1\r\nevent: message\r\ndata: {msg}\r\n\r\n"
            );
            Reply {
                status: 200,
                body,
                content_type: "text/event-stream",
                extra: Vec::new(),
            }
        }
    }
}

fn text_result(v: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": v.to_string() }] })
}

/// What a tool answers: Zoho's `data` (wrapped by the caller as the server does), or a whole MCP result.
enum Out {
    Data(Value),
    Raw(Value),
}

fn ok(data: Value) -> Out {
    Out::Data(data)
}

/// Zoho's answer to a parameter it does not take: the usual double wrapping, `status.code` 400 inside,
/// the details as a list.
fn param_refused(param: &str) -> Out {
    let inner = json!({ "requestId": "fake-refused", "status": { "code": 400, "description": "Bad Request" },
                        "data": [{ "errorCode": "EXTRA_PARAM_FOUND", "location": param, "locationType": "queryParam" }] });
    Out::Raw(json!({ "isError": true, "content": [{ "type": "text", "text": json!({ "headers": {}, "status": "failure", "data": inner }).to_string() }] }))
}

fn tool_error(code: u16, desc: &str) -> Out {
    Out::Raw(
        json!({ "isError": true, "content": [{ "type": "text", "text": json!({ "status": { "code": code, "description": desc } }).to_string() }] }),
    )
}

fn list_or_null(null_lists: bool, v: Vec<Value>) -> Value {
    if null_lists && v.is_empty() {
        Value::Null
    } else {
        Value::Array(v)
    }
}

fn row(m: &FakeMail, nulls: bool) -> Value {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let to =
        m.to.iter()
            .map(|t| format!("&lt;{}&gt;", esc(t)))
            .collect::<Vec<_>>()
            .join(",");
    let mut v = json!({
        "messageId": m.id, "folderId": m.folder, "subject": esc(&m.subject), "fromAddress": m.from, "sender": m.sender,
        "toAddress": to, "ccAddress": if m.cc.is_empty() { "Not Provided".to_owned() } else { m.cc.join(",") },
        "receivedTime": m.received_ms.to_string(), "sentDateInGMT": m.received_ms.to_string(),
        "status": if m.read { "1" } else { "0" }, "status2": "0", "flagId": m.flag, "priority": "3",
        "hasAttachment": if m.attachments.iter().any(|a| a.cid.is_none()) { "1" } else { "0" }, "hasInline": "false",
        "summary": m.summary, "size": (m.html.len() + 300).to_string(), "calendarType": "0",
    });
    if nulls {
        if m.to.is_empty() {
            v["toAddress"] = Value::Null;
        }
        if m.summary.is_empty() {
            v["summary"] = Value::Null;
        }
    }
    v
}

fn ids(args: &Value) -> Vec<String> {
    args.pointer("/body/messageIds")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn tool_call(s: &mut FakeState, tool: &str, account: Option<&str>, args: &Value) -> Option<Out> {
    let nulls = s.null_lists;
    let q = |k: &str| {
        args.pointer(&format!("/query_params/{k}"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let mid = args
        .pointer("/path_variables/messageId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let acc = s
        .accounts
        .iter()
        .find(|a| Some(a.id.as_str()) == account)
        .cloned();
    let mine = |m: &FakeMail| Some(m.account.as_str()) == account;
    let not_found = || tool_error(404, "Message not found");
    Some(match tool {
        "ZohoMail_list_accounts" => {
            // One sign-in, one mailbox: `primaryAccount` (address a string, aliases a list). More accounts
            // (a fake-only case) go under `otherAccounts`.
            let one = |a: &FakeAccount| {
                json!({ "accountId": a.id, "emailAddress": a.email, "primaryEmailAddress": a.email, "mailboxAddress": a.email,
                        "incomingUsername": a.email, "emailAlias": a.aliases, "sendMailAddress": a.email, "displayName": "Made Up",
                        "accountType": "ZOHO_ACCOUNT", "isDefaultAccount": true, "zuid": 5, "lastLogin": 1, "status": true })
            };
            let mut d =
                json!({ "primaryAccount": s.accounts.first().map(one).unwrap_or(Value::Null) });
            if s.accounts.len() > 1 {
                d["otherAccounts"] = Value::Array(s.accounts.iter().skip(1).map(one).collect());
            }
            ok(d)
        }
        "ZohoMail_list_folders" => {
            let Some(a) = acc else {
                return Some(tool_error(400, "Invalid account"));
            };
            let list = list_or_null(
                nulls,
                a.folders.iter().map(|(i, n, t)| json!({ "folderId": i, "folderName": n, "folderType": t, "path": format!("/{n}"), "isArchived": false, "isVisibleInWEB": true, "isVisibleInIMAP": true, "folderIcon": "x" })).collect(),
            );
            ok(json!({ "folders": list }))
        }
        "ZohoMail_list_mails" => {
            if let Some(p) = s.refuse_params.iter().find(|p| !q(p).is_null()) {
                return Some(param_refused(p));
            }
            let archived = q("includeArchived").as_bool().unwrap_or(false);
            let folder = q("folderId").as_str().map(str::to_owned);
            let start = match q("pageToken")
                .as_str()
                .and_then(|t| t.strip_prefix("fake-page-"))
            {
                Some(n) => n.parse::<usize>().unwrap_or(1),
                None => q("start").as_u64().unwrap_or(1).max(1) as usize,
            };
            let fields: Option<Vec<String>> = q("fields")
                .as_str()
                .map(|f| f.split(',').map(|x| x.trim().to_owned()).collect());
            let limit = q("limit").as_u64().unwrap_or(10).clamp(1, 200) as usize;
            let mut list: Vec<&FakeMail> = s
                .mails
                .iter()
                .filter(|m| {
                    mine(m)
                        && if archived {
                            m.archived
                        } else {
                            !m.archived && Some(&m.folder) == folder.as_ref()
                        }
                })
                .collect();
            list.sort_by_key(|m| std::cmp::Reverse(m.received_ms));
            let total = list.len();
            let rows: Vec<Value> = list
                .into_iter()
                .skip(start - 1)
                .take(limit)
                .map(|m| {
                    let mut r = row(m, nulls);
                    // Like the live server: named fields drop the flag (and there is never a threadId).
                    if let (Some(f), Value::Object(o)) = (&fields, &mut r) {
                        o.retain(|k, _| f.contains(k) && k != "flagId");
                    }
                    r
                })
                .collect();
            let mut d = json!({ "messages": list_or_null(nulls, rows) });
            if start - 1 + limit < total {
                d["nextPageToken"] = json!(format!("fake-page-{}", start + limit));
            }
            ok(d)
        }
        "ZohoMail_get_mail" => match s.mails.iter().find(|m| mine(m) && m.id == mid) {
            Some(m) => ok(row(m, nulls)),
            None => not_found(),
        },
        "ZohoMail_get_mail_html" => match s.mails.iter().find(|m| mine(m) && m.id == mid) {
            Some(m) => ok(json!({ "messageId": m.id, "content": m.html })),
            None => not_found(),
        },
        "ZohoMail_get_mail_headers" => match s.mails.iter().find(|m| mine(m) && m.id == mid) {
            Some(m) => {
                let raw = s.raw(m);
                let head = raw.split("\r\n\r\n").next().unwrap_or("").to_owned();
                ok(json!({ "messageId": m.id, "headerContent": head }))
            }
            None => not_found(),
        },
        "ZohoMail_get_mail_original" => match s.mails.iter().find(|m| mine(m) && m.id == mid) {
            Some(m) => ok(json!({ "originalContent": s.raw(m) })),
            None => not_found(),
        },
        "ZohoMail_list_mail_attachments" => match s.mails.iter().find(|m| mine(m) && m.id == mid) {
            Some(m) => {
                let one = |a: &FakeAttachment| {
                    let mut v = json!({ "attachmentId": a.id, "attachmentName": a.name, "attachmentSize": a.bytes.len() });
                    if let Some(c) = &a.cid {
                        v["cid"] = json!(c);
                    }
                    v
                };
                let att: Vec<Value> = m
                    .attachments
                    .iter()
                    .filter(|a| a.cid.is_none())
                    .map(one)
                    .collect();
                let inl: Vec<Value> = m
                    .attachments
                    .iter()
                    .filter(|a| a.cid.is_some())
                    .map(one)
                    .collect();
                ok(
                    json!({ "messageId": m.id, "attachments": list_or_null(nulls, att), "inline": list_or_null(nulls, inl) }),
                )
            }
            None => not_found(),
        },
        "ZohoMail_mark_mails_as_read"
        | "ZohoMail_mark_mails_as_unread"
        | "ZohoMail_apply_mail_flag"
        | "ZohoMail_move_mails"
        | "ZohoMail_archive_mails"
        | "ZohoMail_unarchive_mails"
        | "ZohoMail_move_mails_to_trash" => {
            let Some(a) = acc else {
                return Some(tool_error(400, "Invalid account"));
            };
            let trash = a.folder("Trash");
            let flag = match args.pointer("/body/flagId").and_then(Value::as_i64) {
                Some(1) => "info",
                Some(2) => "important",
                Some(3) => "followup",
                _ => "flag_not_set",
            };
            let dest = args
                .pointer("/body/destinationFolderId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            for id in ids(args) {
                let Some(m) = s.mails.iter_mut().find(|m| m.account == a.id && m.id == id) else {
                    continue;
                };
                match tool {
                    "ZohoMail_mark_mails_as_read" => m.read = true,
                    "ZohoMail_mark_mails_as_unread" => m.read = false,
                    "ZohoMail_apply_mail_flag" => m.flag = flag.into(),
                    "ZohoMail_move_mails" => m.folder = dest.clone().unwrap_or_default(),
                    "ZohoMail_archive_mails" => m.archived = true,
                    "ZohoMail_unarchive_mails" => m.archived = false,
                    _ => m.folder = trash.clone(),
                }
            }
            ok(json!({}))
        }
        "ZohoMail_send_mail" | "ZohoMail_reply_to_mail" => {
            let Some(a) = acc else {
                return Some(tool_error(400, "Invalid account"));
            };
            let body = args.get("body").cloned().unwrap_or(Value::Null);
            s.sends
                .push(json!({ "tool": tool, "account": a.id, "body": body, "parent": mid }));
            let text = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
            let thread = s
                .mails
                .iter()
                .find(|m| m.id == mid)
                .map(|m| m.thread.clone().unwrap_or(m.id.clone()));
            let mail = FakeMail {
                account: a.id.clone(),
                folder: a.folder("Sent"),
                thread,
                subject: text("subject"),
                from: text("fromAddress"),
                sender: "Made Up".into(),
                to: text("toAddress")
                    .split(',')
                    .filter(|x| !x.is_empty())
                    .map(str::to_owned)
                    .collect(),
                received_ms: now_ms(),
                read: true,
                summary: text("content").chars().take(100).collect(),
                ..FakeMail::default()
            };
            let id = s.add(mail);
            ok(json!({ "messageId": id, "subject": text("subject") }))
        }
        _ => return None,
    })
}
