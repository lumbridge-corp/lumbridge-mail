//! Gmail API objects (as gog prints them) to the core model: labels to mailboxes, messages to
//! `MessageMeta`, and the MIME payload tree to a `Body`. Pure functions; no I/O.

use crate::address::{decode_words, parse_ids, parse_list};
use crate::api::{Label, Message, MessageItem, MessagePart};
use std::collections::HashMap;
use base64::Engine;
use lumbridge_mail_core::*;
use std::collections::HashSet;

/// Our synthetic mailbox for Gmail's "All Mail" (Gmail has no label for it). Every message that is not
/// in Trash or Spam is in it. It is never sent to Gmail as a label.
pub const ALL_MAIL: &str = "ALL";

/// Gmail labels that are state, not places, so they never become a `Mailbox`.
const NOT_MAILBOXES: &[&str] = &["UNREAD", "CHAT"];

/// The role of a Gmail label id, or None if it is a system label we do not show.
pub fn label_role(label: &Label) -> Option<MailboxRole> {
    Some(match label.id.as_str() {
        "INBOX" => MailboxRole::Inbox,
        "SENT" => MailboxRole::Sent,
        "DRAFT" => MailboxRole::Drafts,
        "TRASH" => MailboxRole::Trash,
        "SPAM" => MailboxRole::Spam,
        "STARRED" => MailboxRole::Starred,
        "IMPORTANT" => MailboxRole::Important,
        id if NOT_MAILBOXES.contains(&id) => return None,
        id if id.starts_with("CATEGORY_") => MailboxRole::Custom,
        _ if label.kind.eq_ignore_ascii_case("system") => return None,
        _ => MailboxRole::Custom,
    })
}

fn label_display_name(label: &Label, role: MailboxRole) -> String {
    match role {
        MailboxRole::Inbox => "Inbox".into(),
        MailboxRole::Sent => "Sent".into(),
        MailboxRole::Drafts => "Drafts".into(),
        MailboxRole::Trash => "Trash".into(),
        MailboxRole::Spam => "Spam".into(),
        MailboxRole::Starred => "Starred".into(),
        MailboxRole::Important => "Important".into(),
        _ => match label.id.strip_prefix("CATEGORY_") {
            Some(cat) => {
                let mut c = cat.to_ascii_lowercase();
                if let Some(f) = c.get_mut(0..1) {
                    f.make_ascii_uppercase();
                }
                format!("Categories/{c}")
            }
            None if label.name.is_empty() => label.id.clone(),
            None => label.name.clone(),
        },
    }
}

/// The mailbox list for an account: every label that is a place, plus "All Mail". Counts come from
/// `labels get` where the caller fetched them (the label list itself carries none).
pub fn mailboxes(account: &AccountId, labels: &[Label]) -> Vec<Mailbox> {
    let mut out: Vec<Mailbox> = labels
        .iter()
        .filter_map(|l| {
            let role = label_role(l)?;
            Some(Mailbox {
                id: MailboxId::new(&l.id),
                account: account.clone(),
                name: label_display_name(l, role),
                role,
                unread: l.messages_unread.min(u32::MAX as u64) as u32,
                total: l.messages_total.min(u32::MAX as u64) as u32,
            })
        })
        .collect();
    out.push(Mailbox {
        id: MailboxId::new(ALL_MAIL),
        account: account.clone(),
        name: "All Mail".into(),
        role: MailboxRole::All,
        unread: 0,
        total: 0,
    });
    // System roles first in a fixed order, then labels by name.
    let rank = |m: &Mailbox| match m.role {
        MailboxRole::Inbox => 0,
        MailboxRole::Starred => 1,
        MailboxRole::Important => 2,
        MailboxRole::Sent => 3,
        MailboxRole::Drafts => 4,
        MailboxRole::All => 5,
        MailboxRole::Spam => 6,
        MailboxRole::Trash => 7,
        _ => 8,
    };
    out.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    out
}

