//! One Resend account: who it is, which addresses it shows, the API key (handed in by the caller, never
//! stored or printed) and the limits it keeps to.

use lumbridge_mail_core::AccountId;
use std::time::Duration;

/// Resend's API.
pub const DEFAULT_BASE_URL: &str = "https://api.resend.com";

/// An API key. `Debug` and `Display` never show it; [`ApiKey::expose`] is the only way to read it and is
/// used for exactly one thing: the `Authorization` header.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into().trim().to_owned())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// Which addresses belong to this account: an exact address (`hello@shop.example`) or a whole domain
/// (`@shop.example`; a bare `shop.example` means the same). Matched without case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressPattern {
    Exact(String),
    Domain(String),
}

impl AddressPattern {
    pub fn parse(s: &str) -> Option<AddressPattern> {
        let s = s.trim().to_ascii_lowercase();
        if s.is_empty() {
            return None;
        }
        match s.split_once('@') {
            Some(("", d)) if !d.is_empty() => Some(AddressPattern::Domain(d.to_owned())),
            Some((l, d)) if !l.is_empty() && !d.is_empty() => Some(AddressPattern::Exact(s)),
            Some(_) => None,
            None => Some(AddressPattern::Domain(s)),
        }
    }

    pub fn matches(&self, email: &str) -> bool {
        let email = email.trim().to_ascii_lowercase();
        match self {
            AddressPattern::Exact(e) => *e == email,
            AddressPattern::Domain(d) => email.rsplit_once('@').is_some_and(|(_, x)| x == d),
        }
    }

    pub fn domain(&self) -> &str {
        match self {
            AddressPattern::Exact(e) => e.rsplit_once('@').map(|(_, d)| d).unwrap_or(""),
            AddressPattern::Domain(d) => d,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResendConfig {
    pub account: AccountId,
    /// The From address of mail sent from this account (and the default for `addresses`).
    pub email: String,
    /// The name mail is sent as.
    pub name: Option<String>,
    /// `https://api.resend.com`; tests point it at the fake server (plain http only to loopback).
    pub base_url: String,
    pub key: ApiKey,
    /// The addresses this account shows (Inbox: mail to them; Sent: mail from them) and may send from.
    /// Resend's lists are team-wide (every domain, every website's mail), so this is the filter. Empty =
    /// the domain of `email`.
    pub addresses: Vec<AddressPattern>,
    /// List received mail (`GET /emails/receiving`). Off for a domain without receiving.
    pub receiving: bool,
    /// Added as `reply_to` on every send (Resend's field). None = no Reply-To.
    pub reply_to: Option<String>,
    /// Requests per second this account makes at most. Resend's limit is per team (all keys, every
    /// website that sends through it), so stay well under it.
    pub requests_per_second: f64,
    /// Items per list page (Resend allows 1..=100).
    pub page_size: u32,
    /// A full sync reads at most this many pages per source (sent, received), newest first.
    pub max_full_pages: u32,
    /// An incremental sync follows at most this many pages of new mail per source before it gives up
    /// on the oldest of a burst (they come back with the next full sync).
    pub max_new_pages: u32,
    /// New received mail is fetched one by one for its snippet and threading headers, at most this many
    /// per sync call (the rest arrive without a snippet; the body is fetched when opened).
    pub max_details_per_sync: usize,
    /// Scheduled emails that are due are re-read (to see them sent), at most this many per sync call.
    pub max_scheduled_checks: usize,
    /// `wait_for_change` sleeps at most this long (Resend has no push without a public webhook).
    pub poll_interval: Duration,
    /// Attachment and raw-message downloads larger than this are refused.
    pub max_download_bytes: u64,
    pub timeout: Duration,
    /// How many times one request is tried again after a 429, and the longest single wait.
    pub max_rate_retries: u32,
    pub max_retry_wait: Duration,
}

impl ResendConfig {
    pub fn new(account: AccountId, email: impl Into<String>, key: ApiKey) -> Self {
        Self {
            account,
            email: email.into().trim().to_ascii_lowercase(),
            name: None,
            base_url: DEFAULT_BASE_URL.into(),
            key,
            addresses: Vec::new(),
            receiving: true,
            reply_to: None,
            requests_per_second: 4.0,
            page_size: 100,
            max_full_pages: 10,
            max_new_pages: 10,
            max_details_per_sync: 50,
            max_scheduled_checks: 20,
            poll_interval: Duration::from_secs(60),
            max_download_bytes: 50 << 20,
            timeout: Duration::from_secs(30),
            max_rate_retries: 4,
            max_retry_wait: Duration::from_secs(60),
        }
    }

    /// `addresses`, or the domain of `email` when none are set.
    pub fn patterns(&self) -> Vec<AddressPattern> {
        if !self.addresses.is_empty() {
            return self.addresses.clone();
        }
        AddressPattern::parse(&format!("@{}", self.email.rsplit_once('@').map(|(_, d)| d).unwrap_or(""))).into_iter().collect()
    }

    pub fn is_mine(&self, email: &str) -> bool {
        self.patterns().iter().any(|p| p.matches(email))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_never_print() {
        let k = ApiKey::new(" re_made_up_123 ");
        assert_eq!(format!("{k:?}"), "ApiKey(<redacted>)");
        assert_eq!(k.expose(), "re_made_up_123");
        let c = ResendConfig::new(AccountId::new("a"), "x@y.example", k);
        assert!(!format!("{c:?}").contains("re_made_up"));
    }

    #[test]
    fn patterns() {
        let c = ResendConfig::new(AccountId::new("a"), "Hello@Shop.example", ApiKey::new("k"));
        assert!(c.is_mine("orders@shop.example") && !c.is_mine("x@other.example"));
        let mut c = c;
        c.addresses = ["support@shop.example", "news.example", "@b.example"].iter().filter_map(|s| AddressPattern::parse(s)).collect();
        assert!(c.is_mine("SUPPORT@shop.example") && !c.is_mine("orders@shop.example"));
        assert!(c.is_mine("a@news.example") && c.is_mine("z@b.example"));
        assert_eq!(AddressPattern::parse("@"), None);
        assert_eq!(AddressPattern::parse("support@shop.example").unwrap().domain(), "shop.example");
    }
}
