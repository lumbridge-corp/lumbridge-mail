//! Outgoing mail as RFC 5322 bytes, for gog's `--raw-file -` (gog 0.41 and later). Built with
//! `mail-builder` (Stalwart Labs, Apache-2.0 OR MIT): multipart/alternative for text + HTML,
//! multipart/mixed when there are attachments, RFC 2047 for non-ASCII headers, threading headers.
//!
//! The Message-ID is derived from the draft's `local_id`, so the same draft always has the same id.
//! That is what makes send idempotent: before sending, the provider asks Gmail whether a sent
//! message with this id exists already.

use lumbridge_mail_core::{Address, Draft, Error, Result};
use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address as MbAddress;
use mail_builder::headers::message_id::MessageId as MbMessageId;
use sha2::{Digest, Sha256};

/// An attachment already read from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedAttachment {
    pub filename: String,
    pub mime: String,
    pub data: Vec<u8>,
}

/// Everything the builder needs besides the draft.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub from: Address,
    /// Without angle brackets.
    pub message_id: String,
    /// Unix seconds.
    pub date: i64,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
}

/// The stable Message-ID for a draft: `lbm.<32 hex>@<sender's domain>`. Hashing keeps our internal
/// ids out of headers other people see.
pub fn message_id_for(local_id: &str, from_email: &str) -> Result<String> {
    if local_id.trim().is_empty() {
        return Err(Error::Invalid("draft has no local_id".into()));
    }
    let digest = Sha256::digest(local_id.as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    let domain = from_email.rsplit_once('@').map(|(_, d)| d).filter(|d| !d.is_empty()).unwrap_or("lumbridge.invalid");
    let domain: String = domain.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')).collect();
    Ok(format!("lbm.{hex}@{domain}"))
}

/// No CR/LF (header injection) and no surrounding space in anything that becomes a header.
fn header_text(s: &str) -> String {
    s.split(['\r', '\n']).map(str::trim).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ")
}

fn mb_list(list: &[Address]) -> Option<MbAddress<'static>> {
    let items: Vec<MbAddress<'static>> = list
        .iter()
        .filter(|a| a.email.contains('@'))
        .map(|a| MbAddress::new_address(a.name.as_deref().map(header_text), header_text(&a.email)))
        .collect();
    (!items.is_empty()).then(|| MbAddress::new_list(items))
}

fn clean_id(id: &str) -> String {
    header_text(id).trim_matches(['<', '>']).to_owned()
}

/// Build the RFC 5322 message. Bcc is kept in the header block: Gmail reads it to deliver and strips it.
pub fn build(draft: &Draft, env: &Envelope, attachments: &[LoadedAttachment]) -> Result<Vec<u8>> {
    let mut b = MessageBuilder::new()
        .message_id(MbMessageId::new(clean_id(&env.message_id)))
        .date(env.date)
        .from(MbAddress::new_address(env.from.name.as_deref().map(header_text), header_text(&env.from.email)))
        .subject(header_text(&draft.subject));
    if let Some(to) = mb_list(&draft.to) {
        b = b.to(to);
    }
    if let Some(cc) = mb_list(&draft.cc) {
        b = b.cc(cc);
    }
    if let Some(bcc) = mb_list(&draft.bcc) {
        b = b.bcc(bcc);
    }
    if let Some(irt) = env.in_reply_to.as_deref().map(clean_id).filter(|s| !s.is_empty()) {
        b = b.in_reply_to(MbMessageId::new(irt));
    }
    let refs: Vec<String> = env.references.iter().map(|r| clean_id(r)).filter(|r| !r.is_empty()).collect();
    if !refs.is_empty() {
        b = b.references(MbMessageId::new_list(refs.into_iter()));
    }
    b = b.text_body(draft.text.clone());
    if let Some(html) = draft.html.as_ref().filter(|h| !h.trim().is_empty()) {
        b = b.html_body(html.clone());
    }
    for a in attachments {
        let mime = if a.mime.contains('/') { header_text(&a.mime) } else { "application/octet-stream".into() };
        b = b.attachment(mime, header_text(&a.filename), a.data.clone());
    }
    b.write_to_vec().map_err(|e| Error::Other(format!("building MIME: {e}")))
}