/// The mailbox ids a message is in: its labels that are mailboxes (when `known` is given, only those),
/// plus All Mail unless it is in Trash or Spam.
pub fn message_mailboxes(label_ids: &[String], known: Option<&HashSet<String>>) -> Vec<MailboxId> {
    let mut out: Vec<MailboxId> = label_ids
        .iter()
        .filter(|l| !NOT_MAILBOXES.contains(&l.as_str()))
        .filter(|l| known.is_none_or(|k| k.contains(l.as_str())))
        .map(MailboxId::new)
        .collect();
    if !label_ids.iter().any(|l| l == "TRASH" || l == "SPAM") {
        out.push(MailboxId::new(ALL_MAIL));
    }
    out
}

pub fn flags(label_ids: &[String]) -> Flags {
    let has = |l: &str| label_ids.iter().any(|x| x == l);
    Flags { seen: !has("UNREAD"), flagged: has("STARRED"), answered: false, draft: has("DRAFT") }
}

/// `MessageMeta` from `gmail raw <id> --format metadata` (or `full`).
pub fn meta(account: &AccountId, msg: &Message, known: Option<&HashSet<String>>) -> MessageMeta {
    let empty = MessagePart::default();
    let p = msg.payload.as_ref().unwrap_or(&empty);
    let h = |n: &str| p.header(n).unwrap_or("");
    let first_id = |n: &str| parse_ids(h(n)).into_iter().next();
    let date = if msg.internal_date > 0 { (msg.internal_date / 1000) as i64 } else { 0 };
    MessageMeta {
        id: MessageId::new(&msg.id),
        account: account.clone(),
        thread: ThreadId::new(if msg.thread_id.is_empty() { &msg.id } else { &msg.thread_id }),
        mailboxes: message_mailboxes(&msg.label_ids, known),
        from: parse_list(h("From")),
        to: parse_list(h("To")),
        cc: parse_list(h("Cc")),
        bcc: parse_list(h("Bcc")),
        subject: one_line(&decode_words(h("Subject"))),
        snippet: snippet(&msg.snippet),
        date,
        flags: flags(&msg.label_ids),
        has_attachments: has_attachments(p),
        size: msg.size_estimate,
        rfc_message_id: first_id("Message-ID"),
        in_reply_to: first_id("In-Reply-To"),
        references: parse_ids(h("References")),
        partial: false,
    }
}

/// `MessageMeta` from one `gmail messages search` row: a *partial* record (`MessageMeta::partial`). The
/// store keeps what it already knows for every field the row lacks: on older gog, To/Cc and the
/// Message-ID; always the size. `by_name` maps gog's label names back to ids (system labels are their
/// own names); `known` limits mailboxes to labels that exist.
pub fn meta_from_item(account: &AccountId, item: &MessageItem, by_name: &HashMap<String, String>, known: Option<&HashSet<String>>) -> MessageMeta {
    let label_ids: Vec<String> = item.labels.iter().map(|n| by_name.get(n).cloned().unwrap_or_else(|| n.clone())).collect();
    let first_id = |n: &str| item.header(n).and_then(|v| parse_ids(v).into_iter().next());
    let content_type = item.header("Content-Type").unwrap_or("").trim().to_ascii_lowercase();
    MessageMeta {
        id: MessageId::new(&item.id),
        account: account.clone(),
        thread: ThreadId::new(if item.thread_id.is_empty() { &item.id } else { &item.thread_id }),
        mailboxes: message_mailboxes(&label_ids, known),
        from: parse_list(&item.from),
        to: parse_list(&item.to),
        cc: parse_list(&item.cc),
        bcc: parse_list(&item.bcc),
        subject: one_line(&decode_words(&item.subject)),
        snippet: snippet_from_body(&item.body),
        date: search_date(item),
        flags: flags(&label_ids),
        has_attachments: !item.attachments.is_empty() || content_type.starts_with("multipart/mixed"),
        size: 0,
        rfc_message_id: first_id("Message-Id"),
        in_reply_to: first_id("In-Reply-To"),
        references: item.header("References").map(parse_ids).unwrap_or_default(),
        partial: true,
    }
}

