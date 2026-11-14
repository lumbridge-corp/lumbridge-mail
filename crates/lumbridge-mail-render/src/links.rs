//! URLs in mail: which links may be followed, which images are trackers, and which links look like
//! phishing (the text shows one domain, the link goes to another; punycode homographs; IP hosts;
//! `https://bank.example@evil.example` userinfo tricks).

use serde::{Deserialize, Serialize};
use url::{Host, Url};

/// A link that deserves a second look before the user follows it. Shown as a hint, never acted on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkWarning {
    /// The link text as shown (trimmed, at most 200 chars).
    pub text: String,
    /// Where it really goes.
    pub href: String,
    pub kind: WarningKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WarningKind {
    /// The text reads as a URL or domain on `shown`, but the link goes to `actual`.
    TextShowsOtherDomain { shown: String, actual: String },
    /// The host is an internationalised (punycode) name; `unicode` is how it displays. Look-alike
    /// letters from other scripts are how homograph phishing works.
    Punycode { host: String, unicode: String },
    /// The host is a bare IP address.
    IpAddress { host: String },
    /// The URL has a `user@` part, which makes `https://bank.example@evil.example` go to evil.example.
    Userinfo { actual: String },
}

/// Normalise a link target, or None if it must not be followed. Allowed: `http`, `https`, `mailto`,
/// `tel`. Browsers ignore tabs and newlines inside URLs (`java\tscript:`), so we strip them before
/// looking at the scheme. Relative links have no base in mail (we drop `<base>`), so they go too.
pub fn safe_href(href: &str) -> Option<String> {
    let cleaned: String = href.chars().filter(|c| !c.is_whitespace() && !c.is_control()).collect();
    if cleaned.is_empty() {
        return None;
    }
    let cleaned = if let Some(rest) = cleaned.strip_prefix("//") { format!("https://{rest}") } else { cleaned };
    let lower = cleaned.to_ascii_lowercase();
    if lower.starts_with("mailto:") || lower.starts_with("tel:") {
        return (cleaned.len() <= 2048).then_some(cleaned);
    }
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return None;
    }
    let url = Url::parse(&cleaned).ok()?;
    url.host()?;
    let s = url.to_string();
    (s.len() <= 8192).then_some(s)
}

/// The host of an http(s) URL (punycode, lower case).
pub fn host_of(url: &str) -> Option<String> {
    let u = Url::parse(url.trim()).ok()?;
    u.host_str().map(|h| h.trim_end_matches('.').to_ascii_lowercase())
}

/// Second-level suffixes where the registrable domain has three labels (no public-suffix list here;
/// this is a hint, not a security boundary).
const TWO_PART_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "ac.uk", "gov.uk", "me.uk", "com.au", "net.au", "org.au", "co.nz", "co.jp", "ne.jp",
    "or.jp", "com.br", "com.mx", "co.in", "co.za", "com.sg", "com.hk", "com.tr", "com.cn", "co.kr", "com.ar",
];

/// `mail.news.example.co.uk` -> `example.co.uk`.
pub fn registrable(host: &str) -> String {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() <= 2 {
        return host;
    }
    let last2 = labels[labels.len() - 2..].join(".");
    let take = if TWO_PART_SUFFIXES.contains(&last2.as_str()) { 3 } else { 2 };
    labels[labels.len().saturating_sub(take)..].join(".")
}

/// Look at one link (its visible text and its safe href) and say if it looks deceptive.
pub fn link_warning(text: &str, href: &str) -> Option<LinkWarning> {
    let url = Url::parse(href).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let shown_text: String = text.trim().chars().take(200).collect();
    let warn = |kind| Some(LinkWarning { text: shown_text.clone(), href: href.to_owned(), kind });
    if !url.username().is_empty() || url.password().is_some() {
        return warn(WarningKind::Userinfo { actual: host });
    }
    if matches!(url.host(), Some(Host::Ipv4(_) | Host::Ipv6(_))) {
        return warn(WarningKind::IpAddress { host });
    }
    if host.split('.').any(|l| l.starts_with("xn--")) {
        let (unicode, _) = idna::domain_to_unicode(&host);
        return warn(WarningKind::Punycode { host, unicode });
    }
    if let Some(shown) = domain_in_text(text)
        && registrable(&shown) != registrable(&host) {
            return warn(WarningKind::TextShowsOtherDomain { shown, actual: host });
        }
    None
}

