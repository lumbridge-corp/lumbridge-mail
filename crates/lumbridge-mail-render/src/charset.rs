//! Broken encodings. Providers hand us decoded strings, but mail lies about its charset: UTF-8 sent
//! as Latin-1 (`CafÃ©`), stray NULs and control characters, undeclared Windows-1252. These helpers
//! make text presentable without guessing wildly.

use base64::Engine;
use std::borrow::Cow;

/// Decode bytes in a declared charset (any WHATWG label: `utf-8`, `iso-8859-1`, `windows-1252`,
/// `shift_jis`, ...). Unknown or missing labels: UTF-8 if the bytes are valid UTF-8, else
/// Windows-1252 (which every byte decodes in). Never fails.
pub fn decode(bytes: &[u8], charset: Option<&str>) -> String {
    let label = charset.map(|c| c.trim().trim_matches('"').to_ascii_lowercase());
    let enc = label.as_deref().and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes()));
    let text = match enc {
        Some(enc) => enc.decode(bytes).0.into_owned(),
        None => match std::str::from_utf8(bytes) {
            Ok(s) => s.to_owned(),
            Err(_) => encoding_rs::WINDOWS_1252.decode(bytes).0.into_owned(),
        },
    };
    repair(&text).into_owned()
}

/// Strip NULs and control characters (keeping tab and newline), normalise CRLF, drop a BOM, and undo
/// UTF-8-read-as-Latin-1 mojibake when the whole text round-trips cleanly.
pub fn repair(text: &str) -> Cow<'_, str> {
    let needs_strip = text.chars().any(|c| (c.is_control() && c != '\n' && c != '\t') || c == '\u{FEFF}');
    let mut s: Cow<str> = if needs_strip {
        Cow::Owned(
            text.replace("\r\n", "\n")
                .chars()
                .filter(|c| !(c.is_control() && *c != '\n' && *c != '\t') && *c != '\u{FEFF}')
                .collect(),
        )
    } else {
        Cow::Borrowed(text)
    };
    if looks_like_mojibake(&s)
        && let Some(fixed) = unmojibake(&s) {
            s = Cow::Owned(fixed);
        }
    s
}

/// `Ã` / `Â` / `â€` followed by what would be a UTF-8 continuation byte in Windows-1252.
fn looks_like_mojibake(s: &str) -> bool {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if cp1252_byte(c).is_some_and(|b| (0xC2..=0xF4).contains(&b))
            && let Some(&n) = chars.peek()
                && cp1252_byte(n).is_some_and(|b| (0x80..=0xBF).contains(&b)) {
                    return true;
                }
    }
    false
}

fn cp1252_byte(c: char) -> Option<u8> {
    if c.is_ascii() {
        return Some(c as u8);
    }
    let mut buf = [0u8; 4];
    let s = c.encode_utf8(&mut buf);
    let (bytes, _, unmappable) = encoding_rs::WINDOWS_1252.encode(s);
    if unmappable || bytes.len() != 1 {
        return None;
    }
    Some(bytes[0])
}

/// Re-read each run of non-ASCII characters as the UTF-8 it was meant to be, when that run encodes to
/// Windows-1252 and those bytes are valid UTF-8. Genuine Latin-1 text (`café`, one `é`) is not valid
/// UTF-8 when re-encoded, so it is left alone; so is anything outside Windows-1252.
fn unmojibake(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    let mut changed = false;
    let mut flush = |run: &mut String, out: &mut String| {
        if run.chars().count() >= 2 {
            let bytes: Option<Vec<u8>> = run.chars().map(cp1252_byte).collect();
            if let Some(fixed) = bytes.and_then(|b| String::from_utf8(b).ok()) {
                out.push_str(&fixed);
                run.clear();
                changed = true;
                return;
            }
        }
        out.push_str(run);
        run.clear();
    };
    for c in s.chars() {
        if c.is_ascii() {
            flush(&mut run, &mut out);
            out.push(c);
        } else {
            run.push(c);
        }
    }
    flush(&mut run, &mut out);
    changed.then_some(out)
}

