//! The pieces of mail threading every provider and the store share (pure): Message-IDs as headers write
//! them, and subjects without their reply and forward prefixes. IMAP builds its thread ids from these
//! (`lumbridge-mail-imap`'s `ids::thread_id`); the store groups mail of providers that give no thread
//! ids with the same rules (`lumbridge-mail-store`'s `threading`).

/// A Message-ID without its angle brackets and surrounding space.
pub fn clean_message_id(s: &str) -> String {
    s.trim().trim_start_matches('<').trim_end_matches('>').trim().to_owned()
}

/// Every `<id>` in a `References` or `In-Reply-To` value, in order, cleaned. A value without angle
/// brackets counts as one id per whitespace-separated word.
pub fn parse_message_ids(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    if value.contains('<') {
        for part in value.split('<').skip(1) {
            if let Some((id, _)) = part.split_once('>') {
                let id = clean_message_id(id);
                if !id.is_empty() && !id.contains(char::is_whitespace) {
                    out.push(id);
                }
            }
        }
    } else {
        out.extend(value.split_whitespace().map(clean_message_id).filter(|i| !i.is_empty()));
    }
    out
}

/// Subject without reply/forward prefixes and list tags, lower-cased, whitespace collapsed.
pub fn normalise_subject(subject: &str) -> String {
    let mut s = subject.trim().to_owned();
    loop {
        let before = s.clone();
        // `[list-name] `
        if s.starts_with('[')
            && let Some(end) = s.find(']')
        {
            s = s[end + 1..].trim_start().to_owned();
        }
        let starts = |s: &str, p: &str| s.get(..p.len()).is_some_and(|h| h.eq_ignore_ascii_case(p));
        for p in ["re:", "re :", "fw:", "fwd:", "aw:", "sv:", "vs:", "antw:", "wg:", "tr:", "rif:", "odp:"] {
            if starts(&s, p) {
                s = s[p.len()..].trim_start().to_owned();
                break;
            }
        }
        // `Re[2]:`
        if starts(&s, "re[")
            && let Some(end) = s.find("]:")
        {
            s = s[end + 2..].trim_start().to_owned();
        }
        if s == before {
            break;
        }
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// The subject carries a reply or forward prefix (or a list tag): it answers or passes on something.
pub fn has_reply_prefix(subject: &str) -> bool {
    normalise_subject(subject) != subject.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjects_normalise() {
        assert_eq!(normalise_subject("Re: Fwd: [list] AW: Hello  World"), "hello world");
        assert_eq!(normalise_subject("Re[3]: Hi"), "hi");
        assert_eq!(normalise_subject("Plain"), "plain");
        assert!(has_reply_prefix("Re: x"));
        assert!(!has_reply_prefix("Regarding x"));
    }

    #[test]
    fn message_ids_parse() {
        assert_eq!(clean_message_id(" <a@x> "), "a@x");
        assert_eq!(parse_message_ids("<a@x>\r\n <b@y> junk <c@z>"), ["a@x", "b@y", "c@z"]);
        assert_eq!(parse_message_ids("a@x b@y"), ["a@x", "b@y"]);
        assert!(parse_message_ids("<>").is_empty());
    }
}
