//! What a connection that is not a person's may see of sensitive mail (CONTRIBUTING.md rule 3): one fixed
//! line ("A security code from Acme"), no snippet, no body. Agents are model-driven, so this is the same
//! rule as "never given to a model".

use lumbridge_mail_ai::text::who;
use lumbridge_mail_core::safety::sensitive_sender_kind;
use lumbridge_mail_core::{Address, Body, Category, MessageMeta, ThreadSummary};

fn line(kind: &str, from: Option<&Address>) -> String {
    format!("{kind} from {}", from.map(who).unwrap_or_else(|| "someone".into()))
}

/// The one line for a message, if it is sensitive (judged on sender, subject and snippet).
pub fn sensitive_line_for_meta(m: &MessageMeta) -> Option<String> {
    let from = m.from.first();
    let kind = sensitive_sender_kind(from.and_then(|a| a.name.as_deref()), from.map(|a| a.email.as_str()).unwrap_or(""), &m.subject, &m.snippet)?;
    Some(line(kind, from))
}

/// Also looks at the body text.
pub fn sensitive_line_with_body(m: &MessageMeta, body: &Body) -> Option<String> {
    if let Some(l) = sensitive_line_for_meta(m) {
        return Some(l);
    }
    let from = m.from.first();
    let text = body.text.clone().unwrap_or_else(|| {
        lumbridge_mail_render::plain_text(body, &lumbridge_mail_render::TextOptions { max_chars: 4000, ..Default::default() })
    });
    let kind = sensitive_sender_kind(from.and_then(|a| a.name.as_deref()), from.map(|a| a.email.as_str()).unwrap_or(""), &m.subject, &text)?;
    Some(line(kind, from))
}

/// The one line for a thread row, if it is sensitive (stored triage, or the rules on the row itself).
pub fn sensitive_line_for_summary(t: &ThreadSummary) -> Option<String> {
    if let Some(tr) = t.triage.as_ref().filter(|tr| tr.category == Category::Sensitive) {
        return Some(tr.summary.clone());
    }
    t.participants.iter().find_map(|p| {
        sensitive_sender_kind(p.name.as_deref(), &p.email, &t.subject, &t.snippet).map(|k| line(k, Some(p)))
    })
}

/// A thread row as an agent may see it.
pub fn summary_for_agent(mut t: ThreadSummary) -> ThreadSummary {
    if let Some(l) = sensitive_line_for_summary(&t) {
        t.subject = l.clone();
        t.snippet = String::new();
        if let Some(tr) = t.triage.as_mut() {
            tr.summary = l;
        }
    }
    t
}

/// A message's metadata as an agent may see it.
pub fn meta_for_agent(mut m: MessageMeta) -> MessageMeta {
    if let Some(l) = sensitive_line_for_meta(&m) {
        m.subject = l;
        m.snippet = String::new();
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumbridge_mail_core::*;

    fn meta(subject: &str, snippet: &str) -> MessageMeta {
        MessageMeta {
            id: MessageId::new("m"),
            account: AccountId::new("a"),
            thread: ThreadId::new("t"),
            mailboxes: vec![],
            from: vec![Address::new(Some("Acme"), "no-reply@acme.example")],
            to: vec![],
            cc: vec![],
            bcc: vec![],
            subject: subject.into(),
            snippet: snippet.into(),
            date: 0,
            flags: Flags::default(),
            has_attachments: false,
            size: 0,
            rfc_message_id: None,
            in_reply_to: None,
            references: vec![],
            partial: false,
        }
    }

    #[test]
    fn codes_become_one_line() {
        let m = meta_for_agent(meta("Your Acme verification code", "Your code is 482913"));
        assert_eq!(m.subject, "A security code from Acme");
        assert!(m.snippet.is_empty());
        let plain = meta_for_agent(meta("Lunch?", "Noon at the usual place"));
        assert_eq!(plain.subject, "Lunch?");
        let hidden = meta("Hello", "see below");
        let body = Body { text: Some("Your code is 482913".into()), ..Body::default() };
        assert!(sensitive_line_with_body(&hidden, &body).is_some());
    }
}
