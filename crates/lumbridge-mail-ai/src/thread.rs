//! What the AI layer is handed: a thread as core types (metadata plus the body when it is cached).

use crate::text;
use lumbridge_mail_core::{AccountId, Address, Body, Error, MessageMeta, Result, ThreadId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailMessage {
    pub meta: MessageMeta,
    /// None when the body is not fetched yet: the snippet stands in.
    pub body: Option<Body>,
}

impl MailMessage {
    pub fn new(meta: MessageMeta, body: Option<Body>) -> Self {
        Self { meta, body }
    }

    /// A header from `Body::headers`, matched without case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.body.as_ref()?.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn sender(&self) -> Option<&Address> {
        self.meta.from.first()
    }

    /// Plain text: the text part, else the HTML through the local fallback, else the snippet. Cleaned of
    /// invisible characters; hidden HTML replaced by a marker. Quoted history still in.
    pub fn text(&self) -> String {
        if let Some(b) = &self.body {
            if let Some(t) = b.text.as_deref().filter(|t| !t.trim().is_empty()) {
                return text::clean(t);
            }
            if let Some(h) = b.html.as_deref().filter(|h| !h.trim().is_empty()) {
                return text::html_to_text(h);
            }
        }
        text::clean(&self.meta.snippet)
    }

    /// Everything the message carries as text, hidden parts and invisible characters included, for the
    /// injection detector only.
    pub(crate) fn raw_texts(&self) -> Vec<String> {
        let mut v = vec![self.meta.subject.clone(), self.meta.snippet.clone()];
        if let Some(a) = self.sender().and_then(|a| a.name.clone()) {
            v.push(a);
        }
        if let Some(b) = &self.body {
            v.extend(b.text.clone());
            v.extend(b.html.as_deref().map(text::html_all_text));
        }
        v
    }

    /// How many hidden HTML elements the body has.
    pub(crate) fn hidden_elements(&self) -> usize {
        self.body.as_ref().and_then(|b| b.html.as_deref()).map(|h| text::html_to_text_counted(h).1).unwrap_or(0)
    }
}

/// One thread of one account, oldest message first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailThread {
    messages: Vec<MailMessage>,
}

impl MailThread {
    /// At least one message, all from the same account and thread.
    pub fn new(mut messages: Vec<MailMessage>) -> Result<Self> {
        let first = messages.first().ok_or_else(|| Error::Invalid("a thread needs a message".into()))?;
        let (a, t) = (first.meta.account.clone(), first.meta.thread.clone());
        if messages.iter().any(|m| m.meta.account != a || m.meta.thread != t) {
            return Err(Error::Invalid("messages from different threads or accounts".into()));
        }
        messages.sort_by_key(|m| m.meta.date);
        Ok(Self { messages })
    }

    pub fn messages(&self) -> &[MailMessage] {
        &self.messages
    }
    pub fn account(&self) -> &AccountId {
        &self.messages[0].meta.account
    }
    pub fn id(&self) -> &ThreadId {
        &self.messages[0].meta.thread
    }
    pub fn subject(&self) -> &str {
        &self.messages[0].meta.subject
    }
    pub fn latest(&self) -> &MailMessage {
        self.messages.last().expect("non-empty")
    }

    /// The newest message not sent by `me` (the account's own address), if any.
    pub fn latest_from_others(&self, me: &str) -> Option<&MailMessage> {
        self.messages.iter().rev().find(|m| m.sender().is_none_or(|a| !a.email.eq_ignore_ascii_case(me)))
    }

    /// Why this thread looks like it tries to instruct an assistant (see `injection`). Looks at hidden
    /// text and invisible characters too.
    pub fn suspicious(&self) -> Vec<&'static str> {
        let mut texts = Vec::new();
        for m in &self.messages {
            texts.extend(m.raw_texts());
        }
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let mut why = crate::injection::reasons(&refs);
        if self.messages.iter().any(|m| m.hidden_elements() > 0) && !why.contains(&crate::injection::HIDDEN_HTML) {
            // Hidden text alone is common in marketing mail (preheaders); only a hit inside it counts.
            let hidden_hit = self.messages.iter().any(|m| {
                let all = m.body.as_ref().and_then(|b| b.html.as_deref()).map(text::html_all_text).unwrap_or_default();
                let shown = m.text();
                !crate::injection::reasons(&[&all]).is_empty() && crate::injection::reasons(&[&shown]).is_empty()
            });
            if hidden_hit {
                why.push(crate::injection::HIDDEN_HTML);
            }
        }
        why
    }
}
