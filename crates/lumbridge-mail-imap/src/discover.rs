//! Server settings for an address. `builtin` is a table of common providers and never touches the network.
//! `lookup` asks DNS (SRV, RFC 6186 / RFC 8314) and the domain's own autoconfig file (Thunderbird
//! format); the caller invokes it explicitly, when the user adds an account. We never ask a third party
//! (no ISPDB), and we never suggest plaintext to a remote host.
//!
//! The provider table and the autoconfig tag scanner are adapted from Overlook
//! (`overlook-provider/src/autoconfig.rs`, MIT, Copyright (c) 2026 Last Refuge).

use crate::config::{Security, Server};
use lumbridge_mail_core::{Error, Result};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Where a suggestion came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Builtin,
    Srv,
    Autoconfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub provider: Option<String>,
    pub imap: Server,
    pub smtp: Server,
    /// The user name to sign in with (often the address itself).
    pub username: String,
    /// The provider only accepts OAuth (or app passwords) for third-party clients.
    pub oauth_only: bool,
    /// The SMTP server files sent mail in Sent itself (don't APPEND a copy).
    pub server_saves_sent: bool,
    pub source: Source,
    /// Something the user should know (app passwords, a bridge to install).
    pub note: Option<&'static str>,
}

fn domain_of(email: &str) -> String {
    email.trim().rsplit_once('@').map(|(_, d)| d).unwrap_or("").trim().trim_end_matches('.').to_ascii_lowercase()
}