/// The References for a reply: the parent's References plus the parent's own id, no repeats, capped
/// at the last 20 (long threads otherwise grow headers without bound).
pub fn reply_references(parent_refs: &[String], parent_id: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in parent_refs.iter().map(String::as_str).chain(parent_id) {
        let r = clean_id(r);
        if !r.is_empty() && !out.contains(&r) {
            out.push(r);
        }
    }
    let n = out.len();
    if n > 20 {
        // Keep the thread root and the most recent 19.
        let mut kept = vec![out[0].clone()];
        kept.extend(out[n - 19..].iter().cloned());
        return kept;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> Draft {
        Draft {
            local_id: "d-1".into(),
            to: vec![Address::new(Some("Ada Lovelace"), "ada@example.com")],
            cc: vec![Address::new(None, "bob@example.org")],
            bcc: vec![Address::new(None, "carol@example.net")],
            subject: "Re: Café plans\r\nBcc: evil@example.com".into(),
            text: "See you at noon.".into(),
            html: Some("<p>See you at <b>noon</b>.</p>".into()),
            ..Default::default()
        }
    }

    fn env() -> Envelope {
        Envelope {
            from: Address::new(Some("Test Sender"), "sender@example.com"),
            message_id: message_id_for("d-1", "sender@example.com").unwrap(),
            date: 1_700_000_000,
            in_reply_to: Some("<parent@example.com>".into()),
            references: vec!["root@example.com".into(), "parent@example.com".into()],
        }
    }

    #[test]
    fn message_ids_are_stable_and_opaque() {
        let a = message_id_for("d-1", "sender@example.com").unwrap();
        assert_eq!(a, message_id_for("d-1", "sender@example.com").unwrap());
        assert_ne!(a, message_id_for("d-2", "sender@example.com").unwrap());
        assert!(a.starts_with("lbm.") && a.ends_with("@example.com") && !a.contains("d-1"), "{a}");
        assert!(message_id_for(" ", "x@y").is_err());
    }

    #[test]
    fn builds_threaded_multipart_mail_with_attachments() {
        let att = LoadedAttachment { filename: "pixel.png".into(), mime: "image/png".into(), data: vec![0x89, b'P', b'N', b'G', 0, 0xff, 0xfe] };
        let raw = String::from_utf8(build(&draft(), &env(), &[att]).unwrap()).unwrap();
        let headers = raw.split("\r\n\r\n").next().unwrap();
        assert!(headers.contains(&format!("Message-ID: <{}>", env().message_id)), "{raw}");
        assert!(headers.contains("In-Reply-To: <parent@example.com>"), "{headers}");
        assert!(headers.contains("References: <root@example.com> <parent@example.com>"), "{headers}");
        assert!(headers.contains("To: \"Ada Lovelace\" <ada@example.com>"), "{headers}");
        assert!(headers.contains("Bcc: <carol@example.net>"), "{headers}");
        assert!(headers.contains("multipart/mixed"), "{headers}");
        // The CR/LF in the subject did not start a new header.
        assert!(!raw.contains("\r\nBcc: evil@example.com"), "{raw}");
        assert!(headers.contains("Subject: =?utf-8?"), "non-ASCII subject is encoded: {headers}");
        assert!(raw.contains("multipart/alternative"), "{raw}");
        assert!(raw.contains("text/html"));
        assert!(raw.contains("Content-Disposition: attachment; filename=\"pixel.png\""), "{raw}");
        assert!(raw.contains("Content-Transfer-Encoding: base64"), "binary attachment is base64: {raw}");
    }

    #[test]
    fn plain_text_only_mail_is_single_part() {
        let mut d = draft();
        d.html = None;
        d.subject = "Lunch".into();
        let e = Envelope { in_reply_to: None, references: vec![], ..env() };
        let raw = String::from_utf8(build(&d, &e, &[]).unwrap()).unwrap();
        assert!(!raw.contains("multipart"), "{raw}");
        assert!(raw.contains("Subject: Lunch\r\n"));
        assert!(!raw.contains("In-Reply-To"));
    }

    #[test]
    fn references_grow_without_repeats_and_stay_bounded() {
        assert_eq!(reply_references(&["a@x".into(), "b@x".into()], Some("<b@x>")), vec!["a@x", "b@x"]);
        assert_eq!(reply_references(&[], Some("p@x")), vec!["p@x"]);
        let long: Vec<String> = (0..40).map(|i| format!("m{i}@x")).collect();
        let r = reply_references(&long, Some("last@x"));
        assert_eq!(r.len(), 20);
        assert_eq!(r[0], "m0@x");
        assert_eq!(r[19], "last@x");
    }
}