/// If the link text *is* a URL or a bare domain (optionally with a path), its host, in ASCII form.
/// "Visit example.com today" is not; "www.example.com/login" and "https://example.com" are.
fn domain_in_text(text: &str) -> Option<String> {
    let t = text.trim().trim_matches(|c: char| c == '<' || c == '>' || c == '"' || c == '\'' || c == '[' || c == ']');
    if t.is_empty() || t.chars().any(char::is_whitespace) || t.contains('@') {
        return None;
    }
    let lower = t.to_lowercase();
    let without_scheme = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://")).unwrap_or(&lower);
    let host_part = without_scheme.split(['/', '?', '#', ':']).next()?.trim_end_matches('.');
    let labels: Vec<&str> = host_part.split('.').collect();
    if labels.len() < 2 || labels.iter().any(|l| l.is_empty()) {
        return None;
    }
    let tld = labels.last()?;
    if tld.chars().count() < 2 || !tld.chars().all(char::is_alphabetic) {
        return None;
    }
    // A domain-looking token: labels of letters, digits and hyphens (any script).
    if !labels.iter().all(|l| l.chars().all(|c| c.is_alphanumeric() || c == '-')) {
        return None;
    }
    // Things like "file.pdf" or "v1.2" are not domains; require a scheme, "www.", or a common TLD shape.
    let has_scheme = lower.starts_with("http");
    if !has_scheme && !host_part.starts_with("www.") && !COMMON_TLDS.contains(tld) && tld.is_ascii() {
        return None;
    }
    let ascii = idna::domain_to_ascii(host_part).ok()?;
    Some(ascii)
}

const COMMON_TLDS: &[&str] = &[
    "com", "net", "org", "io", "co", "ai", "app", "dev", "uk", "us", "de", "fr", "es", "it", "nl", "ca", "au", "jp",
    "in", "br", "ru", "cn", "info", "biz", "me", "tv", "xyz", "online", "site", "shop", "store", "bank", "gov", "edu",
];

/// Hosts that exist to see whether you opened a mail. Images from them are dropped and counted as
/// tracking pixels, whatever their size.
const TRACKER_HOSTS: &[&str] = &[
    "mailtrack.io", "mailstat.us", "sidekickopen", "t.yesware.com", "app.yesware.com", "track.mixmax.com",
    "mixmax.com", "streak.com", "mailfoogae.appspot.com", "bananatag.com", "readnotify.com", "getnotify.com",
    "cirrusinsight.com", "boomerangapp.com", "superhuman.com", "pixel.", "open.", "opens.", "tracking.",
    "google-analytics.com", "doubleclick.net", "bat.bing.com", "facebook.com/tr", "mailchimp.com/track",
    "list-manage.com/track", "sendgrid.net/wf/open", "mandrillapp.com/track", "track.hubspot.com", "hs-analytics",
    "mktoresp.com", "exacttarget.com", "returnpath.net", "litmus.com", "emltrk.com",
];
const TRACKER_PATHS: &[&str] = &[
    "/open.gif", "/open.png", "/open?", "/track/open", "/wf/open", "/e/o/", "/pixel", "/beacon", "/o.gif",
    "/t.gif", "/1x1", "/spacer.gif", "/trk", "/imp?", "/impression", "/e2t/to/",
];

/// Is this image URL a known open tracker?
pub fn is_tracker_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://")) else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let path = &rest[host.len()..];
    TRACKER_HOSTS.iter().any(|t| {
        if t.contains('/') {
            rest.contains(t)
        } else if t.ends_with('.') {
            host.starts_with(t) || host.contains(&format!(".{t}"))
        } else {
            host == *t || host.ends_with(&format!(".{t}")) || (!t.contains('.') && host.contains(t))
        }
    }) || TRACKER_PATHS.iter().any(|p| path.starts_with(p) || path.contains(p))
}