struct Row {
    domains: &'static [&'static str],
    provider: &'static str,
    imap: (&'static str, u16, Security),
    smtp: (&'static str, u16, Security),
    oauth_only: bool,
    saves_sent: bool,
    self_signed: bool,
    note: Option<&'static str>,
}

const TABLE: &[Row] = &[
    Row { domains: &["gmail.com", "googlemail.com"], provider: "Gmail", imap: ("imap.gmail.com", 993, Security::Tls), smtp: ("smtp.gmail.com", 465, Security::Tls), oauth_only: false, saves_sent: true, self_signed: false, note: Some("Needs an app password (or an OAuth token); prefer the gog provider for Gmail.") },
    Row { domains: &["outlook.com", "hotmail.com", "live.com", "msn.com", "hotmail.co.uk", "outlook.de"], provider: "Outlook.com", imap: ("outlook.office365.com", 993, Security::Tls), smtp: ("smtp-mail.outlook.com", 587, Security::StartTls), oauth_only: true, saves_sent: true, self_signed: false, note: Some("Microsoft accepts only OAuth for IMAP and SMTP.") },
    Row { domains: &["icloud.com", "me.com", "mac.com"], provider: "iCloud Mail", imap: ("imap.mail.me.com", 993, Security::Tls), smtp: ("smtp.mail.me.com", 587, Security::StartTls), oauth_only: false, saves_sent: false, self_signed: false, note: Some("Needs an app-specific password.") },
    Row { domains: &["fastmail.com", "fastmail.fm", "messagingengine.com", "fastmail.us", "fastmail.uk", "fastmail.de"], provider: "Fastmail", imap: ("imap.fastmail.com", 993, Security::Tls), smtp: ("smtp.fastmail.com", 465, Security::Tls), oauth_only: false, saves_sent: false, self_signed: false, note: Some("Needs an app password.") },
    Row { domains: &["yahoo.com", "ymail.com", "rocketmail.com", "yahoo.co.uk", "yahoo.ca", "yahoo.de", "yahoo.fr"], provider: "Yahoo Mail", imap: ("imap.mail.yahoo.com", 993, Security::Tls), smtp: ("smtp.mail.yahoo.com", 465, Security::Tls), oauth_only: false, saves_sent: false, self_signed: false, note: Some("Needs an app password.") },
    Row { domains: &["aol.com"], provider: "AOL Mail", imap: ("imap.aol.com", 993, Security::Tls), smtp: ("smtp.aol.com", 465, Security::Tls), oauth_only: false, saves_sent: false, self_signed: false, note: Some("Needs an app password.") },
    Row { domains: &["proton.me", "protonmail.com", "protonmail.ch", "pm.me"], provider: "Proton Mail Bridge", imap: ("127.0.0.1", 1143, Security::StartTls), smtp: ("127.0.0.1", 1025, Security::StartTls), oauth_only: false, saves_sent: true, self_signed: true, note: Some("Runs through Proton Mail Bridge on this machine; use the Bridge's password, not your Proton one.") },
    Row { domains: &["zoho.com", "zohomail.com", "zoho.eu", "zohomail.eu"], provider: "Zoho Mail", imap: ("imap.zoho.com", 993, Security::Tls), smtp: ("smtp.zoho.com", 465, Security::Tls), oauth_only: false, saves_sent: false, self_signed: false, note: None },
    Row { domains: &["posteo.de", "posteo.net", "posteo.eu", "posteo.org", "posteo.ch", "posteo.at"], provider: "Posteo", imap: ("posteo.de", 993, Security::Tls), smtp: ("posteo.de", 465, Security::Tls), oauth_only: false, saves_sent: false, self_signed: false, note: None },
    Row { domains: &["mailbox.org"], provider: "mailbox.org", imap: ("imap.mailbox.org", 993, Security::Tls), smtp: ("smtp.mailbox.org", 465, Security::Tls), oauth_only: false, saves_sent: false, self_signed: false, note: None },
    Row { domains: &["gmx.de", "gmx.net", "gmx.at", "gmx.ch", "gmx.com", "gmx.us"], provider: "GMX", imap: ("imap.gmx.net", 993, Security::Tls), smtp: ("mail.gmx.net", 465, Security::Tls), oauth_only: false, saves_sent: false, self_signed: false, note: None },
    Row { domains: &["web.de"], provider: "WEB.DE", imap: ("imap.web.de", 993, Security::Tls), smtp: ("smtp.web.de", 465, Security::Tls), oauth_only: false, saves_sent: false, self_signed: false, note: None },
    Row { domains: &["yandex.com", "yandex.ru", "ya.ru"], provider: "Yandex Mail", imap: ("imap.yandex.com", 993, Security::Tls), smtp: ("smtp.yandex.com", 465, Security::Tls), oauth_only: false, saves_sent: false, self_signed: false, note: Some("Needs an app password.") },
    Row { domains: &["hey.com"], provider: "HEY", imap: ("imap.hey.com", 993, Security::Tls), smtp: ("smtp.hey.com", 465, Security::Tls), oauth_only: false, saves_sent: false, self_signed: false, note: None },
];

/// The built-in table. No network.
pub fn builtin(email: &str) -> Option<Discovered> {
    let domain = domain_of(email);
    let row = TABLE.iter().find(|r| r.domains.contains(&domain.as_str()))?;
    let server = |(h, p, s): (&str, u16, Security)| Server { host: h.into(), port: p, security: s, accept_invalid_certs: row.self_signed };
    Some(Discovered {
        provider: Some(row.provider.to_owned()),
        imap: server(row.imap),
        smtp: server(row.smtp),
        username: email.trim().to_owned(),
        oauth_only: row.oauth_only,
        server_saves_sent: row.saves_sent,
        source: Source::Builtin,
        note: row.note,
    })
}

/// One SRV answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrvRecord {
    pub priority: u16,
    pub weight: u16,
    pub port: u16,
    pub target: String,
}

/// The best usable target of an SRV answer set: lowest priority, then highest weight. A target of `.`
/// means "no such service here" (RFC 2782).
pub fn best_srv(records: &[SrvRecord]) -> Option<(String, u16)> {
    let mut v: Vec<&SrvRecord> = records.iter().filter(|r| r.port != 0).collect();
    if v.iter().any(|r| r.target.trim_end_matches('.').is_empty()) && v.len() == 1 {
        return None;
    }
    v.retain(|r| !r.target.trim_end_matches('.').is_empty());
    v.sort_by(|a, b| a.priority.cmp(&b.priority).then(b.weight.cmp(&a.weight)));
    v.first().map(|r| (r.target.trim_end_matches('.').to_ascii_lowercase(), r.port))
}

