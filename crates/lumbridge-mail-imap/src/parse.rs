//! From message bytes to the core model, through `mail-parser`. Mail is hostile input: everything here is
//! parsing and trimming, nothing is interpreted.

use crate::ids::{self, MessageKey};
use lumbridge_mail_core::{AccountId, Address, Attachment, AttachmentId, Body, Flags, MessageMeta};
use mail_parser::{HeaderValue, MessageParser, MimeHeaders};

/// Headers worth keeping on `Body` for the reader and the AI rules.
const KEPT_HEADERS: &[&str] = &[
    "List-Unsubscribe",
    "List-Unsubscribe-Post",
    "List-Id",
    "List-Post",
    "Reply-To",
    "Authentication-Results",
    "ARC-Authentication-Results",
    "Received-SPF",
    "DKIM-Signature",
    "Precedence",
    "Auto-Submitted",
    "X-Auto-Response-Suppress",
    "Return-Path",
    "Sender",
    "X-Mailer",
    "Feedback-ID",
    "X-GitHub-Reason",
];

const SNIPPET_CHARS: usize = 200;

fn addresses(list: Option<&mail_parser::Address<'_>>) -> Vec<Address> {
    let Some(list) = list else { return Vec::new() };
    list.iter()
        .filter_map(|m| {
            let email = m.address()?.trim();
            (!email.is_empty()).then(|| Address::new(m.name(), email))
        })
        .collect()
}

