//! A draft to Resend's `POST /emails` body, and the idempotency key that makes a retried send safe.
//!
//! Nothing here decides to send. `Action::Send` reaches the provider only from the daemon's queue, and
//! the daemon queues it only for a person (`safety::SEND_NEEDS_A_PERSON`).

use crate::convert::format_address;
use base64::Engine;
use lumbridge_mail_core::{Address, Draft, Error, Result};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// Resend's limit: 40 MB per email, measured after base64.
pub const MAX_ATTACHMENTS_BASE64: usize = 40 * 1000 * 1000;
/// Resend's limit on `to`.
pub const MAX_TO: usize = 50;

/// The same for every attempt to send one draft (`account` + `local_id`), so Resend answers a retry
/// with the first send's id instead of sending again (keys are kept 24 hours). 1..=256 characters.
pub fn idempotency_key(account: &str, local_id: &str) -> Result<String> {
    if local_id.trim().is_empty() {
        return Err(Error::Invalid("the draft has no local id".into()));
    }
    let mut h = Sha256::new();
    h.update(account.as_bytes());
    h.update([0]);
    h.update(local_id.as_bytes());
    let hex: String = h.finalize().iter().take(20).map(|b| format!("{b:02x}")).collect();
    Ok(format!("lbmail-{hex}"))
}

/// A Message-ID for In-Reply-To / References: no spaces, controls or angle brackets.
fn header_id(id: &str) -> Result<String> {
    let id = id.trim().trim_start_matches('<').trim_end_matches('>');
    if id.is_empty() || id.len() > 512 || id.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | ',')) {
        return Err(Error::Invalid(format!("not a Message-ID: {id:?}")));
    }
    Ok(format!("<{id}>"))
}

fn list(v: &[Address]) -> Result<Vec<Value>> {
    v.iter().map(|a| format_address(a).map(Value::String)).collect()
}

/// A file read for sending.
pub struct Loaded {
    pub filename: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// The request body. `from` is the account's address (and name); `reply_to` the account's setting.
/// In-Reply-To and References come from the draft (the provider fills them from the parent first).
pub fn payload(draft: &Draft, from: &Address, reply_to: Option<&str>, attachments: &[Loaded]) -> Result<Value> {
    if draft.to.is_empty() {
        return Err(Error::Invalid("Resend needs at least one To address".into()));
    }
    if draft.to.len() > MAX_TO {
        return Err(Error::Invalid(format!("Resend takes at most {MAX_TO} To addresses")));
    }
    let subject: String = draft.subject.chars().filter(|c| !c.is_control()).collect();
    let mut body = Map::new();
    body.insert("from".into(), Value::String(format_address(from)?));
    body.insert("to".into(), Value::Array(list(&draft.to)?));
    if !draft.cc.is_empty() {
        body.insert("cc".into(), Value::Array(list(&draft.cc)?));
    }
    if !draft.bcc.is_empty() {
        body.insert("bcc".into(), Value::Array(list(&draft.bcc)?));
    }
    body.insert("subject".into(), Value::String(subject));
    body.insert("text".into(), Value::String(draft.text.clone()));
    if let Some(h) = draft.html.as_ref().filter(|h| !h.trim().is_empty()) {
        body.insert("html".into(), Value::String(h.clone()));
    }
    if let Some(r) = reply_to.filter(|r| !r.trim().is_empty()) {
        body.insert("reply_to".into(), Value::String(format_address(&Address::new(None, r))?));
    }
    let mut headers = Map::new();
    if let Some(p) = &draft.in_reply_to {
        headers.insert("In-Reply-To".into(), Value::String(header_id(p)?));
    }
    let mut refs: Vec<String> = draft.references.iter().map(|r| header_id(r)).collect::<Result<_>>()?;
    if let Some(p) = &draft.in_reply_to {
        let p = header_id(p)?;
        if !refs.contains(&p) {
            refs.push(p);
        }
    }
    // Long chains: the first and the last 19, as mail clients do.
    if refs.len() > 20 {
        let tail = refs.split_off(refs.len() - 19);
        refs.truncate(1);
        refs.extend(tail);
    }
    if !refs.is_empty() {
        headers.insert("References".into(), Value::String(refs.join(" ")));
    }
    if !headers.is_empty() {
        body.insert("headers".into(), Value::Object(headers));
    }
    if !attachments.is_empty() {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut total = 0usize;
        let mut out = Vec::with_capacity(attachments.len());
        for a in attachments {
            let content = b64.encode(&a.bytes);
            total += content.len();
            if total > MAX_ATTACHMENTS_BASE64 {
                return Err(Error::Invalid("attachments are larger than Resend's 40 MB per email".into()));
            }
            let name: String = a.filename.chars().filter(|c| !c.is_control()).collect();
            out.push(json!({ "filename": name, "content": content, "content_type": a.mime }));
        }
        body.insert("attachments".into(), Value::Array(out));
    }
    Ok(Value::Object(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> Draft {
        Draft {
            local_id: "d1".into(),
            to: vec![Address::new(Some("Ada"), "ada@example.com")],
            subject: "Re: Quote".into(),
            text: "Yes.".into(),
            in_reply_to: Some("<q1@example.com>".into()),
            references: vec!["root@example.com".into()],
            ..Draft::default()
        }
    }

    #[test]
    fn keys_are_stable_and_per_draft() {
        let a = idempotency_key("shop", "d1").unwrap();
        assert_eq!(a, idempotency_key("shop", "d1").unwrap());
        assert_ne!(a, idempotency_key("shop", "d2").unwrap());
        assert_ne!(a, idempotency_key("other", "d1").unwrap());
        assert!(a.len() <= 256 && a.starts_with("lbmail-"));
        assert!(idempotency_key("shop", " ").is_err());
    }

    #[test]
    fn the_payload_threads_and_quotes() {
        let from = Address::new(Some("Shop"), "hello@shop.example");
        let files = [Loaded { filename: "a.txt".into(), mime: "text/plain".into(), bytes: b"hi".to_vec() }];
        let p = payload(&draft(), &from, Some("support@shop.example"), &files).unwrap();
        assert_eq!(p["from"], "\"Shop\" <hello@shop.example>");
        assert_eq!(p["to"][0], "\"Ada\" <ada@example.com>");
        assert_eq!(p["headers"]["In-Reply-To"], "<q1@example.com>");
        assert_eq!(p["headers"]["References"], "<root@example.com> <q1@example.com>");
        assert_eq!(p["reply_to"], "support@shop.example");
        assert_eq!(p["attachments"][0]["content"], "aGk=");
        assert!(p.get("cc").is_none() && p.get("html").is_none());
        let mut bad = draft();
        bad.in_reply_to = Some("a b@x".into());
        assert!(payload(&bad, &from, None, &[]).is_err());
        let mut none = draft();
        none.to.clear();
        assert!(payload(&none, &from, None, &[]).is_err());
    }
}