/// Combines the four RFC 6186/8314 lookups (implicit TLS preferred over STARTTLS).
pub fn from_srv(
    email: &str,
    imaps: &[SrvRecord],
    imap: &[SrvRecord],
    submissions: &[SrvRecord],
    submission: &[SrvRecord],
) -> Option<Discovered> {
    let incoming = best_srv(imaps)
        .map(|(h, p)| Server::new(h, p, Security::Tls))
        .or_else(|| best_srv(imap).map(|(h, p)| Server::new(h, p, Security::StartTls)))?;
    let outgoing = best_srv(submissions)
        .map(|(h, p)| Server::new(h, p, Security::Tls))
        .or_else(|| best_srv(submission).map(|(h, p)| Server::new(h, p, Security::StartTls)))?;
    Some(Discovered {
        provider: None,
        imap: incoming,
        smtp: outgoing,
        username: email.trim().to_owned(),
        oauth_only: false,
        server_saves_sent: false,
        source: Source::Srv,
        note: None,
    })
}

/// Reads a Thunderbird autoconfig file: the first `incomingServer type="imap"` and the first
/// `outgoingServer type="smtp"`. Plaintext servers are never returned.
pub fn parse_autoconfig(xml: &str, email: &str) -> Option<Discovered> {
    let provider = text_in(xml, "displayName").filter(|s| !s.is_empty());
    let incoming = elements(xml, "incomingServer").into_iter().find(|(attrs, _)| attrs.contains("type=\"imap\""))?.1;
    let outgoing = elements(xml, "outgoingServer").into_iter().find(|(attrs, _)| attrs.contains("type=\"smtp\""))?.1;
    let local = email.trim().split('@').next().unwrap_or("").to_owned();
    let endpoint = |block: &str, default_port: u16| -> Option<(Server, String, bool)> {
        let host = text_in(block, "hostname")?.to_ascii_lowercase();
        if host.is_empty() || host.contains(['%', ' ', '/']) {
            return None;
        }
        let port = text_in(block, "port").and_then(|p| p.parse().ok()).unwrap_or(default_port);
        let security = match text_in(block, "socketType").map(|s| s.to_ascii_uppercase()).as_deref() {
            Some("SSL") | Some("TLS") => Security::Tls,
            Some("STARTTLS") => Security::StartTls,
            Some("PLAIN") => Security::Plain,
            _ => Security::Tls,
        };
        let username = match text_in(block, "username").as_deref() {
            Some("%EMAILLOCALPART%") => local.clone(),
            Some(u) if !u.is_empty() && !u.contains('%') => u.to_owned(),
            _ => email.trim().to_owned(),
        };
        let oauth = elements(block, "authentication").iter().any(|(_, a)| a.trim().eq_ignore_ascii_case("OAuth2"))
            && !elements(block, "authentication").iter().any(|(_, a)| a.trim().starts_with("password"));
        let server = Server::new(host, port, security);
        if server.security == Security::Plain && !server.is_loopback() {
            return None;
        }
        Some((server, username, oauth))
    };
    let (imap, username, oauth_in) = endpoint(&incoming, 993)?;
    let (smtp, _, oauth_out) = endpoint(&outgoing, 465)?;
    Some(Discovered {
        provider,
        imap,
        smtp,
        username,
        oauth_only: oauth_in || oauth_out,
        server_saves_sent: false,
        source: Source::Autoconfig,
        note: None,
    })
}

/// `(attributes, inner text)` of every `<tag ...>...</tag>`, in document order.
fn elements(xml: &str, tag: &str) -> Vec<(String, String)> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        // `<tagX` is a different tag.
        if !after.starts_with(['>', ' ', '\t', '\r', '\n', '/']) {
            rest = after;
            continue;
        }
        let Some(gt) = after.find('>') else { break };
        let attrs = after[..gt].to_owned();
        let inner_start = gt + 1;
        let Some(end) = after[inner_start..].find(&close) else { break };
        out.push((attrs, after[inner_start..inner_start + end].to_owned()));
        rest = &after[inner_start + end + close.len()..];
    }
    out
}

