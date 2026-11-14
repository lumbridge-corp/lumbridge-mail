//! Row shapes for the mirror, built from core types. Whoever builds them decides what may be
//! mirrored (the daemon's mirror module: anon accounts never, sensitive mail as metadata and its one
//! line); these constructors make the safe shape the easy one:
//! - [`AccountRow::from_account`] returns `None` for an anon account.
//! - [`MessageRow::sensitive`] replaces the subject with the one line and drops the snippet, and a
//!   sensitive message's [`Change::UpsertMessage`] always drops its body ([`Change::message`]).
//! - [`BodyRow::from_body`] keeps clean text and the sanitised block tree, never the received HTML.

use lumbridge_mail_core::{
    Account, Address, BODY_HEADERS, Body, DELIVERY_HEADER, Mailbox, MessageMeta, Persona, ThreadId,
    Triage,
};
use lumbridge_mail_render::{Policy, TextOptions};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountRow {
    pub id: String,
    pub persona: String,
    pub label: String,
    pub email: String,
    pub name: String,
    pub provider: String,
    pub color: Option<String>,
    pub ai_policy: String,
    pub readonly: bool,
    pub kept: bool,
}

fn wire(v: &impl Serialize) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

impl AccountRow {
    /// None for an anon account: those are never mirrored, not even this row.
    pub fn from_account(a: &Account, kept: bool) -> Option<AccountRow> {
        if a.persona == Persona::Anon {
            return None;
        }
        Some(AccountRow {
            id: a.id.0.clone(),
            persona: match &a.persona {
                Persona::Other(s) => format!("other:{s}"),
                p => wire(p),
            },
            label: a.label.clone(),
            email: a.email.clone(),
            name: a.name.clone(),
            provider: wire(&a.provider),
            color: a.color.clone(),
            ai_policy: wire(&a.ai),
            readonly: a.readonly,
            kept,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MailboxRow {
    pub account: String,
    pub id: String,
    pub name: String,
    pub role: String,
}

impl MailboxRow {
    pub fn from_mailbox(m: &Mailbox) -> MailboxRow {
        MailboxRow {
            account: m.account.0.clone(),
            id: m.id.0.clone(),
            name: m.name.clone(),
            role: m.role.as_str().into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MessageRow {
    pub account: String,
    pub id: String,
    pub thread_id: String,
    pub mailbox_ids: Vec<String>,
    pub subject: String,
    pub snippet: String,
    pub from_addrs: Value,
    pub to_addrs: Value,
    pub cc_addrs: Value,
    pub bcc_addrs: Value,
    pub from_email: String,
    pub from_name: Option<String>,
    pub participants: String,
    /// Unix seconds; None when the message has no date.
    pub sent_at: Option<i64>,
    pub seen: bool,
    pub flagged: bool,
    pub answered: bool,
    pub draft: bool,
    pub has_attachments: bool,
    pub size_bytes: i64,
    pub rfc_message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub refs: Vec<String>,
    pub sensitive_kind: Option<String>,
}

fn addrs(list: &[Address]) -> Value {
    Value::Array(
        list.iter()
            .map(|a| json!({ "name": a.name, "email": a.email }))
            .collect(),
    )
}

fn participants(m: &MessageMeta) -> String {
    let mut out: Vec<String> = Vec::new();
    for a in m.from.iter().chain(&m.to).chain(&m.cc) {
        let s = match &a.name {
            Some(n) => format!("{n} {}", a.email),
            None => a.email.clone(),
        };
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out.join(", ")
}

impl MessageRow {
    /// A message that is not sensitive, as it is.
    pub fn from_meta(m: &MessageMeta) -> MessageRow {
        let from = m.from.first();
        MessageRow {
            account: m.account.0.clone(),
            id: m.id.0.clone(),
            thread_id: m.thread.0.clone(),
            mailbox_ids: m.mailboxes.iter().map(|b| b.0.clone()).collect(),
            subject: m.subject.clone(),
            snippet: m.snippet.clone(),
            from_addrs: addrs(&m.from),
            to_addrs: addrs(&m.to),
            cc_addrs: addrs(&m.cc),
            bcc_addrs: addrs(&m.bcc),
            from_email: from.map(|a| a.email.clone()).unwrap_or_default(),
            from_name: from.and_then(|a| a.name.clone()),
            participants: participants(m),
            sent_at: (m.date != 0).then_some(m.date),
            seen: m.flags.seen,
            flagged: m.flags.flagged,
            answered: m.flags.answered,
            draft: m.flags.draft,
            has_attachments: m.has_attachments,
            size_bytes: i64::try_from(m.size).unwrap_or(i64::MAX),
            rfc_message_id: m.rfc_message_id.clone(),
            in_reply_to: m.in_reply_to.clone(),
            refs: m.references.clone(),
            sensitive_kind: None,
        }
    }

    /// Sensitive mail: metadata only. The subject becomes `line` ("A security code from Acme"), the
    /// snippet goes, and `kind` ("A security code") is kept so readers know why.
    pub fn sensitive(m: &MessageMeta, kind: &str, line: &str) -> MessageRow {
        MessageRow {
            subject: line.to_owned(),
            snippet: String::new(),
            sensitive_kind: Some(kind.to_owned()),
            ..Self::from_meta(m)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AttachmentRow {
    pub id: String,
    pub filename: String,
    pub mime: String,
    pub size_bytes: i64,
    pub content_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BodyRow {
    pub account: String,
    pub message_id: String,
    pub body_text: String,
    pub quoted_text: String,
    pub blocks: Value,
    pub headers: Value,
    /// schema.org objects from the HTML (JSON-LD, microdata), `[]` when none: see
    /// [`BodyRow::with_structured`]. Schema version 2.
    pub structured: Value,
    pub content_hash: String,
    pub attachments: Vec<AttachmentRow>,
}

/// Largest `structured` value kept (serialised); bigger is left out.
pub const MAX_STRUCTURED_BYTES: usize = 64 << 10;

/// Longest text kept per body (the index caps what it reads anyway).
pub const MAX_TEXT_CHARS: usize = 200_000;
pub const MAX_QUOTED_CHARS: usize = 100_000;
/// A block tree bigger than this (serialised) is left out; the text is still there.
pub const MAX_BLOCKS_BYTES: usize = 2 << 20;

impl BodyRow {
    /// Clean text and the sanitised block tree of a body (remote content blocked, as in the reader),
    /// selected headers, and the attachment list. The received HTML itself is never kept.
    pub fn from_body(account: &str, message_id: &str, body: &Body) -> BodyRow {
        let text_opts = TextOptions {
            quoted: false,
            signature: true,
            links: true,
            images: false,
            max_chars: MAX_TEXT_CHARS,
        };
        let body_text = lumbridge_mail_render::plain_text(body, &text_opts);
        let doc = lumbridge_mail_render::blocks(body, &Policy::default());
        let quoted_text = if doc.quoted.is_empty() {
            String::new()
        } else {
            let q = lumbridge_mail_render::text::render(
                &doc.quoted,
                &TextOptions {
                    links: true,
                    ..TextOptions::default()
                },
            );
            q.chars().take(MAX_QUOTED_CHARS).collect()
        };
        let mut blocks = serde_json::to_value(&doc).unwrap_or(Value::Null);
        if serde_json::to_string(&blocks).map(|s| s.len()).unwrap_or(0) > MAX_BLOCKS_BYTES {
            blocks = json!({ "omitted": "too large" });
        }
        let headers = Value::Array(
            body.headers
                .iter()
                .filter(|(k, _)| {
                    BODY_HEADERS
                        .iter()
                        .chain(&[DELIVERY_HEADER])
                        .any(|h| h.eq_ignore_ascii_case(k))
                })
                .map(|(k, v)| json!([k, v]))
                .collect(),
        );
        let content_hash = body_hash(&body_text, &quoted_text, &blocks, &headers, &Value::Null);
        let attachments = body
            .attachments
            .iter()
            .map(|a| AttachmentRow {
                id: a.id.0.clone(),
                filename: a.filename.clone(),
                mime: a.mime.clone(),
                size_bytes: i64::try_from(a.size).unwrap_or(i64::MAX),
                content_id: a.content_id.clone(),
            })
            .collect();
        BodyRow {
            account: account.to_owned(),
            message_id: message_id.to_owned(),
            body_text,
            quoted_text,
            blocks,
            headers,
            structured: json!([]),
            content_hash,
            attachments,
        }
    }

    /// Adds the schema.org objects found in the HTML (the daemon extracts them; this crate does not
    /// parse HTML). An array, size-capped; the content hash covers it (and stays what it was when
    /// there is none, so bodies mirrored before version 2 are not all re-sent).
    pub fn with_structured(mut self, structured: Value) -> BodyRow {
        let keep = matches!(&structured, Value::Array(a) if !a.is_empty())
            && serde_json::to_string(&structured)
                .map(|s| s.len())
                .unwrap_or(usize::MAX)
                <= MAX_STRUCTURED_BYTES;
        self.structured = if keep { structured } else { json!([]) };
        let extra = if keep {
            self.structured.clone()
        } else {
            Value::Null
        };
        self.content_hash = body_hash(
            &self.body_text,
            &self.quoted_text,
            &self.blocks,
            &self.headers,
            &extra,
        );
        self
    }
}

fn body_hash(
    body_text: &str,
    quoted_text: &str,
    blocks: &Value,
    headers: &Value,
    structured: &Value,
) -> String {
    let mut h = Sha256::new();
    for part in [
        body_text,
        quoted_text,
        &blocks.to_string(),
        &headers.to_string(),
    ] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    if !structured.is_null() {
        h.update(structured.to_string().as_bytes());
        h.update([0u8]);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TriageRow {
    pub account: String,
    pub thread_id: String,
    pub category: String,
    pub priority: i16,
    pub summary: String,
    pub needs_reply: bool,
    pub due_at: Option<i64>,
    pub by: String,
    pub at: i64,
    pub suspicious: bool,
}

impl TriageRow {
    /// `sensitive_line`: the thread holds sensitive mail; its summary is that line, whatever the
    /// triage said.
    pub fn from_triage(
        account: &str,
        thread: &ThreadId,
        t: &Triage,
        sensitive_line: Option<&str>,
    ) -> TriageRow {
        TriageRow {
            account: account.to_owned(),
            thread_id: thread.0.clone(),
            category: t.category.as_str().into(),
            priority: i16::from(t.priority.min(100)),
            summary: sensitive_line
                .map(str::to_owned)
                .unwrap_or_else(|| t.summary.clone()),
            needs_reply: t.needs_reply,
            due_at: t.due,
            by: t.by.clone(),
            at: t.at,
            suspicious: t.suspicious,
        }
    }
}

/// What to do with a message's body when its metadata is written.
#[derive(Debug, Clone, PartialEq)]
pub enum BodyChange {
    /// Leave whatever is there.
    Keep,
    /// Write this body and its attachment list (replacing what is there).
    Put(Box<BodyRow>),
    /// Remove the body, attachments, embeddings and parsed facts (the message is sensitive).
    Drop,
}

/// One idempotent write.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    UpsertAccount(AccountRow),
    /// Removes the account and everything mirrored for it.
    DeleteAccount {
        id: String,
    },
    UpsertMailbox(MailboxRow),
    DeleteMailbox {
        account: String,
        id: String,
    },
    UpsertMessage {
        row: Box<MessageRow>,
        body: BodyChange,
    },
    /// The message is gone from the server: removed here with its body, attachments, embeddings and
    /// parsed facts.
    DeleteMessage {
        account: String,
        id: String,
    },
    UpsertTriage(TriageRow),
    DeleteTriage {
        account: String,
        thread_id: String,
    },
}

impl Change {
    /// A message write; a sensitive message never carries a body (it is dropped instead).
    pub fn message(row: MessageRow, body: BodyChange) -> Change {
        let body = if row.sensitive_kind.is_some() {
            BodyChange::Drop
        } else {
            body
        };
        Change::UpsertMessage {
            row: Box::new(row),
            body,
        }
    }

    /// The account this change belongs to.
    pub fn account(&self) -> &str {
        match self {
            Change::UpsertAccount(a) => &a.id,
            Change::DeleteAccount { id } => id,
            Change::UpsertMailbox(m) => &m.account,
            Change::DeleteMailbox { account, .. }
            | Change::DeleteMessage { account, .. }
            | Change::DeleteTriage { account, .. } => account,
            Change::UpsertMessage { row, .. } => &row.account,
            Change::UpsertTriage(t) => &t.account,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumbridge_mail_core::*;

    fn meta() -> MessageMeta {
        MessageMeta {
            id: MessageId::new("m1"),
            account: AccountId::new("personal"),
            thread: ThreadId::new("t1"),
            mailboxes: vec![MailboxId::new("INBOX")],
            from: vec![Address::new(Some("Acme"), "no-reply@acme.example")],
            to: vec![Address::new(Some("Sam"), "sam@example.com")],
            cc: vec![],
            bcc: vec![],
            subject: "Your Acme verification code".into(),
            snippet: "Your code is 482913".into(),
            date: 1_700_000_000,
            flags: Flags::default(),
            has_attachments: false,
            size: 10,
            rfc_message_id: None,
            in_reply_to: None,
            references: vec![],
            partial: false,
        }
    }

    #[test]
    fn anon_accounts_have_no_row() {
        let mut a = Account {
            id: AccountId::new("forum"),
            persona: Persona::Anon,
            label: "Forum".into(),
            email: "otter@anon.example".into(),
            name: "Otter".into(),
            provider: ProviderKind::Demo,
            color: None,
            ai: AiPolicy::LocalOnly,
            readonly: false,
        };
        assert!(AccountRow::from_account(&a, false).is_none());
        a.persona = Persona::Other("side".into());
        assert_eq!(
            AccountRow::from_account(&a, true).unwrap().persona,
            "other:side"
        );
        a.persona = Persona::Work;
        let row = AccountRow::from_account(&a, false).unwrap();
        assert_eq!(
            (
                row.persona.as_str(),
                row.provider.as_str(),
                row.ai_policy.as_str()
            ),
            ("work", "demo", "local_only")
        );
    }

    #[test]
    fn sensitive_messages_keep_only_metadata_and_never_a_body() {
        let row = MessageRow::sensitive(&meta(), "A security code", "A security code from Acme");
        assert_eq!(row.subject, "A security code from Acme");
        assert!(row.snippet.is_empty());
        assert_eq!(row.from_email, "no-reply@acme.example");
        let body = BodyRow::from_body(
            "personal",
            "m1",
            &Body {
                text: Some("Your code is 482913".into()),
                ..Body::default()
            },
        );
        match Change::message(row, BodyChange::Put(Box::new(body))) {
            Change::UpsertMessage { body, .. } => assert_eq!(body, BodyChange::Drop),
            other => panic!("{other:?}"),
        }
        let plain = MessageRow::from_meta(&meta());
        assert_eq!(
            plain.participants,
            "Acme no-reply@acme.example, Sam sam@example.com"
        );
        assert_eq!(plain.sent_at, Some(1_700_000_000));
    }

    #[test]
    fn bodies_keep_clean_text_and_blocks_never_html() {
        let html = r#"<html><body><p>Hello <b>Sam</b>, see <a href="https://shop.example/x?utm_source=mail">the order</a>.</p>
            <script>alert(1)</script><img src="https://tracker.example/p.gif" width="1" height="1">
            <blockquote>On Monday Ada wrote: earlier words</blockquote></body></html>"#;
        let body = Body {
            text: None,
            html: Some(html.into()),
            attachments: vec![Attachment {
                id: AttachmentId::new("a1"),
                filename: "receipt.pdf".into(),
                mime: "application/pdf".into(),
                size: 1234,
                content_id: None,
            }],
            headers: vec![
                ("List-Id".into(), "<news.example>".into()),
                ("X-Secret-Internal".into(), "nope".into()),
            ],
        };
        let row = BodyRow::from_body("personal", "m1", &body);
        assert!(row.body_text.contains("Hello Sam"), "{}", row.body_text);
        let all = serde_json::to_string(&row).unwrap();
        assert!(
            !all.contains("<script") && !all.contains("alert(1)") && !all.contains("<p>"),
            "{all}"
        );
        assert!(!all.contains("tracker.example/p.gif"));
        assert!(
            !all.contains("X-Secret-Internal"),
            "only the headers we name"
        );
        assert_eq!(row.attachments.len(), 1);
        assert_eq!(row.content_hash.len(), 64);
        assert_eq!(
            BodyRow::from_body("personal", "m1", &body).content_hash,
            row.content_hash,
            "stable"
        );
        // Structured data: none keeps the hash; some changes it; too much is left out.
        let same = row.clone().with_structured(json!([]));
        assert_eq!(same.content_hash, row.content_hash);
        let with = row
            .clone()
            .with_structured(json!([{"@type": "Order", "orderNumber": "A-1"}]));
        assert_ne!(with.content_hash, row.content_hash);
        assert_eq!(with.structured[0]["orderNumber"], "A-1");
        let huge = row
            .clone()
            .with_structured(json!([{"@type": "Order", "x": "y".repeat(MAX_STRUCTURED_BYTES)}]));
        assert_eq!(huge.structured, json!([]));
        assert_eq!(huge.content_hash, row.content_hash);
    }

    #[test]
    fn sensitive_threads_have_the_line_as_their_summary() {
        let t = Triage {
            category: Category::Transactional,
            priority: 200,
            summary: "Your code is 482913".into(),
            needs_reply: false,
            due: None,
            by: "rules".into(),
            at: 5,
            suspicious: false,
        };
        let r = TriageRow::from_triage(
            "personal",
            &ThreadId::new("t1"),
            &t,
            Some("A security code from Acme"),
        );
        assert_eq!(r.summary, "A security code from Acme");
        assert_eq!(r.priority, 100);
    }
}
