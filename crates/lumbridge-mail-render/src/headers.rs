//! Headers worth acting on: `List-Unsubscribe` (+ RFC 8058 one-click), `List-Id`, `Reply-To`, and a
//! summary of `Authentication-Results` (SPF / DKIM / DMARC).
//!
//! Only the *topmost* `Authentication-Results` header is believed by default: it was added by the
//! receiving server (Gmail, our IMAP host). Lower ones arrived with the message and can be forged by the
//! sender. Pass `trusted` ids to [`auth_summary`] to pick the header by its authserv-id instead.

use crate::charset::decode_words;
use crate::links;
use lumbridge_mail_core::{Address, Body};
use serde::{Deserialize, Serialize};

/// Everything this module extracts, in one go.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HeaderInfo {
    pub unsubscribe: Option<Unsubscribe>,
    /// `List-Id` without angle brackets (`news.example.com`).
    pub list_id: Option<String>,
    pub reply_to: Vec<Address>,
    /// Reply-To goes to a different domain than From (a common phishing and invoice-fraud tell).
    pub reply_to_differs: bool,
    pub auth: Option<AuthSummary>,
}

/// How to unsubscribe. The app offers one-click when `one_click`, else the web page, else the mailto
/// (which, being a send, needs the user to press Send like any other mail).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Unsubscribe {
    /// An `https:` URL (an `http:` one only if no https was offered).
    pub web: Option<String>,
    /// RFC 8058: POST `List-Unsubscribe=One-Click` to `web`, no page to visit.
    pub one_click: bool,
    pub mailto: Option<Mailto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Mailto {
    pub to: String,
    pub subject: Option<String>,
    pub body: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
    SoftFail,
    Neutral,
    None,
    TempError,
    PermError,
    Policy,
    Other,
}

impl Verdict {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "pass" => Self::Pass,
            "fail" | "hardfail" => Self::Fail,
            "softfail" => Self::SoftFail,
            "neutral" => Self::Neutral,
            "none" => Self::None,
            "temperror" => Self::TempError,
            "permerror" => Self::PermError,
            "policy" => Self::Policy,
            _ => Self::Other,
        }
    }
}

/// What the reader shows next to the sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    /// DMARC passed, or DKIM passed for the From domain.
    Verified,
    /// Nothing failed, nothing proved the sender either.
    Unverified,
    /// DMARC failed, or SPF failed with no aligned DKIM pass: the From address is probably forged.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DkimResult {
    pub result: Verdict,
    /// `header.d`
    pub domain: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthSummary {
    pub authserv_id: String,
    pub spf: Option<Verdict>,
    /// `smtp.mailfrom` domain.
    pub spf_domain: Option<String>,
    pub dkim: Vec<DkimResult>,
    pub dmarc: Option<Verdict>,
    /// `header.from` as DMARC saw it.
    pub dmarc_from: Option<String>,
    pub trust: Trust,
}

/// All values of a header, in order (top first), case-insensitively.
pub fn header_values<'a>(headers: &'a [(String, String)], name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    headers.iter().filter(move |(k, _)| k.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// Read everything at once. `from` is the message's sender (from `MessageMeta`), used for the
/// Reply-To and DKIM alignment checks.
pub fn header_info(body: &Body, from: Option<&Address>) -> HeaderInfo {
    let h = &body.headers;
    let unsubscribe = header_values(h, "List-Unsubscribe").next().and_then(|v| {
        let post = header_values(h, "List-Unsubscribe-Post").next();
        list_unsubscribe(v, post)
    });
    let list_id = header_values(h, "List-Id").next().and_then(|v| {
        let v = decode_words(v);
        let id = match (v.rfind('<'), v.rfind('>')) {
            (Some(a), Some(b)) if b > a => v[a + 1..b].trim().to_owned(),
            _ => v.trim().to_owned(),
        };
        (!id.is_empty()).then_some(id)
    });
    let reply_to: Vec<Address> = header_values(h, "Reply-To").flat_map(parse_addresses).collect();
    let from_domain = from.map(|f| links::registrable(f.domain()));
    let reply_to_differs = match &from_domain {
        Some(fd) if !fd.is_empty() => reply_to.iter().any(|a| links::registrable(a.domain()) != *fd),
        _ => false,
    };
    let auth = auth_summary(h, from.map(Address::domain), &[]);
    HeaderInfo { unsubscribe, list_id, reply_to, reply_to_differs, auth }
}

/// Parse `List-Unsubscribe: <mailto:...>, <https://...>` (+ `List-Unsubscribe-Post`).
pub fn list_unsubscribe(value: &str, post: Option<&str>) -> Option<Unsubscribe> {
    let mut u = Unsubscribe::default();
    let mut http = None;
    for part in value.split(',') {
        let p = part.trim();
        let uri = p.strip_prefix('<').and_then(|r| r.split_once('>')).map(|(u, _)| u).unwrap_or(p).trim();
        let uri: String = uri.chars().filter(|c| !c.is_whitespace()).collect();
        let lower = uri.to_ascii_lowercase();
        if lower.starts_with("mailto:") && u.mailto.is_none() {
            u.mailto = parse_mailto(&uri);
        } else if lower.starts_with("https://") && u.web.is_none() {
            u.web = links::safe_href(&uri);
        } else if lower.starts_with("http://") && http.is_none() {
            http = links::safe_href(&uri);
        }
    }
    let https = u.web.is_some();
    if u.web.is_none() {
        u.web = http;
    }
    u.one_click = https && post.is_some_and(|p| p.replace(' ', "").eq_ignore_ascii_case("List-Unsubscribe=One-Click"));
    (u.web.is_some() || u.mailto.is_some()).then_some(u)
}

fn parse_mailto(uri: &str) -> Option<Mailto> {
    let rest = &uri["mailto:".len()..];
    let (to, query) = rest.split_once('?').unwrap_or((rest, ""));
    let to = pct_decode(to);
    if !to.contains('@') || to.len() > 320 {
        return None;
    }
    let mut m = Mailto { to, ..Mailto::default() };
    for kv in query.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            match k.to_ascii_lowercase().as_str() {
                "subject" => m.subject = Some(pct_decode(v)),
                "body" => m.body = Some(pct_decode(v)),
                _ => {}
            }
        }
    }
    Some(m)
}