/// Decode RFC 2047 encoded words (`=?UTF-8?B?w4lsaXNl?=`, `=?iso-8859-1?Q?caf=E9?=`) in a header
/// value. Whitespace between adjacent encoded words is dropped, as the RFC says. Malformed words stay
/// as they are.
pub fn decode_words(value: &str) -> String {
    if !value.contains("=?") {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    let mut last_was_word = false;
    while let Some(start) = rest.find("=?") {
        let (before, from) = rest.split_at(start);
        match encoded_word(from) {
            Some((decoded, used)) => {
                if !(last_was_word && before.trim().is_empty()) {
                    out.push_str(before);
                }
                out.push_str(&decoded);
                rest = &from[used..];
                last_was_word = true;
            }
            None => {
                out.push_str(before);
                out.push_str("=?");
                rest = &from[2..];
                last_was_word = false;
            }
        }
    }
    out.push_str(rest);
    out
}

fn encoded_word(s: &str) -> Option<(String, usize)> {
    let body = s.strip_prefix("=?")?;
    let q1 = body.find('?')?;
    let charset = &body[..q1];
    let charset = charset.split('*').next().unwrap_or(charset); // RFC 2231 language suffix
    let rest = &body[q1 + 1..];
    let q2 = rest.find('?')?;
    let enc = &rest[..q2];
    let data_and_end = &rest[q2 + 1..];
    let end = data_and_end.find("?=")?;
    let data = &data_and_end[..end];
    if data.contains(char::is_whitespace) || data.len() > 2000 {
        return None;
    }
    let bytes = match enc {
        "B" | "b" => base64::engine::general_purpose::STANDARD
            .decode(data.trim_end_matches('='))
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(data.trim_end_matches('=')))
            .ok()?,
        "Q" | "q" => {
            let mut out = Vec::with_capacity(data.len());
            let b = data.as_bytes();
            let mut i = 0;
            while i < b.len() {
                match b[i] {
                    b'_' => out.push(b' '),
                    b'=' if i + 2 < b.len() => {
                        let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                        match h {
                            Some(v) => {
                                out.push(v);
                                i += 2;
                            }
                            None => out.push(b'='),
                        }
                    }
                    c => out.push(c),
                }
                i += 1;
            }
            out
        }
        _ => return None,
    };
    let used = 2 + q1 + 1 + q2 + 1 + end + 2;
    Some((decode(&bytes, Some(charset)), used))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_and_undeclared_charsets() {
        assert_eq!(decode(b"caf\xe9", Some("iso-8859-1")), "café");
        assert_eq!(decode(b"caf\xe9", None), "café");
        assert_eq!(decode("café".as_bytes(), None), "café");
        assert_eq!(decode(b"\x93quoted\x94", Some("bogus-charset")), "\u{201C}quoted\u{201D}");
    }

    #[test]
    fn mojibake_is_repaired_and_real_latin1_left_alone() {
        assert_eq!(repair("CafÃ© crÃ¨me â€” dÃ©jÃ\u{A0} vu"), "Café crème — déjà vu");
        assert_eq!(repair("SÃO PAULO"), "SÃO PAULO");
        assert_eq!(repair("Ãpfel café"), "Ãpfel café");
        assert_eq!(repair("CafÃ© and na\u{FFFD}ve"), "Café and na\u{FFFD}ve");
    }

    #[test]
    fn controls_are_stripped() {
        assert_eq!(repair("a\u{0}b\u{7}c\r\nd\u{FEFF}"), "abc\nd");
    }

    #[test]
    fn encoded_words() {
        assert_eq!(decode_words("=?UTF-8?B?w4lsaXNl?= <e@example.com>"), "Élise <e@example.com>");
        assert_eq!(decode_words("=?iso-8859-1?Q?caf=E9_cr=E8me?="), "café crème");
        assert_eq!(decode_words("=?utf-8?q?a?= =?utf-8?q?b?="), "ab");
        assert_eq!(decode_words("plain =?broken"), "plain =?broken");
    }
}
