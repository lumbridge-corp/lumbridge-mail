//! Outgoing mail: a `Draft` to RFC 5322 bytes (mail-builder), and SMTP submission (lettre).
//!
//! Message-IDs are derived from the draft's `local_id` (see `ids::outgoing_message_id`), so the draft on
//! the server, the copy in Sent and the mail that went out all carry the same one. That is what lets
//! `Send` check Sent before a retry.

use crate::config::{Credentials, Identity, Security, Server};
use lumbridge_mail_core::{Address, Draft, Error, Result};
use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address as BAddress;
use std::time::Duration;

/// No line breaks or control characters in a header value we write.
fn header_safe(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().to_owned()
}

fn valid_email(e: &str) -> bool {
    let e = e.trim();
    let Some((local, domain)) = e.rsplit_once('@') else { return false };
    !local.is_empty()
        && !domain.is_empty()
        && !e.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>' | ',' | ';' | '"'))
}

fn addr(a: &Address) -> Result<BAddress<'static>> {
    if !valid_email(&a.email) {
        return Err(Error::Invalid(format!("not an email address: {}", header_safe(&a.email))));
    }
    Ok(BAddress::new_address(a.name.as_deref().map(header_safe).filter(|n| !n.is_empty()), a.email.trim().to_owned()))
}

fn list(addrs: &[Address]) -> Result<Option<BAddress<'static>>> {
    if addrs.is_empty() {
        return Ok(None);
    }
    Ok(Some(BAddress::new_list(addrs.iter().map(addr).collect::<Result<Vec<_>>>()?)))
}

fn clean_id(s: &str) -> Option<String> {
    let s = header_safe(s);
    let s = s.trim_start_matches('<').trim_end_matches('>').trim();
    (!s.is_empty() && !s.contains(['<', '>', ' '])).then(|| s.to_owned())
}

/// Builds the message. `with_bcc`: keep the Bcc header (for the Drafts and Sent copies only; never for
/// the bytes that go over SMTP).
pub async fn build(draft: &Draft, from: &Identity, message_id: &str, with_bcc: bool, date: i64) -> Result<Vec<u8>> {
    let from_addr = Address::new(Some(&from.name), &from.email);
    let mut b = MessageBuilder::new()
        .from(addr(&from_addr)?)
        .message_id(message_id.to_owned())
        .subject(header_safe(&draft.subject))
        .date(date);
    if let Some(to) = list(&draft.to)? {
        b = b.to(to);
    }
    if let Some(cc) = list(&draft.cc)? {
        b = b.cc(cc);
    }
    if with_bcc && let Some(bcc) = list(&draft.bcc)? {
        b = b.bcc(bcc);
    }
    if let Some(irt) = draft.in_reply_to.as_deref().and_then(clean_id) {
        b = b.in_reply_to(irt);
    }
    let refs: Vec<String> = draft.references.iter().filter_map(|r| clean_id(r)).collect();
    if !refs.is_empty() {
        b = b.references(refs);
    }
    b = b.text_body(draft.text.clone());
    if let Some(html) = &draft.html {
        b = b.html_body(html.clone());
    }
    for a in &draft.attachments {
        let bytes = tokio::fs::read(&a.path)
            .await
            .map_err(|e| Error::Invalid(format!("cannot read attachment {}: {e}", header_safe(&a.filename))))?;
        let mime = if a.mime.contains('/') && !a.mime.chars().any(|c| c.is_control() || c.is_whitespace()) {
            a.mime.clone()
        } else {
            "application/octet-stream".into()
        };
        b = b.attachment(mime, header_safe(&a.filename), bytes);
    }
    b.write_to_vec().map_err(|e| Error::Other(format!("cannot build message: {e}")))
}

/// Every envelope recipient: To, Cc and Bcc, without repeats.
pub fn recipients(draft: &Draft) -> Result<Vec<lettre::Address>> {
    let mut out: Vec<lettre::Address> = Vec::new();
    for a in draft.to.iter().chain(&draft.cc).chain(&draft.bcc) {
        let parsed: lettre::Address = a
            .email
            .trim()
            .parse()
            .map_err(|_| Error::Invalid(format!("not an email address: {}", header_safe(&a.email))))?;
        if !out.contains(&parsed) {
            out.push(parsed);
        }
    }
    if out.is_empty() {
        return Err(Error::Invalid("a message needs at least one recipient".into()));
    }
    Ok(out)
}