const TRACKING_PARAMS: &[&str] = &["fbclid", "gclid", "dclid", "msclkid", "mc_eid", "mc_cid", "_hsenc", "_hsmi", "mkt_tok", "yclid", "igshid"];

/// Drop `utm_*` and click-id query parameters (for text given to the AI layer and to the user).
pub fn strip_tracking_params(url: &str) -> String {
    let Ok(mut u) = Url::parse(url) else { return url.to_owned() };
    if u.query().is_none() {
        return url.to_owned();
    }
    let kept: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| {
            let k = k.to_ascii_lowercase();
            !k.starts_with("utm_") && !TRACKING_PARAMS.contains(&k.as_str())
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        u.set_query(None);
    } else {
        u.query_pairs_mut().clear().extend_pairs(kept);
    }
    u.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_safe_schemes_pass() {
        assert_eq!(safe_href("https://example.com/a").as_deref(), Some("https://example.com/a"));
        assert_eq!(safe_href("//example.com/a").as_deref(), Some("https://example.com/a"));
        assert_eq!(safe_href("mailto:ada@example.com").as_deref(), Some("mailto:ada@example.com"));
        assert_eq!(safe_href("javascript:alert(1)"), None);
        assert_eq!(safe_href("java\tscript:alert(1)"), None);
        assert_eq!(safe_href(" JaVaScRiPt:alert(1)"), None);
        assert_eq!(safe_href("vbscript:x"), None);
        assert_eq!(safe_href("data:text/html,<script>"), None);
        assert_eq!(safe_href("file:///etc/passwd"), None);
        assert_eq!(safe_href("/relative/path"), None);
        assert_eq!(safe_href("#top"), None);
    }

    #[test]
    fn registrable_domains() {
        assert_eq!(registrable("mail.news.example.com"), "example.com");
        assert_eq!(registrable("shop.example.co.uk"), "example.co.uk");
        assert_eq!(registrable("example.com"), "example.com");
    }

    #[test]
    fn phishing_hints() {
        let w = link_warning("https://bank.example.com/login", "https://evil.example.net/x").unwrap();
        assert!(matches!(w.kind, WarningKind::TextShowsOtherDomain { ref shown, .. } if shown == "bank.example.com"));
        assert!(link_warning("www.example.com", "https://login.example.com/").is_none());
        assert!(link_warning("Click here", "https://evil.example.net/").is_none());
        assert!(link_warning("Visit example.com today", "https://evil.example.net/").is_none());
        let w = link_warning("Sign in", "https://xn--pple-43d.com/").unwrap();
        assert!(matches!(w.kind, WarningKind::Punycode { ref unicode, .. } if unicode == "аpple.com"));
        let w = link_warning("Sign in", "https://bank.example.com@evil.example.net/").unwrap();
        assert!(matches!(w.kind, WarningKind::Userinfo { .. }));
        assert!(matches!(link_warning("x", "http://192.0.2.7/").unwrap().kind, WarningKind::IpAddress { .. }));
        // Cyrillic "а" in the shown text: looks like apple.com, is not.
        let w = link_warning("аpple.com", "https://apple.com/").unwrap();
        assert!(matches!(w.kind, WarningKind::TextShowsOtherDomain { .. }));
    }

    #[test]
    fn trackers() {
        assert!(is_tracker_url("https://mailtrack.io/trace/mail/abc.png"));
        assert!(is_tracker_url("https://news.example.com/track/open?id=1"));
        assert!(is_tracker_url("https://pixel.example.com/x"));
        assert!(is_tracker_url("https://u123.ct.sendgrid.net/wf/open?upn=x"));
        assert!(!is_tracker_url("https://cdn.example.com/hero.jpg"));
        assert!(!is_tracker_url("https://example.com/opener.jpg"));
    }

    #[test]
    fn tracking_params_are_stripped() {
        assert_eq!(
            strip_tracking_params("https://example.com/a?utm_source=x&id=7&fbclid=y"),
            "https://example.com/a?id=7"
        );
        assert_eq!(strip_tracking_params("https://example.com/a?utm_source=x"), "https://example.com/a");
    }
}