fn text_in(xml: &str, tag: &str) -> Option<String> {
    elements(xml, tag).into_iter().next().map(|(_, inner)| unescape(inner.trim()))
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn valid_domain(d: &str) -> bool {
    !d.is_empty()
        && d.len() <= 253
        && d.contains('.')
        && d.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

/// Asks the network: SRV records first, then `https://autoconfig.<domain>/mail/config-v1.1.xml` and
/// `https://<domain>/.well-known/autoconfig/mail/config-v1.1.xml`. Only call this when the user asks
/// (adding an account); it talks to DNS and to the mail domain's own web server, nobody else.
pub async fn lookup(email: &str, timeout: Duration) -> Result<Option<Discovered>> {
    let domain = domain_of(email);
    if !valid_domain(&domain) {
        return Err(Error::Invalid(format!("no domain in {email}")));
    }
    if let Some(d) = srv_lookup(email, &domain, timeout).await {
        return Ok(Some(d));
    }
    for (host, path) in [
        (format!("autoconfig.{domain}"), "/mail/config-v1.1.xml".to_owned()),
        (domain.clone(), "/.well-known/autoconfig/mail/config-v1.1.xml".to_owned()),
    ] {
        if let Ok(Ok(Some(xml))) = tokio::time::timeout(timeout, https_get(&host, &path)).await
            && let Some(d) = parse_autoconfig(&xml, email)
        {
            return Ok(Some(d));
        }
    }
    Ok(None)
}

/// `builtin`, else `lookup`.
pub async fn discover(email: &str, timeout: Duration) -> Result<Option<Discovered>> {
    match builtin(email) {
        Some(d) => Ok(Some(d)),
        None => lookup(email, timeout).await,
    }
}

async fn srv_lookup(email: &str, domain: &str, timeout: Duration) -> Option<Discovered> {
    let resolver = hickory_resolver::TokioResolver::builder_tokio().ok()?.build().ok()?;
    let query = |service: &'static str| {
        let resolver = resolver.clone();
        let name = format!("{service}.{domain}.");
        async move {
            let Ok(Ok(lookup)) = tokio::time::timeout(timeout, resolver.srv_lookup(name)).await else {
                return Vec::new();
            };
            lookup
                .answers()
                .iter()
                .filter_map(|r| match &r.data {
                    hickory_resolver::proto::rr::RData::SRV(srv) => Some(SrvRecord {
                        priority: srv.priority,
                        weight: srv.weight,
                        port: srv.port,
                        target: srv.target.to_ascii(),
                    }),
                    _ => None,
                })
                .collect::<Vec<_>>()
        }
    };
    let (a, b, c, d) = tokio::join!(query("_imaps._tcp"), query("_imap._tcp"), query("_submissions._tcp"), query("_submission._tcp"));
    from_srv(email, &a, &b, &c, &d)
}

/// A minimal HTTPS GET (HTTP/1.1, no redirects, at most 256 KiB). Enough for an autoconfig file without
/// pulling in an HTTP client.
async fn https_get(host: &str, path: &str) -> Result<Option<String>> {
    let server = Server::new(host, 443, Security::Tls);
    let tcp = crate::conn::tcp(&server).await?;
    let mut tls = crate::conn::tls_wrap(tcp, &server).await?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: mail-client\r\nAccept: application/xml, text/xml\r\nConnection: close\r\n\r\n"
    );
    tls.write_all(req.as_bytes()).await.map_err(|e| Error::Offline(e.to_string()))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match tls.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 256 * 1024 {
                    return Ok(None);
                }
            }
            // Servers often close TLS without close_notify; what we have is what we got.
            Err(_) if !buf.is_empty() => break,
            Err(e) => return Err(Error::Offline(e.to_string())),
        }
    }
    Ok(parse_http(&buf))
}

