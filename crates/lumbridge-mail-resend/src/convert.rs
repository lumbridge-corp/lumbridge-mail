//! Resend objects to the core model: ids, mailboxes, addresses, dates, threads, bodies and headers.

use crate::api::{AttachmentInfo, ReceivedEmail, SentEmail};
use lumbridge_mail_core::*;

/// Mailbox ids. Inbox, Sent and Scheduled come from the API; Bounced is a view of Sent; Archive and
/// Trash hold what was archived or deleted here (local only), and Trash also holds canceled emails.
pub const INBOX: &str = "inbox";
pub const SENT: &str = "sent";
pub const SCHEDULED: &str = "scheduled";
pub const BOUNCED: &str = "bounced";
pub const ARCHIVE: &str = "archive";
pub const TRASH: &str = "trash";

pub fn mailboxes(account: &AccountId, receiving: bool) -> Vec<Mailbox> {
    let one = |id: &str, name: &str, role: MailboxRole| Mailbox {
        id: MailboxId::new(id),
        account: account.clone(),
        name: name.into(),
        role,
        unread: 0,
        total: 0,
    };
    let mut out = Vec::new();
    if receiving {
        out.push(one(INBOX, "Inbox", MailboxRole::Inbox));
    }
    out.push(one(SENT, "Sent", MailboxRole::Sent));
    out.push(one(SCHEDULED, "Scheduled", MailboxRole::Custom));
    out.push(one(BOUNCED, "Bounced", MailboxRole::Custom));
    out.push(one(ARCHIVE, "Archive", MailboxRole::Archive));
    out.push(one(TRASH, "Trash", MailboxRole::Trash));
    out
}

/// Which list a message came from. Message ids are `s:<resend id>` and `r:<resend id>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Sent,
    Received,
}

pub fn message_id(kind: Kind, resend_id: &str) -> MessageId {
    MessageId::new(format!("{}:{resend_id}", if kind == Kind::Sent { "s" } else { "r" }))
}

/// Resend ids are UUIDs; anything else never reaches a URL.
pub fn valid_resend_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The kind and Resend id of one of our message ids; `NotFound` for ids this provider did not make.
pub fn split_id(id: &MessageId) -> Result<(Kind, &str)> {
    let (k, rest) = match id.as_str().split_once(':') {
        Some(("s", r)) => (Kind::Sent, r),
        Some(("r", r)) => (Kind::Received, r),
        _ => return Err(Error::NotFound(format!("not a Resend message id: {id}"))),
    };
    if !valid_resend_id(rest) {
        return Err(Error::NotFound(format!("not a Resend message id: {id}")));
    }
    Ok((k, rest))
}

// ---- dates ---------------------------------------------------------------------------------------

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Milliseconds since the epoch from Resend's two formats: `2023-04-03 22:13:42.674981+00` (sent
/// emails, domains) and `2023-10-06T23:47:56.678Z` (received emails). Offsets `+hh`, `+hh:mm`, `+hhmm`.
pub fn parse_time_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut i = 19;
    let mut ms = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let frac = &s[start..i];
        let mut digits: String = frac.chars().take(3).collect();
        while digits.len() < 3 {
            digits.push('0');
        }
        ms = digits.parse().ok()?;
    }
    let rest = s[i..].trim();
    let offset_min = match rest {
        "" | "Z" | "z" => 0,
        r if r.starts_with('+') || r.starts_with('-') => {
            let sign = if r.starts_with('-') { -1 } else { 1 };
            let digits: String = r[1..].chars().filter(|c| c.is_ascii_digit()).collect();
            let (hh, mm) = match digits.len() {
                2 => (digits.parse::<i64>().ok()?, 0),
                4 => (digits[..2].parse::<i64>().ok()?, digits[2..].parse::<i64>().ok()?),
                _ => return None,
            };
            sign * (hh * 60 + mm)
        }
        _ => return None,
    };
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - offset_min * 60;
    Some(secs * 1000 + ms)
}

// ---- addresses -----------------------------------------------------------------------------------