/// Hands the bytes to the SMTP server.
pub async fn submit(
    server: &Server,
    creds: &Credentials,
    from: &Identity,
    to: Vec<lettre::Address>,
    bytes: &[u8],
    timeout: Duration,
) -> Result<()> {
    use lettre::transport::smtp::authentication::{Credentials as LCreds, Mechanism};
    use lettre::transport::smtp::client::{Tls, TlsParameters};
    use lettre::transport::smtp::extension::ClientId;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

    if server.security == Security::Plain && !server.is_loopback() {
        return Err(Error::Refused(format!("plaintext to {} is not allowed; use TLS or STARTTLS", server.host)));
    }
    let tls_params = || {
        TlsParameters::builder(server.host.clone())
            .dangerous_accept_invalid_certs(server.skip_verify())
            .build_rustls()
            .map_err(|e| Error::Other(format!("TLS setup for {}: {e}", server.host)))
    };
    let tls = match server.security {
        Security::Tls => Tls::Wrapper(tls_params()?),
        Security::StartTls => Tls::Required(tls_params()?),
        Security::Plain => Tls::None,
    };
    let (lcreds, mechanisms) = match creds {
        Credentials::Password { username, password } => {
            (LCreds::new(username.clone(), password.clone()), vec![Mechanism::Plain, Mechanism::Login])
        }
        Credentials::OAuth2 { username, token } => (LCreds::new(username.clone(), token.clone()), vec![Mechanism::Xoauth2]),
    };
    let transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(server.host.clone())
        .port(server.port)
        .tls(tls)
        // Never our machine's name: an address literal says nothing about us.
        .hello_name(ClientId::Ipv4(std::net::Ipv4Addr::LOCALHOST))
        .credentials(lcreds)
        .authentication(mechanisms)
        .timeout(Some(timeout))
        .build();
    let from_addr: lettre::Address =
        from.email.trim().parse().map_err(|_| Error::Invalid("the account's own address is not valid".into()))?;
    let envelope = lettre::address::Envelope::new(Some(from_addr), to)
        .map_err(|e| Error::Invalid(format!("bad envelope: {e}")))?;
    transport.send_raw(&envelope, bytes).await.map(|_| ()).map_err(|e| smtp_err(&server.host, e))
}

fn smtp_err(host: &str, e: lettre::transport::smtp::Error) -> Error {
    let code = e.status().map(|c| c.to_string());
    if let Some(c) = &code
        && (c.starts_with("535") || c.starts_with("534") || c.starts_with("530") || c.starts_with("454 4.7"))
    {
        return Error::Auth(format!("{host} refused the SMTP sign-in ({c})"));
    }
    if e.is_permanent() {
        return Error::Invalid(format!("{host} refused the message ({})", code.unwrap_or_default()));
    }
    if e.is_transient() || e.is_timeout() {
        return Error::Offline(format!("{host} asked us to try later ({})", code.unwrap_or_else(|| "timeout".into())));
    }
    if e.is_tls() {
        return Error::Other(format!("TLS with {host} failed"));
    }
    // Connection dropped, refused, reset: try again later (after the Sent check).
    Error::Offline(format!("SMTP with {host} failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumbridge_mail_core::DraftAttachment;

    fn ident() -> Identity {
        Identity { email: "ada@example.com".into(), name: "Ada L".into() }
    }

    #[tokio::test]
    async fn builds_threaded_multipart_mail() {
        let dir = std::env::temp_dir().join(format!("lbm-imap-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.txt");
        std::fs::write(&path, b"made-up notes").unwrap();
        let d = Draft {
            local_id: "d1".into(),
            to: vec![Address::new(Some("Bob"), "bob@example.org")],
            bcc: vec![Address::new(None, "secret@example.org")],
            subject: "Re: Plans\r\nBcc: injected@example.net".into(),
            text: "See you then.".into(),
            html: Some("<p>See you then.</p>".into()),
            in_reply_to: Some("<m1@example.org>".into()),
            references: vec!["m0@example.org".into(), "<m1@example.org>".into()],
            attachments: vec![DraftAttachment { path: path.display().to_string(), filename: "notes.txt".into(), mime: "text/plain".into() }],
            ..Default::default()
        };
        let wire = String::from_utf8(build(&d, &ident(), "lbm.1@example.com", false, 1_800_000_000).await.unwrap()).unwrap();
        assert!(wire.contains("Message-ID: <lbm.1@example.com>"), "{wire}");
        assert!(wire.contains("In-Reply-To: <m1@example.org>"));
        assert!(wire.contains("<m0@example.org>") && wire.contains("References:"));
        assert!(!wire.contains("secret@example.org"), "Bcc must not go over the wire");
        assert!(!wire.contains("\r\nBcc: injected"), "no header injection through the subject");
        assert!(wire.contains("multipart/"));
        assert!(wire.contains("notes.txt"));
        let copy = String::from_utf8(build(&d, &ident(), "lbm.1@example.com", true, 1_800_000_000).await.unwrap()).unwrap();
        assert!(copy.contains("secret@example.org"), "the Sent copy keeps Bcc");
        let parsed = crate::parse::body(wire.as_bytes());
        assert_eq!(parsed.text.as_deref().map(str::trim), Some("See you then."));
        assert_eq!(parsed.attachments.len(), 1);
        let rcpt = recipients(&d).unwrap();
        assert_eq!(rcpt.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn drafts_may_have_no_recipients_but_sends_may_not() {
        let d = Draft { local_id: "d2".into(), subject: "Idea".into(), text: "...".into(), ..Default::default() };
        assert!(build(&d, &ident(), "x@example.com", true, 0).await.is_ok());
        assert!(matches!(recipients(&d), Err(Error::Invalid(_))));
        let bad = Draft { to: vec![Address::new(None, "not an address")], ..d.clone() };
        assert!(matches!(build(&bad, &ident(), "x@example.com", false, 0).await, Err(Error::Invalid(_))));
        let missing = Draft {
            attachments: vec![DraftAttachment { path: "/nonexistent/x".into(), filename: "x".into(), mime: "a/b".into() }],
            ..d
        };
        assert!(matches!(build(&missing, &ident(), "x@example.com", false, 0).await, Err(Error::Invalid(_))));
    }
}