/// Message-ID lists (`In-Reply-To`, `References`), without angle brackets.
fn text_list(v: &HeaderValue<'_>) -> Vec<String> {
    let items: Vec<String> = match v {
        HeaderValue::TextList(l) => l.iter().map(|s| s.to_string()).collect(),
        HeaderValue::Text(t) => vec![t.to_string()],
        _ => Vec::new(),
    };
    items
        .into_iter()
        .map(|s| s.trim().trim_start_matches('<').trim_end_matches('>').trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// What the server told us about a message besides its bytes.
#[derive(Debug, Clone, Default)]
pub struct FetchInfo {
    pub flags: Flags,
    /// INTERNALDATE, Unix seconds.
    pub internal_date: Option<i64>,
    pub size: Option<u32>,
}

fn snippet(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(SNIPPET_CHARS).collect();
    if collapsed.chars().count() > SNIPPET_CHARS {
        out.push('…');
    }
    out
}

/// Builds the list-row view of a message from the first `preview` bytes of it. `truncated` says whether
/// the bytes stop before the end of the message.
pub fn meta(account: &AccountId, key: &MessageKey, bytes: &[u8], truncated: bool, info: &FetchInfo) -> MessageMeta {
    let parsed = MessageParser::default().parse(bytes);
    let id = key.id();
    let Some(msg) = parsed else {
        return MessageMeta {
            thread: ids::thread_id(None, None, &[], "", id.as_str()),
            id,
            account: account.clone(),
            mailboxes: vec![key.mailbox()],
            from: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: String::new(),
            snippet: String::new(),
            date: info.internal_date.unwrap_or(0),
            flags: info.flags,
            has_attachments: false,
            size: info.size.map(u64::from).unwrap_or(bytes.len() as u64),
            rfc_message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            partial: false,
        };
    };
    let subject = msg.subject().unwrap_or("").trim().to_owned();
    let rfc_message_id = msg.message_id().map(|m| m.trim().to_owned()).filter(|m| !m.is_empty());
    let in_reply_to = text_list(msg.in_reply_to()).into_iter().next();
    let references = text_list(msg.references());
    let thread = ids::thread_id(rfc_message_id.as_deref(), in_reply_to.as_deref(), &references, &subject, id.as_str());
    let date = info.internal_date.or_else(|| msg.date().map(|d| d.to_timestamp())).unwrap_or(0);
    let text = msg.body_preview(SNIPPET_CHARS * 4).map(|t| t.into_owned()).unwrap_or_default();
    let multipart_mixed = msg.content_type().is_some_and(|ct| {
        ct.ctype().eq_ignore_ascii_case("multipart") && ct.subtype().is_some_and(|s| s.eq_ignore_ascii_case("mixed"))
    });
    let has_attachments = msg.attachment_count() > 0 || (truncated && multipart_mixed);
    MessageMeta {
        id,
        account: account.clone(),
        thread,
        mailboxes: vec![key.mailbox()],
        from: addresses(msg.from()),
        to: addresses(msg.to()),
        cc: addresses(msg.cc()),
        bcc: addresses(msg.bcc()),
        subject,
        snippet: snippet(&text),
        date,
        flags: info.flags,
        has_attachments,
        size: info.size.map(u64::from).unwrap_or(bytes.len() as u64),
        rfc_message_id,
        in_reply_to,
        references,
        partial: false,
    }
}

/// The readable content of a whole message.
pub fn body(raw: &[u8]) -> Body {
    let Some(msg) = MessageParser::default().parse(raw) else {
        return Body { text: Some(String::from_utf8_lossy(raw).into_owned()), ..Default::default() };
    };
    // `body_html` would turn a text-only message into HTML; only keep HTML the sender wrote.
    let html = msg
        .html_bodies()
        .any(|p| p.is_text_html())
        .then(|| msg.body_html(0).map(|h| h.into_owned()))
        .flatten();
    let text = msg.body_text(0).map(|t| t.into_owned());
    let attachments = msg
        .attachments()
        .enumerate()
        .map(|(i, part)| {
            let mime = part
                .content_type()
                .map(|ct| match ct.subtype() {
                    Some(sub) => format!("{}/{}", ct.ctype(), sub),
                    None => ct.ctype().to_owned(),
                })
                .unwrap_or_else(|| "application/octet-stream".into())
                .to_ascii_lowercase();
            Attachment {
                id: AttachmentId::new(format!("a{i}")),
                filename: part.attachment_name().map(sanitise_filename).unwrap_or_else(|| format!("attachment-{}", i + 1)),
                mime,
                size: part.contents().len() as u64,
                content_id: part.content_id().map(|c| c.trim().trim_start_matches('<').trim_end_matches('>').to_owned()),
            }
        })
        .collect();
    let mut kept = Vec::new();
    for name in KEPT_HEADERS {
        for (n, v) in msg.headers_raw() {
            if n.eq_ignore_ascii_case(name) {
                kept.push(((*name).to_owned(), v.split_whitespace().collect::<Vec<_>>().join(" ")));
            }
        }
    }
    Body { text, html, attachments, headers: kept }
}

/// The decoded bytes of attachment `id` (as numbered by `body`).
pub fn attachment(raw: &[u8], id: &AttachmentId) -> Option<Vec<u8>> {
    let index: usize = id.as_str().strip_prefix('a')?.parse().ok()?;
    let msg = MessageParser::default().parse(raw)?;
    let part = msg.attachments().nth(index)?;
    Some(part.contents().to_vec())
}

/// A file name from a message is a suggestion: no paths, no control characters.
fn sanitise_filename(name: &str) -> String {
    let leaf = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let clean: String = leaf.chars().filter(|c| !c.is_control()).collect::<String>().trim().trim_start_matches('.').to_owned();
    if clean.is_empty() { "attachment".into() } else { clean }
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

    const MAIL: &[u8] = b"From: Ada Lovelace <Ada@Example.com>\r\n\
To: Bob <bob@example.org>, carol@example.org\r\n\
Subject: Re: Engine notes\r\n\
Message-ID: <m2@example.com>\r\n\
In-Reply-To: <m1@example.com>\r\n\
References: <m0@example.com> <m1@example.com>\r\n\
Date: Tue, 10 Nov 2026 09:00:00 +0000\r\n\
List-Unsubscribe: <https://example.com/unsub>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\
\r\n\
--b1\r\n\
Content-Type: multipart/alternative; boundary=\"b2\"\r\n\
\r\n\
--b2\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
The   engine\r\nworks.\r\n\
--b2\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>The engine <b>works</b>.</p>\r\n\
--b2--\r\n\
--b1\r\n\
Content-Type: application/pdf; name=\"../../etc/notes.pdf\"\r\n\
Content-Disposition: attachment; filename=\"../../etc/notes.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0xLjQK\r\n\
--b1--\r\n";

    fn key() -> MessageKey {
        MessageKey { folder: "INBOX".into(), validity: 3, uid: 9 }
    }

    #[test]
    fn meta_from_headers_and_preview() {
        let info = FetchInfo { flags: Flags { seen: true, ..Default::default() }, internal_date: Some(1_800_000_000), size: Some(999) };
        let m = meta(&AccountId::new("acct"), &key(), MAIL, false, &info);
        assert_eq!(m.id.as_str(), "INBOX:3:9");
        assert_eq!(m.from, vec![Address::new(Some("Ada Lovelace"), "ada@example.com")]);
        assert_eq!(m.to.len(), 2);
        assert_eq!(m.subject, "Re: Engine notes");
        assert_eq!(m.snippet, "The engine works.");
        assert!(m.has_attachments);
        assert!(m.flags.seen);
        assert_eq!(m.date, 1_800_000_000);
        assert_eq!(m.size, 999);
        assert_eq!(m.rfc_message_id.as_deref(), Some("m2@example.com"));
        assert_eq!(m.in_reply_to.as_deref(), Some("m1@example.com"));
        assert_eq!(m.references, vec!["m0@example.com".to_owned(), "m1@example.com".to_owned()]);
        assert_eq!(m.thread, ids::thread_id(Some("m0@example.com"), None, &[], "x", "y"));
    }

    #[test]
    fn truncated_mixed_mail_still_hints_attachments() {
        let head = &MAIL[..MAIL.windows(5).position(|w| w == b"--b2\r").unwrap() + 40];
        let m = meta(&AccountId::new("a"), &key(), head, true, &FetchInfo::default());
        assert!(m.has_attachments);
        // Date header when there is no INTERNALDATE.
        assert_eq!(m.date, 1_794_301_200);
    }

    #[test]
    fn body_parts_and_attachments() {
        let b = body(MAIL);
        assert_eq!(b.text.as_deref().map(str::trim), Some("The   engine\r\nworks.".trim()));
        assert!(b.html.as_deref().unwrap().contains("<b>works</b>"));
        assert_eq!(b.attachments.len(), 1);
        let a = &b.attachments[0];
        assert_eq!((a.id.as_str(), a.filename.as_str(), a.mime.as_str()), ("a0", "notes.pdf", "application/pdf"));
        assert!(b.headers.iter().any(|(k, v)| k == "List-Unsubscribe" && v.contains("unsub")));
        assert_eq!(attachment(MAIL, &a.id).unwrap(), b"%PDF-1.4\n");
        assert!(attachment(MAIL, &AttachmentId::new("a5")).is_none());
        assert!(attachment(MAIL, &AttachmentId::new("x")).is_none());
    }

    #[test]
    fn plain_text_mail_has_no_html() {
        let b = body(b"From: a@b.c\r\nSubject: hi\r\n\r\nJust text\r\n");
        assert_eq!(b.text.as_deref(), Some("Just text\r\n"));
        assert!(b.html.is_none());
        assert!(b.attachments.is_empty());
    }
}
