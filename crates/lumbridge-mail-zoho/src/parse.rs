//! Zoho's answers to the core model. Written without sample answers from a real account (none may be read
//! into a session), from Zoho Mail's REST documentation, so everything is tolerant: every field is
//! optional, numbers may come as strings and strings as numbers, `null` lists are empty lists, unknown
//! fields are ignored. When an answer has no recognisable shape, the error (and the log line) names its
//! keys, never its values.
//!
//! Mail is data: nothing here interprets it, it is only trimmed and parsed.

use lumbridge_mail_core::*;
use serde_json::Value;

/// Up to this many characters of summary make a list row's snippet.
const SNIPPET_CHARS: usize = 200;

/// A string from a string, number or bool (trimmed; empty is None).
pub fn text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// An integer from a number or a numeric string.
pub fn int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f as i64)),
        Value::String(s) => {
            let s = s.trim();
            s.parse::<i64>().ok().or_else(|| {
                s.parse::<f64>()
                    .ok()
                    .filter(|f| f.is_finite())
                    .map(|f| f as i64)
            })
        }
        _ => None,
    }
}

/// A bool from `true`, `"true"`, `1`, `"1"` (and their opposites).
pub fn boolish(v: Option<&Value>) -> Option<bool> {
    match v? {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|i| i != 0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" | "" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// The list in `v`: `v` itself if it is an array, `[]` if it is null, else the first of `keys` that holds
/// an array (or null: empty). None when there is no list at all.
pub fn list_of(v: &Value, keys: &[&str]) -> Option<Vec<Value>> {
    match v {
        Value::Array(a) => Some(a.clone()),
        Value::Null => Some(Vec::new()),
        Value::Object(m) => {
            for k in keys {
                match m.get(*k) {
                    Some(Value::Array(a)) => return Some(a.clone()),
                    Some(Value::Null) => return Some(Vec::new()),
                    Some(inner @ Value::Object(_)) => {
                        if let Some(l) = list_of(inner, keys) {
                            return Some(l);
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        _ => None,
    }
}

/// The shape of an answer: key names (and array lengths), no values.
pub fn shape(v: &Value) -> String {
    fn go(v: &Value, depth: u32) -> String {
        match v {
            Value::Null => "null".into(),
            Value::Bool(_) => "bool".into(),
            Value::Number(_) => "number".into(),
            Value::String(_) => "string".into(),
            Value::Array(a) => match a.first() {
                Some(f) if depth < 2 => format!("[{} x {}]", a.len(), go(f, depth + 1)),
                _ => format!("[{}]", a.len()),
            },
            Value::Object(m) => {
                let keys: Vec<String> = m
                    .iter()
                    .take(24)
                    .map(|(k, v)| {
                        let k: String = k
                            .chars()
                            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                            .take(40)
                            .collect();
                        if depth < 2 {
                            format!("{k}: {}", go(v, depth + 1))
                        } else {
                            k
                        }
                    })
                    .collect();
                format!("{{{}}}", keys.join(", "))
            }
        }
    }
    go(v, 0)
}

/// An unexpected answer: logged and returned by shape only.
pub fn shape_error(what: &str, v: &Value) -> Error {
    let s = shape(v);
    eprintln!("lumbridge-mail-zoho: unexpected {what} answer; shape: {s}");
    Error::Other(format!("Zoho: unexpected {what} answer (shape: {s})"))
}

/// `&lt;a@b.example&gt;` and friends (Zoho HTML-escapes addresses in listings).
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find(';').filter(|e| *e <= 10);
        let decoded = end.and_then(|e| {
            let ent = &tail[1..e];
            let c = match ent {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "amp" => Some('&'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some(' '),
                _ => {
                    let n = ent
                        .strip_prefix("#x")
                        .or_else(|| ent.strip_prefix("#X"))
                        .map(|h| u32::from_str_radix(h, 16));
                    let n = n.or_else(|| ent.strip_prefix('#').map(|d| d.parse::<u32>()));
                    n.and_then(|r| r.ok()).and_then(char::from_u32)
                }
            };
            c.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &tail[e + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// An address list as Zoho shows it (`"Name" <a@b>, c@d`, maybe HTML-escaped; `Not Provided` = none).
pub fn addresses(raw: &str) -> Vec<Address> {
    let s = unescape(raw);
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("not provided") {
        return Vec::new();
    }
    let header = format!("To: {}\r\n\r\n", s.replace(['\r', '\n'], " "));
    if let Some(m) = mail_parser::MessageParser::default().parse_headers(header.as_bytes())
        && let Some(list) = m.to()
    {
        let out: Vec<Address> = list
            .iter()
            .filter_map(|a| {
                a.address()
                    .map(str::trim)
                    .filter(|e| e.contains('@'))
                    .map(|e| Address::new(a.name(), e))
            })
            .collect();
        if !out.is_empty() {
            return out;
        }
    }
    s.split([',', ';'])
        .filter_map(|part| {
            let part = part.trim();
            let email = match (part.rfind('<'), part.rfind('>')) {
                (Some(a), Some(b)) if a < b => &part[a + 1..b],
                _ => part,
            };
            email
                .contains('@')
                .then(|| Address::new(None, email.trim()))
        })
        .collect()
}

pub fn snippet(s: &str) -> String {
    let collapsed: String = unescape(s).split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(SNIPPET_CHARS).collect();
    if collapsed.chars().count() > SNIPPET_CHARS {
        out.push('…');
    }
    out
}

/// One row of `list_mails` (or the answer of `get_mail`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub thread: Option<String>,
    pub folder: Option<String>,
    pub subject: String,
    pub from: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    /// Unix seconds (0 when Zoho gave none).
    pub date: i64,
    pub seen: Option<bool>,
    pub flagged: Option<bool>,
    pub has_attachments: bool,
    pub size: u64,
    pub summary: String,
}

/// Zoho message and folder ids are numbers; anything else is not used as an id.
pub fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 40 && s.bytes().all(|b| b.is_ascii_digit())
}

fn millis_to_secs(ms: i64) -> i64 {
    // Seconds already (before 2001-09-09 in ms would be < 1e12): keep as is.
    if ms.abs() >= 100_000_000_000 {
        ms / 1000
    } else {
        ms
    }
}

pub fn row(v: &Value) -> Option<Row> {
    let id = text(v.get("messageId"))?;
    if !valid_id(&id) {
        return None;
    }
    let mut from = addresses(&text(v.get("fromAddress")).unwrap_or_default());
    if let (Some(f), Some(name)) = (from.first_mut(), text(v.get("sender")))
        && f.name.is_none()
        && !name.eq_ignore_ascii_case(&f.email)
    {
        f.name = Some(unescape(&name));
    }
    let date = int(v.get("receivedTime"))
        .or_else(|| int(v.get("sentDateInGMT")))
        .map(millis_to_secs)
        .unwrap_or(0);
    let seen = match text(v.get("status")).as_deref() {
        Some("1") => Some(true),
        Some("0") => Some(false),
        Some(s) => boolish(Some(&Value::String(s.to_owned()))),
        None => None,
    };
    let flagged = text(v.get("flagid"))
        .or_else(|| text(v.get("flagId")))
        .map(|f| {
            let f = f.to_ascii_lowercase();
            !(f.is_empty() || f == "flag_not_set" || f == "0" || f == "none")
        });
    let has_attachments = boolish(v.get("hasAttachment")).unwrap_or(false);
    Some(Row {
        id,
        thread: text(v.get("threadId")).filter(|t| valid_id(t)),
        folder: text(v.get("folderId")).filter(|t| valid_id(t)),
        subject: unescape(&text(v.get("subject")).unwrap_or_default()),
        from,
        to: addresses(&text(v.get("toAddress")).unwrap_or_default()),
        cc: addresses(&text(v.get("ccAddress")).unwrap_or_default()),
        date,
        seen,
        flagged,
        has_attachments,
        size: int(v.get("size")).unwrap_or(0).max(0) as u64,
        summary: snippet(&text(v.get("summary")).unwrap_or_default()),
    })
}

/// The rows of a `list_mails` answer. `Err` (by shape) when it is not a list at all.
pub fn rows(v: &Value) -> Result<Vec<Row>> {
    page(v).map(|p| p.rows)
}

/// One `list_mails` answer: `{messages: [...], nextPageToken}` (or a bare list).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Page {
    pub rows: Vec<Row>,
    /// Zoho's cursor for the next page, when it gave one.
    pub next: Option<String>,
    /// Rows in the answer, including any without a usable id.
    pub listed: usize,
}

pub fn page(v: &Value) -> Result<Page> {
    let list = list_of(v, &["messages", "data", "mails", "list"])
        .ok_or_else(|| shape_error("list_mails", v))?;
    let next = text(v.get("nextPageToken"))
        .or_else(|| text(v.get("pageToken")))
        .filter(|t| t.len() <= 4096);
    Ok(Page {
        listed: list.len(),
        rows: list.iter().filter_map(row).collect(),
        next,
    })
}

/// The entries of a `list_accounts` answer: `{primaryAccount: {...}}` (one sign-in, one mailbox), a
/// list, or `{accounts: [...]}`. None when there is nothing account-shaped.
pub fn accounts(v: &Value) -> Option<Vec<Value>> {
    match v {
        Value::Array(a) => Some(a.clone()),
        Value::Object(m) => {
            if m.contains_key("accountId") {
                return Some(vec![v.clone()]);
            }
            let mut out = Vec::new();
            let mut found = false;
            for (k, x) in m {
                let kl = k.to_ascii_lowercase();
                if !kl.contains("account") && kl != "data" {
                    continue;
                }
                match x {
                    Value::Object(o) if o.contains_key("accountId") => {
                        found = true;
                        out.push(x.clone());
                    }
                    Value::Array(a) => {
                        found = true;
                        out.extend(a.iter().filter(|e| e.get("accountId").is_some()).cloned());
                    }
                    Value::Null => found = true,
                    _ => {}
                }
            }
            found.then_some(out)
        }
        _ => None,
    }
}

/// The shape of an answer for the probe: key names, JSON types (strings as `digits` or `str`), list
/// lengths. Keys that could be values (long, or with `@`, spaces or other odd characters) are masked.
pub fn probe_shape(v: &Value) -> String {
    fn key(k: &str) -> String {
        if k.len() > 40
            || !k
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            "<key>".into()
        } else if k.chars().all(|c| c.is_ascii_digit()) {
            "<digits>".into()
        } else {
            k.to_owned()
        }
    }
    fn go(v: &Value, depth: u32) -> String {
        match v {
            Value::Null => "null".into(),
            Value::Bool(_) => "bool".into(),
            Value::Number(n) => {
                if n.is_f64() {
                    "float".into()
                } else {
                    "int".into()
                }
            }
            Value::String(s) => {
                if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
                    "digits".into()
                } else {
                    "str".into()
                }
            }
            Value::Array(a) => match a.first() {
                Some(f) if depth < 6 => format!("[{} x {}]", a.len(), go(f, depth + 1)),
                _ => format!("[{}]", a.len()),
            },
            Value::Object(m) if depth >= 6 => format!("{{{} keys}}", m.len()),
            Value::Object(m) => {
                let parts: Vec<String> = m
                    .iter()
                    .map(|(k, x)| format!("{}: {}", key(k), go(x, depth + 1)))
                    .collect();
                format!("{{{}}}", parts.join(", "))
            }
        }
    }
    go(v, 0)
}

impl Row {
    /// A list record (`partial`: the store keeps what it knows for empty fields).
    pub fn meta(&self, account: &AccountId, mailbox: &str, draft: bool) -> MessageMeta {
        MessageMeta {
            id: MessageId::new(&self.id),
            account: account.clone(),
            thread: ThreadId::new(self.thread.clone().unwrap_or_else(|| self.id.clone())),
            mailboxes: vec![MailboxId::new(mailbox)],
            from: self.from.clone(),
            to: self.to.clone(),
            cc: self.cc.clone(),
            bcc: Vec::new(),
            subject: self.subject.clone(),
            snippet: self.summary.clone(),
            date: self.date,
            flags: Flags {
                seen: self.seen.unwrap_or(true),
                flagged: self.flagged.unwrap_or(false),
                answered: false,
                draft,
            },
            has_attachments: self.has_attachments,
            size: self.size,
            rfc_message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            partial: true,
        }
    }

    /// Seen and flagged as two bits, for spotting changes cheaply.
    pub fn bits(&self) -> u8 {
        u8::from(self.seen.unwrap_or(true)) | (u8::from(self.flagged.unwrap_or(false)) << 1)
    }
}

/// A folder from `list_folders`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub id: String,
    pub name: String,
    /// Zoho's `folderType` (`Inbox`, `Sent`, `Drafts`, `Templates`, `Snoozed`, `Spam`, `Trash`, `Outbox`).
    pub kind: String,
    pub path: String,
}

pub fn folders(v: &Value) -> Result<Vec<Folder>> {
    let list =
        list_of(v, &["folders", "data", "list"]).ok_or_else(|| shape_error("list_folders", v))?;
    Ok(list
        .iter()
        .filter_map(|f| {
            let id = text(f.get("folderId"))?;
            if !valid_id(&id) {
                return None;
            }
            let name = unescape(&text(f.get("folderName")).unwrap_or_default());
            let path = unescape(&text(f.get("path")).unwrap_or_default());
            // Without a type, a top-level folder's name says what it is (Inbox, Sent, ...).
            let kind = text(f.get("folderType")).unwrap_or_else(|| {
                if path.trim_matches('/').contains('/') {
                    String::new()
                } else {
                    name.clone()
                }
            });
            Some(Folder {
                id,
                kind,
                name,
                path,
            })
        })
        .collect())
}

impl Folder {
    /// Zoho marks a user folder's type as `Inbox` too; only the top-level one is the inbox.
    pub fn role(&self) -> MailboxRole {
        let top = |n: &str| {
            let p = self.path.trim_matches('/');
            (p.is_empty() || p.eq_ignore_ascii_case(n)) && self.name.eq_ignore_ascii_case(n)
        };
        match self.kind.to_ascii_lowercase().as_str() {
            "inbox" if top("Inbox") => MailboxRole::Inbox,
            "sent" => MailboxRole::Sent,
            "drafts" => MailboxRole::Drafts,
            "spam" => MailboxRole::Spam,
            "trash" => MailboxRole::Trash,
            _ => MailboxRole::Custom,
        }
    }

    /// Outbox and Templates are Zoho's working folders, not mail.
    pub fn synced(&self) -> bool {
        !matches!(
            self.kind.to_ascii_lowercase().as_str(),
            "outbox" | "templates"
        )
    }

    pub fn display(&self) -> String {
        let p = self.path.trim_matches('/');
        if p.is_empty() {
            self.name.clone()
        } else {
            p.to_owned()
        }
    }
}

/// The header block as `(name, value)` pairs, values unfolded onto one line. From a raw block (a string)
/// or Zoho's parsed form (`{name: [values]}` or `{name: value}`).
pub fn header_pairs(v: &Value) -> Vec<(String, String)> {
    let content = match v {
        Value::Object(m) => m
            .get("headerContent")
            .or_else(|| m.get("headers"))
            .unwrap_or(v),
        _ => v,
    };
    match content {
        Value::String(raw) => unfold(raw),
        Value::Object(m) if !m.contains_key("headerContent") => m
            .iter()
            .flat_map(|(k, val)| {
                let vals: Vec<String> = match val {
                    Value::Array(a) => a.iter().filter_map(|x| text(Some(x))).collect(),
                    other => text(Some(other)).into_iter().collect(),
                };
                vals.into_iter()
                    .map(move |x| (k.clone(), x.replace(['\r', '\n'], " ")))
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn unfold(raw: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in raw.replace("\r\n", "\n").split('\n') {
        if line.is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            if let Some(last) = out.last_mut() {
                last.1.push(' ');
                last.1.push_str(line.trim());
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            if !k.is_empty() && k.bytes().all(|b| b.is_ascii_graphic()) {
                out.push((k.to_owned(), v.trim().to_owned()));
            }
        }
    }
    out
}

/// The headers worth keeping on a `Body`: every [`BODY_HEADERS`] occurrence.
pub fn body_headers(pairs: &[(String, String)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .filter(|(k, _)| {
            BODY_HEADERS
                .iter()
                .chain(THREAD_HEADERS)
                .any(|h| h.eq_ignore_ascii_case(k))
        })
        .cloned()
        .collect()
}

/// Headers a body carries for threading: Zoho's listings have no thread id and no Message-IDs, so the
/// store learns them from the first body fetched (`Store::learn_thread_headers`).
pub const THREAD_HEADERS: &[&str] = &["Message-ID", "In-Reply-To", "References"];

/// An attachment from `list_mail_attachments`.
pub fn attachments(v: &Value) -> Vec<Attachment> {
    let mut all = Vec::new();
    let groups: Vec<Vec<Value>> = match v {
        Value::Object(m) if m.contains_key("attachments") || m.contains_key("inline") => {
            ["attachments", "inline"]
                .iter()
                .map(|k| m.get(*k).and_then(|x| list_of(x, &[])).unwrap_or_default())
                .collect()
        }
        other => vec![list_of(other, &["data", "attachments"]).unwrap_or_default()],
    };
    for a in groups.into_iter().flatten() {
        let Some(id) = text(a.get("attachmentId")) else {
            continue;
        };
        if id.len() > 200 || id.chars().any(|c| c.is_control() || c == '/') {
            continue;
        }
        let filename = sanitise_filename(&unescape(
            &text(a.get("attachmentName")).unwrap_or_default(),
        ));
        let content_id = text(a.get("cid"))
            .or_else(|| text(a.get("contentId")))
            .map(|c| c.trim_start_matches('<').trim_end_matches('>').to_owned());
        all.push(Attachment {
            id: AttachmentId::new(id),
            mime: mime_for(&filename).into(),
            filename,
            size: int(a.get("attachmentSize"))
                .or_else(|| int(a.get("size")))
                .unwrap_or(0)
                .max(0) as u64,
            content_id,
        });
    }
    all
}

pub fn sanitise_filename(s: &str) -> String {
    let base = s.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = base.chars().filter(|c| !c.is_control()).take(200).collect();
    let clean = clean.trim().trim_start_matches('.').to_owned();
    if clean.is_empty() {
        "attachment".into()
    } else {
        clean
    }
}

pub fn mime_for(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "ics" => "text/calendar",
        "zip" => "application/zip",
        "json" => "application/json",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "eml" => "message/rfc822",
        _ => "application/octet-stream",
    }
}

/// The addresses a `list_accounts` entry answers to (lower case).
pub fn account_addresses(a: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for k in [
        "primaryEmailAddress",
        "mailboxAddress",
        "incomingUserName",
        "incomingUsername",
        "emailAddress",
        "emailAlias",
        "sendMailAddress",
        "mailId",
    ] {
        match a.get(k) {
            Some(Value::String(s)) => out.push(s.clone()),
            Some(Value::Array(list)) => {
                for e in list {
                    match e {
                        Value::String(s) => out.push(s.clone()),
                        other => out.extend(
                            text(other.get("mailId")).or_else(|| text(other.get("emailAddress"))),
                        ),
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(list) = a.get("sendMailDetails").and_then(|s| list_of(s, &[])) {
        out.extend(list.iter().filter_map(|d| text(d.get("fromAddress"))));
    }
    out.into_iter()
        .map(|s| unescape(s.trim()).to_ascii_lowercase())
        .filter(|s| s.contains('@'))
        .collect()
}

/// The text of a tool answer that is a document (`get_mail_html`'s `content`, `get_mail_original`'s
/// raw message): the string itself, or the first of `keys` that holds one.
pub fn document(v: &Value, keys: &[&str]) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(m) => keys.iter().find_map(|k| match m.get(*k) {
            Some(Value::String(s)) => Some(s.clone()),
            Some(inner @ Value::Object(_)) => document(inner, keys),
            _ => None,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rows_tolerate_strings_numbers_and_nulls() {
        let v = json!([
            {"messageId": "1700000000000100001", "threadId": 1700000000000100000u64, "folderId": "9000000002014",
             "subject": "Lunch &amp; plans", "fromAddress": "pat@example.com", "sender": "Pat Example",
             "toAddress": "&quot;Sam&quot; &lt;sam@one.example&gt;, kim@example.org", "ccAddress": "Not Provided",
             "receivedTime": "1794211200000", "status": "0", "flagid": "flag_not_set", "hasAttachment": "1",
             "summary": "  Hello   there ", "size": 2050, "somethingNew": {"x": 1}},
            {"messageId": 1700000000000100002u64, "receivedTime": 1794211300000u64, "status": 1, "flagid": "important",
             "hasAttachment": false, "toAddress": null, "summary": null},
            {"subject": "no id"},
            {"messageId": "../etc"}
        ]);
        let r = rows(&v).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].subject, "Lunch & plans");
        assert_eq!(
            r[0].from,
            vec![Address::new(Some("Pat Example"), "pat@example.com")]
        );
        assert_eq!(
            r[0].to,
            vec![
                Address::new(Some("Sam"), "sam@one.example"),
                Address::new(None, "kim@example.org")
            ]
        );
        assert!(r[0].cc.is_empty());
        assert_eq!(
            (r[0].date, r[0].seen, r[0].flagged, r[0].has_attachments),
            (1_794_211_200, Some(false), Some(false), true)
        );
        assert_eq!(r[0].thread.as_deref(), Some("1700000000000100000"));
        assert_eq!(r[0].summary, "Hello there");
        assert_eq!(
            (r[1].seen, r[1].flagged, r[1].date),
            (Some(true), Some(true), 1_794_211_300)
        );
        assert!(r[1].to.is_empty() && r[1].summary.is_empty());
        assert_eq!(r[0].bits(), 0);
        assert_eq!(r[1].bits(), 3);
        // Null and wrapped lists.
        assert!(rows(&Value::Null).unwrap().is_empty());
        assert!(rows(&json!({"data": null})).unwrap().is_empty());
        assert_eq!(
            rows(&json!({"messages": [{"messageId": "5"}]}))
                .unwrap()
                .len(),
            1
        );
        let e = rows(&json!({"weird": "secret subject"}))
            .unwrap_err()
            .to_string();
        assert!(e.contains("weird") && !e.contains("secret"), "{e}");
    }

    #[test]
    fn folders_and_roles() {
        let v = json!({"data": [
            {"folderId": "1", "folderName": "Inbox", "folderType": "Inbox", "path": "/Inbox"},
            {"folderId": "2", "folderName": "test", "folderType": "Inbox", "path": "/test"},
            {"folderId": 3, "folderName": "Sent", "folderType": "Sent", "path": "/Sent"},
            {"folderId": "4", "folderName": "Outbox", "folderType": "Outbox"},
            {"folderId": "5", "folderName": "Child", "folderType": "Inbox", "path": "/Projects/Child"},
            {"folderName": "no id"}
        ]});
        let f = folders(&v).unwrap();
        assert_eq!(f.len(), 5);
        assert_eq!(f[0].role(), MailboxRole::Inbox);
        assert_eq!(f[1].role(), MailboxRole::Custom);
        assert_eq!(f[2].role(), MailboxRole::Sent);
        assert!(!f[3].synced() && f[2].synced());
        assert_eq!(f[4].display(), "Projects/Child");
        assert!(folders(&json!({"data": null})).unwrap().is_empty());
    }

    #[test]
    fn headers_both_forms() {
        let raw = json!({"headerContent": "Subject: x\r\nList-Id: <news.example.com>\r\nList-Unsubscribe: <https://u.example.com/x>,\r\n <mailto:u@example.com>\r\nReceived: a\r\n\r\nbody"});
        let h = body_headers(&header_pairs(&raw));
        assert_eq!(
            h,
            vec![
                ("List-Id".to_string(), "<news.example.com>".to_string()),
                (
                    "List-Unsubscribe".to_string(),
                    "<https://u.example.com/x>, <mailto:u@example.com>".to_string()
                ),
            ]
        );
        let parsed = json!({"headerContent": {"Precedence": ["bulk"], "Reply-To": "r@example.com", "Received": ["a", "b"]}, "messageId": 1});
        let h = body_headers(&header_pairs(&parsed));
        assert!(
            h.contains(&("Precedence".into(), "bulk".into()))
                && h.contains(&("Reply-To".into(), "r@example.com".into()))
        );
        assert!(header_pairs(&Value::Null).is_empty());
        // The threading headers come along (the listing has none of them).
        let t = json!({"headerContent": "Message-ID: <b@mail.example.com>\r\nIn-Reply-To: <a@mail.example.com>\r\nReferences: <a@mail.example.com>\r\nX-Other: y\r\n\r\n"});
        let h = body_headers(&header_pairs(&t));
        assert_eq!(
            h.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            ["Message-ID", "In-Reply-To", "References"]
        );
    }

    #[test]
    fn attachments_and_accounts() {
        let v = json!({"attachments": [{"attachmentId": "11", "attachmentName": "../report.pdf", "attachmentSize": "6737"}],
                       "inline": [{"attachmentId": 12, "attachmentName": "logo.png", "attachmentSize": 10, "cid": "<abc>"}]});
        let a = attachments(&v);
        assert_eq!(a.len(), 2);
        assert_eq!(
            (a[0].filename.as_str(), a[0].mime.as_str(), a[0].size),
            ("report.pdf", "application/pdf", 6737)
        );
        assert_eq!(a[1].content_id.as_deref(), Some("abc"));
        assert!(attachments(&json!({"attachments": null, "inline": null})).is_empty());
        assert!(attachments(&Value::Null).is_empty());
        let acc = json!({"accountId": "1", "primaryEmailAddress": "Sam@One.example",
                         "emailAddress": [{"mailId": "alias@one.example", "isAlias": true}], "sendMailDetails": null});
        assert_eq!(
            account_addresses(&acc),
            vec!["sam@one.example", "alias@one.example"]
        );
        let acc2 = json!({"accountId": 2, "emailAddress": "b@two.example"});
        assert_eq!(account_addresses(&acc2), vec!["b@two.example"]);
    }

    #[test]
    fn unescape_and_addresses() {
        assert_eq!(
            unescape("a &lt;b&gt; &#64; &#x41; &bogus; &"),
            "a <b> @ A &bogus; &"
        );
        assert_eq!(addresses("Not Provided"), vec![]);
        assert_eq!(
            addresses("x@y.example"),
            vec![Address::new(None, "x@y.example")]
        );
    }

    #[test]
    fn the_real_shapes() {
        // `list_mails`' inner data, every value a string, `flagId` with a capital I, no threadId.
        let v = json!({"nextPageToken": "opaque-1", "messages": [{
            "summary": "s", "sentDateInGMT": "1794211200000", "calendarType": "0", "subject": "Hi",
            "messageId": "1794000000000000001", "flagId": "important", "status2": "0", "priority": "3",
            "hasInline": "false", "toAddress": "&lt;sam@one.example.test&gt;", "folderId": "100101",
            "ccAddress": "Not Provided", "hasAttachment": "0", "size": "2050", "sender": "Pat",
            "receivedTime": "1794211200000", "fromAddress": "pat@example.com", "status": "1"}]});
        let p = page(&v).unwrap();
        assert_eq!(
            (p.next.as_deref(), p.listed, p.rows.len()),
            (Some("opaque-1"), 1, 1)
        );
        let r = &p.rows[0];
        assert_eq!(
            (r.seen, r.flagged, r.thread.as_deref(), r.folder.as_deref()),
            (Some(true), Some(true), None, Some("100101"))
        );
        assert_eq!(
            r.meta(&AccountId::new("a"), "100101", false)
                .thread
                .as_str(),
            "1794000000000000001"
        );
        assert!(page(&json!({"messages": []})).unwrap().rows.is_empty());
        // `list_accounts`: one primary account, the address a string, aliases a list.
        let a = json!({"primaryAccount": {"accountId": "1001", "emailAddress": "Sam@One.example.test",
                       "emailAlias": ["sam@two.example.test"], "zuid": 5, "sendMailAddress": "sam@one.example.test"}});
        let list = accounts(&a).unwrap();
        assert_eq!(list.len(), 1);
        let addrs = account_addresses(&list[0]);
        assert!(
            addrs.contains(&"sam@one.example.test".to_string())
                && addrs.contains(&"sam@two.example.test".to_string())
        );
        assert_eq!(accounts(&json!([{"accountId": "1"}])).unwrap().len(), 1);
        assert!(accounts(&json!({"weird": 1})).is_none());
        // Folders under `folders`.
        let f = folders(&json!({"folders": [{"folderId": "1", "folderName": "Inbox", "folderType": "Inbox", "path": "/Inbox", "isArchived": false}]})).unwrap();
        assert_eq!(f[0].role(), MailboxRole::Inbox);
        // The raw message under `originalContent`.
        assert_eq!(
            document(
                &json!({"originalContent": "From: a"}),
                &["content", "originalContent"]
            )
            .as_deref(),
            Some("From: a")
        );
    }

    #[test]
    fn probe_shapes_show_no_values() {
        let v = json!({"subject": "Secret plans", "messageId": "123", "n": 4, "ok": true, "f": 1.5, "x": null,
                       "list": [{"a": "b"}], "sam@one.example.test": "v", "12345": "y", "with space": 1});
        let s = probe_shape(&v);
        assert!(
            !s.contains("Secret")
                && !s.contains("123\"")
                && !s.contains("sam@")
                && !s.contains("12345")
                && !s.contains("with space"),
            "{s}"
        );
        assert!(
            s.contains("subject: str")
                && s.contains("messageId: digits")
                && s.contains("n: int")
                && s.contains("list: [1 x {a: str}]"),
            "{s}"
        );
        assert!(
            s.contains("<key>: str") && s.contains("<digits>: str"),
            "{s}"
        );
    }
}
