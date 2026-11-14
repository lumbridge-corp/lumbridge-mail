//! Header text from Gmail: address lists (RFC 5322) and encoded words (RFC 2047).
//!
//! Gmail's API usually hands headers back already decoded, but not always (and not for every sender's
//! broken mail), so encoded words are decoded when they appear. Parsing is forgiving: mail is hostile
//! input, and a bad header must give a best-effort value, never a panic or an error.

use base64::Engine;
use lumbridge_mail_core::Address;

/// Decode `=?charset?B|Q?text?=` words. Whitespace between two encoded words is dropped (RFC 2047 6.2).
pub fn decode_words(s: &str) -> String {
    if !s.contains("=?") {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    let mut pending_ws = String::new();
    let mut last_was_word = false;
    while !rest.is_empty() {
        if let Some((decoded, used)) = rest.strip_prefix("=?").and_then(|r| decode_one(r).map(|(d, n)| (d, n + 2))) {
            if !last_was_word {
                out.push_str(&pending_ws);
            }
            pending_ws.clear();
            out.push_str(&decoded);
            rest = &rest[used..];
            last_was_word = true;
            continue;
        }
        let c = rest.chars().next().unwrap_or(' ');
        if c.is_whitespace() {
            pending_ws.push(c);
        } else {
            out.push_str(&pending_ws);
            pending_ws.clear();
            out.push(c);
            last_was_word = false;
        }
        rest = &rest[c.len_utf8()..];
    }
    out.push_str(&pending_ws);
    out
}

/// `charset?E?text?=` (after the leading `=?`). Returns the text and the bytes consumed.
fn decode_one(r: &str) -> Option<(String, usize)> {
    let q1 = r.find('?')?;
    let charset = &r[..q1];
    let enc = r.get(q1 + 1..q1 + 2)?;
    if r.get(q1 + 2..q1 + 3)? != "?" {
        return None;
    }
    let text_start = q1 + 3;
    let end = r[text_start..].find("?=")? + text_start;
    let text = &r[text_start..end];
    if text.contains(char::is_whitespace) {
        return None;
    }
    let bytes = match enc {
        "B" | "b" => base64::engine::general_purpose::STANDARD
            .decode(text.trim_end_matches('='))
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(text.trim_end_matches('=')))
            .ok()?,
        "Q" | "q" => decode_q(text),
        _ => return None,
    };
    // RFC 2231 language suffix: `utf-8*en`.
    let charset = charset.split('*').next().unwrap_or(charset);
    Some((crate::convert::decode_charset(&bytes, Some(charset)), end + 2))
}