/// The label ids of a search row (names mapped back), sorted, for "did its labels change?".
pub fn item_label_ids(item: &MessageItem, by_name: &HashMap<String, String>) -> Vec<String> {
    let mut v: Vec<String> = item.labels.iter().map(|n| by_name.get(n).cloned().unwrap_or_else(|| n.clone())).collect();
    v.sort();
    v.dedup();
    v
}

/// Unix seconds of a search row: Gmail's internalDate when gog prints it (`internalDateIso`, 0.34.2+),
/// else the Date header as gog formats it with `--timezone UTC` (`2006-01-02 15:04`). 0 when neither
/// parses (the store then keeps a stored date).
pub fn search_date(item: &MessageItem) -> i64 {
    parse_rfc3339(&item.internal_date_iso).or_else(|| parse_gog_minutes(&item.date)).unwrap_or(0)
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn num(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn civil(date: &str, hh: &str, mm: &str, ss: &str) -> Option<i64> {
    let mut d = date.split('-');
    let (y, mo, da) = (num(d.next()?)?, num(d.next()?)?, num(d.next()?)?);
    let (h, mi, se) = (num(hh)?, num(mm)?, if ss.is_empty() { 0 } else { num(ss)? });
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, da) * 86_400 + h * 3600 + mi * 60 + se)
}

/// `2026-11-09T06:00:00-08:00`, `...Z`, fractions allowed.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once(['T', 't', ' '])?;
    let (time, offset) = rest.split_at(rest.find(['Z', 'z', '+', '-'])?);
    let time = time.split('.').next()?;
    let mut t = time.split(':');
    let base = civil(date, t.next()?, t.next()?, t.next().unwrap_or(""))?;
    let off = match offset.chars().next()? {
        'Z' | 'z' => 0,
        sign => {
            let (oh, om) = offset[1..].split_once(':').unwrap_or((&offset[1..], "0"));
            let v = num(oh)? * 3600 + num(om)? * 60;
            if sign == '-' { -v } else { v }
        }
    };
    Some(base - off)
}

/// gog's display date `2006-01-02 15:04`, read as UTC (we pass `--timezone UTC`).
pub fn parse_gog_minutes(s: &str) -> Option<i64> {
    let (date, time) = s.trim().split_once(' ')?;
    let (h, m) = time.split_once(':')?;
    civil(date, h, m, "")
}