/// `Name <a@b.example>`, `"Last, First" <a@b.example>`, `<a@b.example>` or a bare address.
pub fn parse_address(s: &str) -> Option<Address> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let (Some(open), Some(close)) = (s.rfind('<'), s.rfind('>'))
        && open < close
    {
        let email = s[open + 1..close].trim();
        if !email.contains('@') {
            return None;
        }
        let name = s[..open].trim().trim_matches('"').replace("\\\"", "\"");
        return Some(Address::new(Some(name.trim()), email));
    }
    s.contains('@').then(|| Address::new(None, s))
}

pub fn addresses(list: &[String]) -> Vec<Address> {
    list.iter().filter_map(|s| parse_address(s)).collect()
}

/// An address for Resend's `from`/`to`: `"Name" <a@b>` with the name quoted, or the bare address.
/// Control characters (header injection) and quotes in names are dropped; addresses with spaces,
/// angle brackets or no `@` are `Invalid`.
pub fn format_address(a: &Address) -> Result<String> {
    let email = a.email.trim();
    let bad = email.is_empty()
        || !email.contains('@')
        || email.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"' | ',' | ';'));
    if bad {
        return Err(Error::Invalid(format!("not an address Resend can send to: {email:?}")));
    }
    match a.name.as_deref().map(|n| n.chars().filter(|c| !c.is_control() && !matches!(c, '"' | '\\')).collect::<String>()) {
        Some(n) if !n.trim().is_empty() => Ok(format!("\"{}\" <{email}>", n.trim())),
        _ => Ok(email.to_owned()),
    }
}

// ---- threads -------------------------------------------------------------------------------------