fn decode_q(text: &str) -> Vec<u8> {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'_' => out.push(b' '),
            b'=' if i + 2 < b.len() => {
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push(h << 4 | l);
                        i += 2;
                    }
                    _ => out.push(b'='),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    out
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// Parse an address-list header (`From`, `To`, `Cc`, ...). Groups are flattened; entries without an
/// `@` are dropped (they are not addresses we can show or reply to).
pub fn parse_list(value: &str) -> Vec<Address> {
    let mut out = Vec::new();
    for item in split_top_level(value) {
        if let Some(a) = parse_one(&item) {
            out.push(a);
        }
    }
    out
}

/// Split on commas (and group `:`/`;`) that are outside quotes, comments and angle brackets.
fn split_top_level(s: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut cur = String::new();
    let (mut quote, mut angle, mut comment, mut escape) = (false, false, 0u32, false);
    for c in s.chars() {
        if escape {
            cur.push(c);
            escape = false;
            continue;
        }
        match c {
            '\\' if quote || comment > 0 => {
                cur.push(c);
                escape = true;
            }
            '"' if comment == 0 => {
                quote = !quote;
                cur.push(c);
            }
            '(' if !quote => {
                comment += 1;
                cur.push(c);
            }
            ')' if !quote && comment > 0 => {
                comment -= 1;
                cur.push(c);
            }
            '<' if !quote && comment == 0 => {
                angle = true;
                cur.push(c);
            }
            '>' if !quote && comment == 0 => {
                angle = false;
                cur.push(c);
            }
            ',' | ';' if !quote && !angle && comment == 0 => items.push(std::mem::take(&mut cur)),
            // `Group name: a@b, c@d;` — the display name before the colon is not an address.
            ':' if !quote && !angle && comment == 0 && !cur.contains('@') => cur.clear(),
            _ => cur.push(c),
        }
    }
    items.push(cur);
    items.into_iter().map(|i| i.trim().to_owned()).filter(|i| !i.is_empty()).collect()
}

fn parse_one(item: &str) -> Option<Address> {
    let (name, email) = match (item.rfind('<'), item.rfind('>')) {
        (Some(l), Some(r)) if l < r => (item[..l].trim(), item[l + 1..r].trim()),
        _ => {
            // Bare address, maybe with a trailing comment: `ada@example.com (Ada Lovelace)`.
            let (addr, comment) = match item.find('(') {
                Some(p) => (&item[..p], item[p..].trim_matches(|c| c == '(' || c == ')' || c == ' ')),
                None => (item, ""),
            };
            (comment, addr.trim())
        }
    };
    let email = email.trim_start_matches("mailto:");
    if !email.contains('@') || email.contains(char::is_whitespace) {
        return None;
    }
    let name = unquote(name);
    let name = decode_words(&name);
    let name = name.trim();
    let name = if name.eq_ignore_ascii_case(email) { "" } else { name };
    Some(Address::new(Some(name), email))
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    // Drop comments from display names: `Ada (work) <a@b>`.
    let mut out = String::new();
    let (mut quote, mut escape, mut comment) = (false, false, 0u32);
    for c in s.chars() {
        if escape {
            out.push(c);
            escape = false;
            continue;
        }
        match c {
            '\\' if quote => escape = true,
            '"' if comment == 0 => quote = !quote,
            '(' if !quote => comment += 1,
            ')' if !quote && comment > 0 => comment -= 1,
            _ if comment > 0 => {}
            _ => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Message ids from `Message-ID`, `In-Reply-To` or `References`, without angle brackets.
pub fn parse_ids(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = value;
    while let Some(l) = rest.find('<') {
        let Some(r) = rest[l..].find('>') else { break };
        let id = rest[l + 1..l + r].trim();
        if !id.is_empty() && !id.contains(char::is_whitespace) {
            out.push(id.to_owned());
        }
        rest = &rest[l + r + 1..];
    }
    if out.is_empty() {
        // Some senders omit the brackets.
        out.extend(value.split_whitespace().filter(|w| w.contains('@')).map(|w| w.trim_matches(['<', '>']).to_owned()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_lists_with_quotes_commas_and_groups() {
        let v = parse_list(r#""Lovelace, Ada" <Ada@Example.com>, bob@example.org (Bob B), Team: c@example.net, "D \"Q\"" <d@example.net>;, nobody"#);
        let got: Vec<(Option<&str>, &str)> = v.iter().map(|a| (a.name.as_deref(), a.email.as_str())).collect();
        assert_eq!(got, vec![
            (Some("Lovelace, Ada"), "ada@example.com"),
            (Some("Bob B"), "bob@example.org"),
            (None, "c@example.net"),
            (Some("D \"Q\""), "d@example.net"),
        ]);
    }

    #[test]
    fn encoded_words_decode() {
        assert_eq!(decode_words("=?UTF-8?B?Q2Fmw6k=?= menu"), "Café menu");
        assert_eq!(decode_words("=?utf-8?Q?Caf=C3=A9_au_lait?="), "Café au lait");
        assert_eq!(decode_words("=?UTF-8?Q?a?= =?UTF-8?Q?b?="), "ab");
        assert_eq!(decode_words("=?ISO-8859-1?Q?Jos=E9?= Lopez"), "José Lopez");
        assert_eq!(decode_words("plain =? not a word"), "plain =? not a word");
        let a = parse_list("=?UTF-8?B?w4lsaXNl?= <elise@example.com>");
        assert_eq!(a[0].name.as_deref(), Some("Élise"));
    }

    #[test]
    fn message_ids() {
        assert_eq!(parse_ids("<a@x> <b@y>\r\n <c@z>"), vec!["a@x", "b@y", "c@z"]);
        assert_eq!(parse_ids("bare@x"), vec!["bare@x"]);
        assert!(parse_ids("").is_empty());
    }

    #[test]
    fn hostile_headers_do_not_panic() {
        for s in ["<", ">", "\"", "(((", "=?", "=?utf-8?Q?=", "=?x?B?!!!?=", "a@b <", "<<a@b>>", ",,,", "é@é"] {
            let _ = parse_list(s);
            let _ = decode_words(s);
            let _ = parse_ids(s);
        }
    }
}
