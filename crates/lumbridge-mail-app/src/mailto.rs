//! `mailto:` URLs (RFC 6068) to a composer prefill. They arrive from the desktop (the `.desktop` handler
//! passes `%u`, macOS sends an open-URL event), from a second instance, or from a link in a message.
//!
//! A mailto only ever fills in a draft: nothing is sent until a person presses Send. Unknown fields
//! (`attach=`, `in-reply-to=`, custom headers) are dropped; lengths are capped.

use crate::ui::composer::parse_addresses;
use lumbridge_mail_core::Address;

const MAX_SUBJECT: usize = 998;
const MAX_BODY: usize = 100_000;
const MAX_RECIPIENTS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mailto {
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    pub body: String,
}

/// Decode `%XX` escapes (as UTF-8, lossy). A `+` stays a `+` (RFC 6068: it is not a space in mailto).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(b) = hex {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn cap(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

fn add(list: &mut Vec<Address>, text: &str) {
    let (ok, _) = parse_addresses(text);
    for a in ok {
        if list.len() < MAX_RECIPIENTS && !list.contains(&a) {
            list.push(a);
        }
    }
}

/// Parse `mailto:...`. None if it is not a mailto URL at all.
pub fn parse(url: &str) -> Option<Mailto> {
    let url = url.trim();
    let rest = url.get(..7).filter(|p| p.eq_ignore_ascii_case("mailto:")).map(|_| &url[7..])?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut m = Mailto::default();
    add(&mut m.to, &percent_decode(path));
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = percent_decode(v);
        match percent_decode(k).to_ascii_lowercase().as_str() {
            "to" => add(&mut m.to, &v),
            "cc" => add(&mut m.cc, &v),
            "bcc" => add(&mut m.bcc, &v),
            "subject" if m.subject.is_empty() => m.subject = cap(v.replace(['\r', '\n'], " "), MAX_SUBJECT),
            "body" if m.body.is_empty() => m.body = cap(v.replace("\r\n", "\n"), MAX_BODY),
            _ => {}
        }
    }
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_and_full_mailtos() {
        let m = parse("mailto:maya.chen@postbox.example").unwrap();
        assert_eq!(m.to, vec![Address::new(None, "maya.chen@postbox.example")]);
        let m = parse("MAILTO:a@x.example,b@y.example?cc=c@z.example&bcc=d@z.example&subject=Hello%20there&body=Line%201%0D%0ALine%202").unwrap();
        assert_eq!(m.to.len(), 2);
        assert_eq!(m.cc[0].email, "c@z.example");
        assert_eq!(m.bcc[0].email, "d@z.example");
        assert_eq!(m.subject, "Hello there");
        assert_eq!(m.body, "Line 1\nLine 2");
    }

    #[test]
    fn encoded_names_utf8_and_junk() {
        let m = parse("mailto:%22Rivera%2C%20Sam%22%20%3Csam@inkwell.example%3E?subject=Caf%C3%A9&attach=/etc/passwd&x=%zz").unwrap();
        assert_eq!(m.to, vec![Address::new(Some("Rivera, Sam"), "sam@inkwell.example")]);
        assert_eq!(m.subject, "Café");
        assert!(parse("https://example.com").is_none());
        assert!(parse("mailto:").unwrap().to.is_empty());
        let m = parse("mailto:?to=a@x.example&subject=a%0Ab").unwrap();
        assert_eq!(m.to[0].email, "a@x.example");
        assert_eq!(m.subject, "a b", "no header injection through the subject");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%4"), "%4");
    }
}
