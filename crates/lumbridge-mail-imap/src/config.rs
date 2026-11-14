//! What the caller hands us: servers, credentials, identity and tuning. Nothing here is stored by this
//! crate; the daemon keeps the config and reads credentials from the keyring.

use lumbridge_mail_core::{AccountId, MailboxRole};
use std::fmt;
use std::time::Duration;

/// How the connection is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Security {
    /// TLS from the first byte (IMAP 993, SMTP 465). Preferred (RFC 8314).
    Tls,
    /// Plain TCP, then STARTTLS before anything else is said (IMAP 143, SMTP 587). If the server does not
    /// offer STARTTLS the connection fails; we never fall back to plaintext.
    StartTls,
    /// No TLS at all. Only allowed to a loopback address (tests, or a local bridge); refused otherwise.
    Plain,
}

/// One server endpoint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Server {
    pub host: String,
    pub port: u16,
    pub security: Security,
    /// Accept a self-signed certificate. Only honoured for loopback hosts (Proton Mail Bridge uses one);
    /// for any other host it is ignored and the certificate is verified as usual.
    #[serde(default)]
    pub accept_invalid_certs: bool,
}

impl Server {
    pub fn new(host: impl Into<String>, port: u16, security: Security) -> Self {
        Self { host: host.into(), port, security, accept_invalid_certs: false }
    }

    pub fn is_loopback(&self) -> bool {
        is_loopback_host(&self.host)
    }

    /// `accept_invalid_certs`, but only where it is allowed.
    pub(crate) fn skip_verify(&self) -> bool {
        self.accept_invalid_certs && self.is_loopback()
    }
}

pub(crate) fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// How to sign in. Passed in by the caller for each provider; this crate never stores, prints or logs it.
#[derive(Clone, PartialEq, Eq)]
pub enum Credentials {
    /// A password or an app password (IMAP `LOGIN`, SMTP `AUTH PLAIN` / `AUTH LOGIN`).
    Password { username: String, password: String },
    /// An OAuth 2 access token (IMAP `AUTHENTICATE XOAUTH2`, SMTP `AUTH XOAUTH2`). Refreshing it is the
    /// caller's job: hand the new one in with `ImapProvider::set_credentials`.
    OAuth2 { username: String, token: String },
}

impl Credentials {
    pub fn username(&self) -> &str {
        match self {
            Credentials::Password { username, .. } | Credentials::OAuth2 { username, .. } => username,
        }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Credentials::Password { username, .. } => {
                f.debug_struct("Password").field("username", username).field("password", &"<redacted>").finish()
            }
            Credentials::OAuth2 { username, .. } => {
                f.debug_struct("OAuth2").field("username", username).field("token", &"<redacted>").finish()
            }
        }
    }
}

/// Who we send as.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Identity {
    pub email: String,
    pub name: String,
}

impl Identity {
    pub fn domain(&self) -> &str {
        self.email.rsplit_once('@').map(|(_, d)| d).unwrap_or("localhost")
    }
}

/// Tuning. The defaults are right for most accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Most message headers fetched by one `sync` call; the rest comes with `Delta.more`.
    pub page_size: usize,
    /// Without CONDSTORE, flag changes are found by comparing the flags of the newest this-many messages
    /// per folder with what we saw last time. Older messages' flag changes are missed until a resync.
    pub flag_window: usize,
    /// How much of each message is fetched for the list row (headers, snippet, attachment hint).
    pub preview_bytes: usize,
    /// Folders with these roles are not synced or listed (they repeat mail kept elsewhere, e.g. Gmail's
    /// All Mail, Starred and Important). Actions may still use them (Archive on Gmail moves to All Mail).
    pub skip_roles: Vec<MailboxRole>,
    /// Does the SMTP server put sent mail in Sent itself? None = decide from the host (Gmail and
    /// Microsoft 365 do).
    pub server_saves_sent: Option<bool>,
    /// Network timeout for one command or one connection attempt.
    pub timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            page_size: 250,
            flag_window: 1000,
            preview_bytes: 16 * 1024,
            skip_roles: vec![MailboxRole::All, MailboxRole::Starred, MailboxRole::Important],
            server_saves_sent: None,
            timeout: Duration::from_secs(60),
        }
    }
}

/// Everything needed to build an `ImapProvider`.
#[derive(Debug, Clone)]
pub struct ImapAccount {
    pub id: AccountId,
    pub identity: Identity,
    pub imap: Server,
    /// None = this account can read but not send (`Action::Send` returns `Unsupported`).
    pub smtp: Option<Server>,
    pub credentials: Credentials,
    /// When SMTP signs in differently from IMAP; None = the IMAP credentials.
    pub smtp_credentials: Option<Credentials>,
    pub options: Options,
}

impl ImapAccount {
    pub fn new(id: AccountId, identity: Identity, imap: Server, smtp: Option<Server>, credentials: Credentials) -> Self {
        Self { id, identity, imap, smtp, credentials, smtp_credentials: None, options: Options::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_never_print() {
        let p = Credentials::Password { username: "ada@example.com".into(), password: "hunter2-secret".into() };
        let o = Credentials::OAuth2 { username: "ada@example.com".into(), token: "ya29.token-secret".into() };
        for c in [p, o] {
            let s = format!("{c:?}");
            assert!(!s.contains("secret"), "{s}");
            assert!(s.contains("ada@example.com"));
        }
    }

    #[test]
    fn loopback_only_for_insecure_things() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("[::1]"));
        assert!(!is_loopback_host("imap.example.com"));
        assert!(!is_loopback_host("10.0.0.1"));
        let mut s = Server::new("imap.example.com", 993, Security::Tls);
        s.accept_invalid_certs = true;
        assert!(!s.skip_verify());
        let mut l = Server::new("127.0.0.1", 1143, Security::StartTls);
        l.accept_invalid_certs = true;
        assert!(l.skip_verify());
    }
}