/// Subject without reply/forward prefixes (`Re:`, `Fwd:`, `AW:`, `SV:`, `[tag]` kept), lower case.
pub fn normalise_subject(s: &str) -> String {
    let mut s = s.trim();
    loop {
        let lower = s.to_ascii_lowercase();
        let cut = ["re:", "fwd:", "fw:", "aw:", "sv:", "vs:", "wg:", "tr:", "antw:", "rif:", "re :", "fwd :"]
            .iter()
            .find(|p| lower.starts_with(*p))
            .map(|p| p.len());
        match cut {
            Some(n) => s = s[n..].trim_start(),
            None => break,
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn fnv1a(parts: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.bytes().chain(std::iter::once(0u8)) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// Resend has no threads. A thread is one subject (without `Re:`/`Fwd:`) with one other party: the
/// first recipient of mail we sent, the sender of mail we received. So a reply and its answer meet,
/// and the same transactional mail to a thousand people is a thousand threads, not one.
pub fn thread_id(subject: &str, counterpart: &str) -> ThreadId {
    let h = fnv1a(&[&normalise_subject(subject), &counterpart.trim().to_ascii_lowercase()]);
    ThreadId::new(format!("t:{h:016x}"))
}

fn strip_angles(s: &str) -> String {
    s.trim().trim_start_matches('<').trim_end_matches('>').trim().to_owned()
}

/// Message-IDs in a References header (`<a@x> <b@y>`).
pub fn message_ids(s: &str) -> Vec<String> {
    s.split(|c: char| c.is_whitespace() || c == ',').map(strip_angles).filter(|x| !x.is_empty()).collect()
}

// ---- text ----------------------------------------------------------------------------------------

/// Up to ~200 characters of plain text for a list row, from text or (roughly) from HTML.
pub fn snippet(text: Option<&str>, html: Option<&str>) -> String {
    let source = match (text.filter(|t| !t.trim().is_empty()), html) {
        (Some(t), _) => t.to_owned(),
        (None, Some(h)) => strip_tags(h),
        (None, None) => String::new(),
    };
    let words = source.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = words.chars().take(200).collect();
    if words.chars().count() > 200 {
        out.push('…');
    }
    out
}

/// Tags, `<style>`/`<script>` contents and comments out; a few entities decoded. Only for snippets: the
/// reader always goes through lumbridge-mail-render.
fn strip_tags(h: &str) -> String {
    let lower = h.to_ascii_lowercase();
    let mut out = String::with_capacity(h.len() / 2);
    let mut i = 0;
    let b = h.as_bytes();
    while i < b.len() {
        if b[i] == b'<' {
            let rest = &lower[i..];
            let skip_to = if rest.starts_with("<!--") {
                rest.find("-->").map(|e| e + 3)
            } else if rest.starts_with("<style") || rest.starts_with("<script") || rest.starts_with("<head") {
                let tag = if rest.starts_with("<style") { "</style>" } else if rest.starts_with("<script") { "</script>" } else { "</head>" };
                rest.find(tag).map(|e| e + tag.len())
            } else {
                rest.find('>').map(|e| e + 1)
            };
            match skip_to {
                Some(n) => {
                    i += n;
                    out.push(' ');
                }
                None => break,
            }
        } else {
            let next = h[i..].find('<').map(|n| i + n).unwrap_or(h.len());
            out.push_str(&h[i..next]);
            i = next;
        }
    }
    out.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'")
}

// ---- messages ------------------------------------------------------------------------------------

pub fn delivery_state(e: &SentEmail) -> Option<DeliveryState> {
    e.last_event.as_deref().and_then(DeliveryState::parse)
}

/// Mailboxes of a sent email from its state: Scheduled, Trash (canceled) or Sent (+ Bounced).
pub fn sent_mailboxes(state: Option<DeliveryState>) -> Vec<MailboxId> {
    match state {
        Some(DeliveryState::Scheduled) => vec![MailboxId::new(SCHEDULED)],
        Some(DeliveryState::Canceled) => vec![MailboxId::new(TRASH)],
        Some(s) if s.is_problem() => vec![MailboxId::new(SENT), MailboxId::new(BOUNCED)],
        _ => vec![MailboxId::new(SENT)],
    }
}

/// (year, month, day, seconds of the day) of a time in ms.
pub(crate) fn civil(ms: i64) -> (i64, i64, i64, i64) {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // civil_from_days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d, rem)
}

fn utc_minute(ms: i64) -> String {
    let (y, m, d, rem) = civil(ms);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", rem / 3600, rem % 3600 / 60)
}

/// The list row of a sent email. Resend's list has no text, so the snippet says what happened to it
/// ("Delivered", "Bounced", "Scheduled for 2026-11-12 09:00 UTC"); the row is re-sent when that changes.
pub fn sent_meta(account: &AccountId, e: &SentEmail) -> MessageMeta {
    let state = delivery_state(e);
    let created = e.created_at.as_deref().and_then(parse_time_ms).unwrap_or(0);
    let scheduled = e.scheduled_at.as_deref().and_then(parse_time_ms);
    let to = addresses(&e.to);
    let subject = e.subject.clone().unwrap_or_default();
    let counterpart = to.first().map(|a| a.email.clone()).unwrap_or_default();
    let snippet = match (state, scheduled) {
        (Some(DeliveryState::Scheduled), Some(t)) => format!("Scheduled for {}", utc_minute(t)),
        (Some(s), _) => s.label().to_owned(),
        (None, _) => String::new(),
    };
    let date = match state {
        Some(DeliveryState::Scheduled) => scheduled.unwrap_or(created),
        _ => created,
    };
    MessageMeta {
        id: message_id(Kind::Sent, &e.id),
        account: account.clone(),
        thread: thread_id(&subject, &counterpart),
        mailboxes: sent_mailboxes(state),
        from: e.from.as_deref().and_then(parse_address).into_iter().collect(),
        to,
        cc: addresses(&e.cc),
        bcc: addresses(&e.bcc),
        subject,
        snippet,
        date: date.div_euclid(1000),
        flags: Flags { seen: true, flagged: false, answered: false, draft: false },
        has_attachments: false,
        size: 0,
        rfc_message_id: e.message_id.as_deref().map(strip_angles).filter(|s| !s.is_empty()),
        in_reply_to: None,
        references: Vec::new(),
        partial: false,
    }
}

/// A header's value(s) from Resend's `headers` object (a string or a list of strings per name).
fn header_values(e: &ReceivedEmail, name: &str) -> Vec<String> {
    let Some(h) = &e.headers else { return Vec::new() };
    h.iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(name))
        .flat_map(|(_, v)| match v {
            serde_json::Value::String(s) => vec![s.clone()],
            serde_json::Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect(),
            serde_json::Value::Null => Vec::new(),
            other => vec![other.to_string()],
        })
        .map(|v| v.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

/// The list row of received mail. `e` may be the list item (no text, no headers) or the full email.
pub fn received_meta(account: &AccountId, e: &ReceivedEmail) -> MessageMeta {
    let from: Vec<Address> = e.from.as_deref().and_then(parse_address).into_iter().collect();
    let subject = e.subject.clone().unwrap_or_default();
    let counterpart = from.first().map(|a| a.email.clone()).unwrap_or_default();
    let in_reply_to = header_values(e, "In-Reply-To").first().map(|v| strip_angles(v)).filter(|s| !s.is_empty());
    let references = header_values(e, "References").iter().flat_map(|v| message_ids(v)).collect();
    let size = e.attachments.iter().map(|a| a.size).sum::<u64>()
        + e.text.as_ref().map_or(0, |t| t.len() as u64)
        + e.html.as_ref().map_or(0, |t| t.len() as u64);
    MessageMeta {
        id: message_id(Kind::Received, &e.id),
        account: account.clone(),
        thread: thread_id(&subject, &counterpart),
        mailboxes: vec![MailboxId::new(INBOX)],
        from,
        to: addresses(&e.to),
        cc: addresses(&e.cc),
        bcc: addresses(&e.bcc),
        subject,
        snippet: snippet(e.text.as_deref(), e.html.as_deref()),
        date: e.created_at.as_deref().and_then(parse_time_ms).unwrap_or(0).div_euclid(1000),
        flags: Flags::default(),
        has_attachments: e.attachments.iter().any(|a| a.content_disposition.as_deref() != Some("inline")),
        size,
        rfc_message_id: e.message_id.as_deref().map(strip_angles).filter(|s| !s.is_empty()),
        in_reply_to,
        references,
        partial: false,
    }
}

pub fn attachment(a: &AttachmentInfo) -> Attachment {
    Attachment {
        id: AttachmentId::new(&a.id),
        filename: a.filename.clone().filter(|f| !f.trim().is_empty()).unwrap_or_else(|| "attachment".into()),
        mime: a.content_type.clone().unwrap_or_else(|| "application/octet-stream".into()),
        size: a.size,
        content_id: a.content_id.as_deref().map(strip_angles).filter(|s| !s.is_empty()),
    }
}

/// Headers kept in a received body besides [`BODY_HEADERS`].
const MORE_HEADERS: &[&str] =
    &["Message-ID", "In-Reply-To", "References", "Date", "Sender", "Return-Path", "Received-SPF", "DKIM-Signature", "X-Mailer"];

/// The body of received mail: text, HTML (as received; only the render crate makes it safe), the
/// headers of [`BODY_HEADERS`] and a few more (each occurrence), and the attachments.
/// `Authentication-Results` is made from Resend's `authentication` results when the message has none.
pub fn received_body(e: &ReceivedEmail) -> Body {
    let mut headers = Vec::new();
    for name in BODY_HEADERS.iter().chain(MORE_HEADERS) {
        for v in header_values(e, name) {
            headers.push(((*name).to_owned(), v));
        }
    }
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("Reply-To")) && !e.reply_to.is_empty() {
        headers.push(("Reply-To".into(), e.reply_to.join(", ")));
    }
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("Authentication-Results"))
        && let Some(auth) = &e.authentication
    {
        let parts: Vec<String> = ["spf", "dkim", "dmarc"]
            .iter()
            .filter_map(|k| auth.get(*k).and_then(|v| v.as_str()).map(|v| format!("{k}={}", v.split_whitespace().next().unwrap_or("unknown"))))
            .collect();
        if !parts.is_empty() {
            headers.push(("Authentication-Results".into(), format!("resend.com; {}", parts.join("; "))));
        }
    }
    Body {
        text: e.text.clone().filter(|t| !t.is_empty()),
        html: e.html.clone().filter(|t| !t.is_empty()),
        attachments: e.attachments.iter().map(attachment).collect(),
        headers,
    }
}

/// The body of an email we sent: text and HTML as Resend kept them, Reply-To, the Message-ID and the
/// delivery state in [`DELIVERY_HEADER`].
pub fn sent_body(e: &SentEmail, attachments: &[AttachmentInfo]) -> Body {
    let mut headers = Vec::new();
    if !e.reply_to.is_empty() {
        headers.push(("Reply-To".to_owned(), e.reply_to.join(", ")));
    }
    if let Some(m) = &e.message_id {
        headers.push(("Message-ID".to_owned(), m.clone()));
    }
    if let Some(s) = delivery_state(e) {
        headers.push((DELIVERY_HEADER.to_owned(), s.as_str().to_owned()));
    }
    Body {
        text: e.text.clone().filter(|t| !t.is_empty()),
        html: e.html.clone().filter(|t| !t.is_empty()),
        attachments: attachments.iter().map(attachment).collect(),
        headers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_time_formats() {
        let a = parse_time_ms("2023-04-03 22:13:42.674981+00").unwrap();
        let b = parse_time_ms("2023-04-03T22:13:42.674Z").unwrap();
        assert_eq!(a, b);
        assert_eq!(a, 1_680_560_022_674);
        assert_eq!(parse_time_ms("2023-04-03T23:13:42+01:00").unwrap(), 1_680_560_022_000);
        assert_eq!(parse_time_ms("2023-04-03T21:13:42-0100").unwrap(), 1_680_560_022_000);
        assert_eq!(parse_time_ms("nope"), None);
        assert_eq!(utc_minute(a), "2023-04-03 22:13 UTC");
    }

    #[test]
    fn addresses_both_ways() {
        let a = parse_address("Acme Shop <Orders@Shop.example>").unwrap();
        assert_eq!((a.name.as_deref(), a.email.as_str()), (Some("Acme Shop"), "orders@shop.example"));
        let q = parse_address("\"Doe, Jo\" <jo@x.example>").unwrap();
        assert_eq!(q.name.as_deref(), Some("Doe, Jo"));
        assert_eq!(parse_address("jo@x.example").unwrap().name, None);
        assert!(parse_address("nobody").is_none());
        assert_eq!(format_address(&Address::new(Some("Jo \"J\"\r\nBcc: x@evil.example"), "jo@x.example")).unwrap(), "\"Jo JBcc: x@evil.example\" <jo@x.example>");
        assert_eq!(format_address(&Address::new(None, "jo@x.example")).unwrap(), "jo@x.example");
        assert!(format_address(&Address::new(None, "jo@x.example>, y@z")).is_err());
        assert!(format_address(&Address::new(None, "jo")).is_err());
    }

    #[test]
    fn threads_by_subject_and_other_party() {
        let a = thread_id("Quote for the shelves", "ada@example.com");
        assert_eq!(a, thread_id("RE: Re:  Quote for the shelves", "Ada@Example.com"));
        assert_eq!(a, thread_id("Fwd: quote FOR the shelves", "ada@example.com"));
        assert_ne!(a, thread_id("Quote for the shelves", "bo@example.com"));
        assert_eq!(message_ids("<a@x> <b@y>"), vec!["a@x", "b@y"]);
    }

    #[test]
    fn snippets_from_html() {
        assert_eq!(snippet(None, Some("<html><head><style>p{}</style></head><p>Hi&nbsp;<b>Ada</b></p><!-- x --></html>")), "Hi Ada");
        assert_eq!(snippet(Some("  plain\n text "), Some("<p>x</p>")), "plain text");
    }

    #[test]
    fn ids_round_trip() {
        let id = message_id(Kind::Received, "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(split_id(&id).unwrap(), (Kind::Received, "550e8400-e29b-41d4-a716-446655440000"));
        assert!(split_id(&MessageId::new("INBOX:1:2")).is_err());
        assert!(split_id(&MessageId::new("s:../../domains")).is_err());
    }

    #[test]
    fn sent_state_picks_mailboxes() {
        assert_eq!(sent_mailboxes(Some(DeliveryState::Bounced)), vec![MailboxId::new(SENT), MailboxId::new(BOUNCED)]);
        assert_eq!(sent_mailboxes(Some(DeliveryState::Scheduled)), vec![MailboxId::new(SCHEDULED)]);
        assert_eq!(sent_mailboxes(Some(DeliveryState::Canceled)), vec![MailboxId::new(TRASH)]);
        assert_eq!(sent_mailboxes(None), vec![MailboxId::new(SENT)]);
    }
}