fn pct_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={}", s.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// Parse an address list (`"Doe, Jane" <jane@example.com>, bo@example.com, Team: a@x.y;`).
/// Encoded words in names are decoded. Entries without an `@` are skipped.
pub fn parse_addresses(value: &str) -> Vec<Address> {
    let value = decode_words(value);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote = false;
    let mut angle = false;
    let mut paren = 0u32;
    let mut items = Vec::new();
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quote => escaped = true,
            '"' if paren == 0 => {
                quote = !quote;
                cur.push(ch);
            }
            '(' if !quote => paren += 1,
            ')' if !quote && paren > 0 => paren -= 1,
            _ if paren > 0 => {}
            '<' if !quote => {
                angle = true;
                cur.push(ch);
            }
            '>' if !quote => {
                angle = false;
                cur.push(ch);
            }
            ',' | ';' if !quote && !angle => items.push(std::mem::take(&mut cur)),
            ':' if !quote && !angle => cur.clear(), // a group name
            _ => cur.push(ch),
        }
    }
    items.push(cur);
    for item in items {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (name, email) = match (item.rfind('<'), item.rfind('>')) {
            (Some(a), Some(b)) if b > a => (Some(item[..a].trim().trim_matches('"').trim()), item[a + 1..b].trim()),
            _ => (None, item.trim_matches('"')),
        };
        if !email.contains('@') || email.contains(char::is_whitespace) {
            continue;
        }
        out.push(Address::new(name, email));
    }
    out
}

/// Summarise SPF/DKIM/DMARC. Uses the first header whose authserv-id is in `trusted`, or the topmost
/// one when `trusted` is empty. `from_domain` enables the DKIM alignment check.
pub fn auth_summary(headers: &[(String, String)], from_domain: Option<&str>, trusted: &[&str]) -> Option<AuthSummary> {
    let mut values = header_values(headers, "Authentication-Results");
    let value = if trusted.is_empty() {
        values.next()?
    } else {
        values.find(|v| {
            let id = strip_comments(v).split(';').next().unwrap_or("").split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            trusted.iter().any(|t| t.eq_ignore_ascii_case(&id))
        })?
    };
    let value = strip_comments(value);
    let mut parts = value.split(';');
    let authserv_id = parts.next().unwrap_or("").split_whitespace().next().unwrap_or("").to_owned();
    let mut s = AuthSummary { authserv_id, spf: None, spf_domain: None, dkim: Vec::new(), dmarc: None, dmarc_from: None, trust: Trust::Unverified };
    for res in parts {
        let mut tokens = res.split_whitespace();
        let Some(first) = tokens.next() else { continue };
        let Some((method, result)) = first.split_once('=') else { continue };
        let verdict = Verdict::parse(result);
        let props: Vec<(String, String)> = tokens
            .filter_map(|t| t.split_once('=').map(|(k, v)| (k.to_ascii_lowercase(), v.trim_matches('"').to_ascii_lowercase())))
            .collect();
        let prop = |k: &str| props.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.clone());
        match method.to_ascii_lowercase().as_str() {
            "spf" if s.spf.is_none() => {
                s.spf = Some(verdict);
                s.spf_domain = prop("smtp.mailfrom").map(|m| m.rsplit('@').next().unwrap_or(&m).to_owned());
            }
            "dkim" => s.dkim.push(DkimResult { result: verdict, domain: prop("header.d").or_else(|| prop("header.i").map(|i| i.trim_start_matches('@').rsplit('@').next().unwrap_or("").to_owned())) }),
            "dmarc" if s.dmarc.is_none() => {
                s.dmarc = Some(verdict);
                s.dmarc_from = prop("header.from");
            }
            _ => {}
        }
    }
    let from = from_domain.map(links::registrable).or_else(|| s.dmarc_from.as_deref().map(links::registrable));
    let aligned_dkim = s.dkim.iter().any(|d| {
        d.result == Verdict::Pass && match (&from, &d.domain) {
            (Some(f), Some(dd)) => links::registrable(dd) == *f,
            _ => false,
        }
    });
    s.trust = if s.dmarc == Some(Verdict::Fail) {
        Trust::Failed
    } else if s.dmarc == Some(Verdict::Pass) || aligned_dkim {
        Trust::Verified
    } else if matches!(s.spf, Some(Verdict::Fail)) || s.dkim.iter().any(|d| d.result == Verdict::Fail) && !s.dkim.iter().any(|d| d.result == Verdict::Pass) {
        Trust::Failed
    } else {
        Trust::Unverified
    };
    Some(s)
}

fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0u32;
    let mut quote = false;
    for ch in s.chars() {
        match ch {
            '"' if depth == 0 => {
                quote = !quote;
                out.push(ch);
            }
            '(' if !quote => depth += 1,
            ')' if !quote && depth > 0 => depth -= 1,
            _ if depth > 0 => {}
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn unsubscribe_one_click() {
        let u = list_unsubscribe(
            "<mailto:unsub@news.example.com?subject=unsubscribe%20me>, <https://news.example.com/u/abc>",
            Some("List-Unsubscribe=One-Click"),
        )
        .unwrap();
        assert_eq!(u.web.as_deref(), Some("https://news.example.com/u/abc"));
        assert!(u.one_click);
        let m = u.mailto.unwrap();
        assert_eq!(m.to, "unsub@news.example.com");
        assert_eq!(m.subject.as_deref(), Some("unsubscribe me"));
        let u = list_unsubscribe("<http://news.example.com/u>", Some("List-Unsubscribe=One-Click")).unwrap();
        assert!(!u.one_click, "one-click needs https");
        assert!(list_unsubscribe("<javascript:alert(1)>", None).is_none());
    }

    #[test]
    fn addresses() {
        let a = parse_addresses("\"Doe, Jane\" <Jane@Example.com>, bo@example.org (Bo), Team: c@example.net;");
        assert_eq!(a.len(), 3);
        assert_eq!(a[0].name.as_deref(), Some("Doe, Jane"));
        assert_eq!(a[0].email, "jane@example.com");
        assert_eq!(a[1].email, "bo@example.org");
        let a = parse_addresses("=?UTF-8?B?w4lsaXNl?= <elise@example.com>");
        assert_eq!(a[0].name.as_deref(), Some("Élise"));
    }

    #[test]
    fn auth_results() {
        let headers = h(&[
            ("Authentication-Results", "mx.example.net; spf=pass (sender ok) smtp.mailfrom=bounce@mail.shop.example.com; dkim=pass header.i=@shop.example.com header.s=s1; dmarc=pass (p=REJECT) header.from=shop.example.com"),
            ("Authentication-Results", "forged.example; dmarc=fail"),
        ]);
        let s = auth_summary(&headers, Some("shop.example.com"), &[]).unwrap();
        assert_eq!(s.authserv_id, "mx.example.net");
        assert_eq!(s.spf, Some(Verdict::Pass));
        assert_eq!(s.spf_domain.as_deref(), Some("mail.shop.example.com"));
        assert_eq!(s.dkim[0].domain.as_deref(), Some("shop.example.com"));
        assert_eq!(s.trust, Trust::Verified);
        let s = auth_summary(&h(&[("Authentication-Results", "mx.example.net; spf=softfail; dkim=none; dmarc=fail header.from=bank.example")]), None, &[]).unwrap();
        assert_eq!(s.trust, Trust::Failed);
        let s = auth_summary(&h(&[("Authentication-Results", "mx.example.net; dkim=pass header.d=other.example")]), Some("shop.example.com"), &[]).unwrap();
        assert_eq!(s.trust, Trust::Unverified, "DKIM for another domain proves nothing about From");
    }

    #[test]
    fn reply_to_mismatch() {
        let body = Body {
            headers: h(&[("Reply-To", "Accounts <payments@elsewhere.example>"), ("List-Id", "Shop News <news.shop.example.com>")]),
            ..Body::default()
        };
        let info = header_info(&body, Some(&Address::new(Some("Shop"), "billing@shop.example.com")));
        assert!(info.reply_to_differs);
        assert_eq!(info.list_id.as_deref(), Some("news.shop.example.com"));
    }
}