/// A snippet from a body gog printed in a search row (plain text, or HTML when the message has no text
/// part): tags, scripts and styles dropped, entities decoded, one line, 200 characters.
pub fn snippet_from_body(body: &str) -> String {
    if body.trim().is_empty() {
        return String::new();
    }
    // Only the start matters: never walk a whole newsletter.
    let head: String = body.chars().take(16_384).collect();
    let looks_html = head.contains('<') && (head.to_ascii_lowercase().contains("<html") || head.to_ascii_lowercase().contains("<div")
        || head.to_ascii_lowercase().contains("<p") || head.to_ascii_lowercase().contains("<br") || head.to_ascii_lowercase().contains("<table"));
    if !looks_html {
        return snippet(&head.replace('<', "&lt;"));
    }
    let mut out = String::with_capacity(head.len() / 2);
    let lower = head.to_ascii_lowercase();
    let mut i = 0;
    let bytes = head.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Skip <script>...</script> and <style>...</style> whole.
            let skip_to = ["script", "style", "head", "title"].iter().find_map(|t| {
                let after = &lower[i + 1..];
                (after.starts_with(t) && after[t.len()..].starts_with(['>', ' ', '\t', '\n', '\r', '/']))
                    .then(|| lower[i..].find(&format!("</{t}")).map(|e| i + e))
            });
            let from = match skip_to {
                Some(Some(e)) => e,
                Some(None) => break,
                None => i,
            };
            match head[from..].find('>') {
                Some(e) => {
                    i = from + e + 1;
                    out.push(' ');
                }
                None => break,
            }
            continue;
        }
        let ch = head[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    snippet(&out)
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Gmail's snippet is HTML-escaped plain text. Unescape the common entities, flatten, cap at 200 chars.
pub fn snippet(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let semi = after.find(';').filter(|&i| i <= 10);
        let decoded = semi.and_then(|i| entity(&after[1..i]).map(|c| (c, i + 1)));
        match decoded {
            Some((c, used)) => {
                out.push(c);
                rest = &after[used..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    one_line(&out).chars().take(200).collect()
}

fn entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        n if n.starts_with("#x") || n.starts_with("#X") => char::from_u32(u32::from_str_radix(&n[2..], 16).ok()?)?,
        n if n.starts_with('#') => char::from_u32(n[1..].parse().ok()?)?,
        _ => return None,
    })
}

/// With `--format metadata` Gmail sends no parts, so a `multipart/mixed` root is the signal; with
/// `full`, any part that is an attachment.
fn has_attachments(p: &MessagePart) -> bool {
    if p.parts.is_empty() {
        return p.mime_type.eq_ignore_ascii_case("multipart/mixed") || (!p.filename.is_empty());
    }
    let mut stack: Vec<&MessagePart> = p.parts.iter().collect();
    while let Some(part) = stack.pop() {
        if is_attachment(part) && !part.filename.is_empty() {
            return true;
        }
        stack.extend(part.parts.iter());
    }
    false
}

fn disposition(p: &MessagePart) -> String {
    p.header("Content-Disposition").unwrap_or("").trim().to_ascii_lowercase()
}

fn is_attachment(p: &MessagePart) -> bool {
    if p.mime_type.to_ascii_lowercase().starts_with("multipart/") {
        return false;
    }
    let d = disposition(p);
    if !p.filename.is_empty() || d.starts_with("attachment") {
        return true;
    }
    let text = p.mime_type.eq_ignore_ascii_case("text/plain") || p.mime_type.eq_ignore_ascii_case("text/html");
    // An inline image referenced by `cid:` has a Content-ID and often no filename.
    !text && (p.header("Content-ID").is_some() || p.body.as_ref().is_some_and(|b| b.attachment_id.is_some()))
}

/// Our attachment id for a part: Gmail's `partId` (stable across fetches, unlike `attachmentId`).
pub fn attachment_id(p: &MessagePart) -> AttachmentId {
    AttachmentId::new(if p.part_id.is_empty() { "0" } else { &p.part_id })
}

/// Headers kept on a `Body` for the reader and the rules (unsubscribe, sender auth, list mail).
pub const KEPT_HEADERS: &[&str] = &[
    "Reply-To", "Sender", "Return-Path", "List-Id", "List-Unsubscribe", "List-Unsubscribe-Post", "List-Post",
    "List-Archive", "Precedence", "Auto-Submitted", "Authentication-Results", "ARC-Authentication-Results",
    "Received-SPF", "Feedback-ID", "X-Auto-Response-Suppress", "X-GitHub-Reason",
];

/// `Body` from `gmail raw <id> --format full`: walk the payload, decode base64url parts with their charset.
pub fn body(msg: &Message) -> Body {
    let mut out = Body::default();
    let Some(root) = msg.payload.as_ref() else { return out };
    let names = lumbridge_mail_core::BODY_HEADERS.iter().chain(KEPT_HEADERS.iter().filter(|k| {
        !lumbridge_mail_core::BODY_HEADERS.iter().any(|b| b.eq_ignore_ascii_case(k))
    }));
    for name in names {
        for h in root.headers.iter().filter(|h| h.name.eq_ignore_ascii_case(name)) {
            out.headers.push((h.name.clone(), one_line(&h.value)));
        }
    }
    walk(root, &mut out);
    out
}

fn walk(p: &MessagePart, out: &mut Body) {
    if !p.parts.is_empty() {
        for c in &p.parts {
            walk(c, out);
        }
        return;
    }
    if is_attachment(p) {
        out.attachments.push(Attachment {
            id: attachment_id(p),
            filename: if p.filename.is_empty() { "attachment".into() } else { decode_words(&p.filename) },
            mime: if p.mime_type.is_empty() { "application/octet-stream".into() } else { p.mime_type.to_ascii_lowercase() },
            size: p.body.as_ref().map(|b| b.size).unwrap_or(0),
            content_id: p.header("Content-ID").and_then(|c| parse_ids(c).into_iter().next()),
        });
        return;
    }
    let mime = p.mime_type.to_ascii_lowercase();
    let Some(data) = p.body.as_ref().and_then(|b| b.data.as_deref()) else { return };
    let Some(bytes) = b64url(data) else { return };
    let text = decode_charset(&bytes, charset(p).as_deref());
    let slot = match mime.as_str() {
        "text/html" => &mut out.html,
        "text/plain" | "" => &mut out.text,
        _ => return,
    };
    match slot {
        // Several inline text parts (text, attachment, more text): keep them all, in order.
        Some(prev) => {
            prev.push('\n');
            prev.push_str(&text);
        }
        None => *slot = Some(text),
    }
}

/// The `charset` parameter of a part's Content-Type.
fn charset(p: &MessagePart) -> Option<String> {
    let ct = p.header("Content-Type")?;
    ct.split(';').skip(1).find_map(|param| {
        let (k, v) = param.split_once('=')?;
        k.trim().eq_ignore_ascii_case("charset").then(|| v.trim().trim_matches('"').to_owned())
    })
}

/// base64url as Gmail sends it, padded or not; standard base64 and stray whitespace are accepted too.
pub fn b64url(s: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let clean = clean.trim_end_matches('=');
    URL_SAFE_NO_PAD.decode(clean).or_else(|_| STANDARD_NO_PAD.decode(clean)).ok()
}

/// Bytes in a declared charset to a String. Valid UTF-8 wins (Gmail sometimes converts already), except
/// for 7-bit stateful charsets like ISO-2022-JP where the bytes are also valid ASCII.
pub fn decode_charset(bytes: &[u8], charset: Option<&str>) -> String {
    let label = charset.map(|c| c.trim().to_ascii_lowercase()).unwrap_or_default();
    let stateful = label.starts_with("iso-2022") || label == "utf-7";
    if !stateful && let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_owned();
    }
    match encoding_rs::Encoding::for_label(label.as_bytes()) {
        Some(enc) => enc.decode(bytes).0.into_owned(),
        // Unknown or missing charset on 8-bit text: Windows-1252 is what such mail almost always is.
        None => encoding_rs::WINDOWS_1252.decode(bytes).0.into_owned(),
    }
}