/// Status 200 body of an HTTP/1.1 response, de-chunked.
fn parse_http(buf: &[u8]) -> Option<String> {
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&buf[..end]).to_string();
    let status = head.lines().next()?.split_whitespace().nth(1)?;
    if status != "200" {
        return None;
    }
    let body = &buf[end + 4..];
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    if !chunked {
        return Some(String::from_utf8_lossy(body).into_owned());
    }
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let line_end = rest.windows(2).position(|w| w == b"\r\n")?;
        let size_str = String::from_utf8_lossy(&rest[..line_end]);
        let size = usize::from_str_radix(size_str.split(';').next()?.trim(), 16).ok()?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            break;
        }
        out.extend_from_slice(rest.get(..size)?);
        rest = rest.get(size + 2..)?;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_table() {
        let g = builtin("Someone@GMAIL.com").unwrap();
        assert_eq!((g.imap.host.as_str(), g.imap.port, g.imap.security), ("imap.gmail.com", 993, Security::Tls));
        assert!(g.server_saves_sent);
        let p = builtin("x@proton.me").unwrap();
        assert!(p.imap.is_loopback() && p.imap.skip_verify(), "the Bridge's self-signed cert is accepted on loopback");
        let o = builtin("x@outlook.com").unwrap();
        assert!(o.oauth_only);
        assert_eq!(o.smtp.security, Security::StartTls);
        assert!(builtin("x@example.com").is_none());
        // No remote server in the table accepts invalid certs.
        for row in TABLE {
            if row.self_signed {
                assert!(crate::config::is_loopback_host(row.imap.0));
            }
        }
    }

    #[test]
    fn srv_prefers_tls_and_priority() {
        let rec = |p, w, port, t: &str| SrvRecord { priority: p, weight: w, port, target: t.into() };
        let imaps = [rec(10, 0, 993, "imap2.example.com."), rec(5, 0, 993, "imap1.example.com.")];
        let sub = [rec(0, 0, 587, "smtp.example.com.")];
        let d = from_srv("ada@example.com", &imaps, &[], &[], &sub).unwrap();
        assert_eq!((d.imap.host.as_str(), d.imap.security), ("imap1.example.com", Security::Tls));
        assert_eq!((d.smtp.host.as_str(), d.smtp.port, d.smtp.security), ("smtp.example.com", 587, Security::StartTls));
        // "." = service not offered.
        assert!(best_srv(&[rec(0, 0, 993, ".")]).is_none());
        assert!(from_srv("a@example.com", &[], &[], &[], &sub).is_none());
    }

    #[test]
    fn autoconfig_xml() {
        let xml = r#"<?xml version="1.0"?>
<clientConfig version="1.1">
  <emailProvider id="example.com">
    <displayName>Example &amp; Co</displayName>
    <incomingServer type="pop3"><hostname>pop.example.com</hostname><port>995</port><socketType>SSL</socketType></incomingServer>
    <incomingServer type="imap">
      <hostname>imap.example.com</hostname><port>143</port><socketType>STARTTLS</socketType>
      <authentication>password-cleartext</authentication><username>%EMAILLOCALPART%</username>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.example.com</hostname><port>465</port><socketType>SSL</socketType><username>%EMAILADDRESS%</username>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#;
        let d = parse_autoconfig(xml, "ada@example.com").unwrap();
        assert_eq!(d.provider.as_deref(), Some("Example & Co"));
        assert_eq!((d.imap.host.as_str(), d.imap.port, d.imap.security), ("imap.example.com", 143, Security::StartTls));
        assert_eq!(d.username, "ada");
        assert_eq!((d.smtp.host.as_str(), d.smtp.port, d.smtp.security), ("smtp.example.com", 465, Security::Tls));
        assert!(!d.oauth_only);
        let plain = xml.replace("<socketType>STARTTLS</socketType>", "<socketType>plain</socketType>");
        assert!(parse_autoconfig(&plain, "ada@example.com").is_none());
        assert!(parse_autoconfig("<clientConfig/>", "a@b.c").is_none());
    }

    #[test]
    fn http_bodies() {
        assert_eq!(parse_http(b"HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\n\r\n<x/>").as_deref(), Some("<x/>"));
        assert_eq!(
            parse_http(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n<x/\r\n1\r\n>\r\n0\r\n\r\n").as_deref(),
            Some("<x/>")
        );
        assert!(parse_http(b"HTTP/1.1 404 Not Found\r\n\r\nnope").is_none());
        assert!(valid_domain("example.com") && !valid_domain("localhost") && !valid_domain("a..b"));
    }
}