/// Find a part by our attachment id (its `partId`).
pub fn find_part<'a>(p: &'a MessagePart, id: &AttachmentId) -> Option<&'a MessagePart> {
    if attachment_id(p) == *id && p.parts.is_empty() {
        return Some(p);
    }
    p.parts.iter().find_map(|c| find_part(c, id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_header_core_asks_for_is_kept() {
        for h in lumbridge_mail_core::BODY_HEADERS {
            assert!(KEPT_HEADERS.iter().any(|k| k.eq_ignore_ascii_case(h)), "{h}");
        }
    }

    #[test]
    fn snippets_unescape_and_cap() {
        assert_eq!(snippet("It&#39;s &quot;fine&quot; &amp; done &lt;3 &#x263A; &bogus; & more"), "It's \"fine\" & done <3 ☺ &bogus; & more");
        assert_eq!(snippet(&"x".repeat(500)).len(), 200);
    }

    #[test]
    fn charsets() {
        assert_eq!(decode_charset(b"caf\xe9", Some("iso-8859-1")), "café");
        assert_eq!(decode_charset("café".as_bytes(), Some("iso-8859-1")), "café");
        assert_eq!(decode_charset(b"caf\xe9", None), "café");
        assert_eq!(decode_charset(b"\x1b$B$3$s$K$A$O\x1b(B", Some("ISO-2022-JP")), "こんにちは");
    }

    #[test]
    fn base64url_padded_or_not() {
        assert_eq!(b64url("aGk_Pz8-").unwrap(), b"hi???>");
        assert_eq!(b64url("aGk=").unwrap(), b"hi");
        assert_eq!(b64url("aGk").unwrap(), b"hi");
        assert_eq!(b64url("aG\nk=").unwrap(), b"hi");
        assert!(b64url("***").is_none());
    }

    #[test]
    fn search_rows_become_partial_metas() {
        let item: MessageItem = serde_json::from_value(serde_json::json!({
            "id": "18f0a1b2c3d4e5aa", "threadId": "18f0a1b2c3d4e5a0",
            "date": "2026-11-09 14:00", "internalDateIso": "2026-11-09T06:00:05-08:00",
            "from": "=?UTF-8?Q?Ad=C3=A1?= <ada@example.com>", "to": "Test User <test.user@example.com>",
            "subject": "Plans  for\tFriday", "labels": ["INBOX", "UNREAD", "Projects/Q4"],
            "headers": {"Message-Id": "<abc@mail.example.com>", "References": "<r1@x.example> <r2@x.example>", "Content-Type": "multipart/alternative"},
            "body": "Hi Sam,\n\nlunch at noon? &amp; bring the notes",
        })).unwrap();
        let by_name: HashMap<String, String> = [("Projects/Q4".to_owned(), "Label_7".to_owned())].into();
        let m = meta_from_item(&AccountId::new("p"), &item, &by_name, None);
        assert!(m.partial);
        assert_eq!(m.date, 1_794_232_805, "internalDate, offset applied");
        assert_eq!(m.from[0].name.as_deref(), Some("Adá"));
        assert_eq!(m.to[0].email, "test.user@example.com");
        assert_eq!(m.subject, "Plans for Friday");
        assert!(!m.flags.seen);
        assert!(m.mailboxes.contains(&MailboxId::new("Label_7")) && m.mailboxes.contains(&MailboxId::new("INBOX")));
        assert_eq!(m.rfc_message_id.as_deref(), Some("abc@mail.example.com"));
        assert_eq!(m.references.len(), 2);
        assert_eq!(m.snippet, "Hi Sam, lunch at noon? & bring the notes");
        assert!(!m.has_attachments);

        // gog 0.28: no ISO date, no recipients, no headers.
        let old: MessageItem = serde_json::from_value(serde_json::json!({
            "id": "a1", "threadId": "a1", "date": "2026-11-09 14:00", "from": "x@example.org", "subject": "s",
            "labels": ["INBOX"], "attachments": [{"filename": "f.pdf", "size": 10, "mimeType": "application/pdf", "attachmentId": "zz"}],
            "body": "<html><head><style>p{color:red}</style></head><body><p>Hello&nbsp;<b>there</b></p><script>x()</script></body></html>",
        })).unwrap();
        let m = meta_from_item(&AccountId::new("p"), &old, &HashMap::new(), None);
        assert_eq!(m.date, 1_794_232_800, "minutes from the Date header, as UTC");
        assert!(m.to.is_empty() && m.rfc_message_id.is_none());
        assert!(m.has_attachments);
        assert_eq!(m.snippet, "Hello there");
        assert_eq!(search_date(&MessageItem { date: "yesterday".into(), ..Default::default() }), 0);
    }

    #[test]
    fn dates_parse() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2000-03-01T00:00:00.123+01:00"), Some(951_865_200));
        assert_eq!(parse_gog_minutes("1970-01-02 00:01"), Some(86_460));
        assert_eq!(parse_rfc3339("garbage"), None);
        assert_eq!(parse_gog_minutes("2026-13-01 00:00"), None);
    }
}
