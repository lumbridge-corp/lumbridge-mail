//! Checks on what a model wrote back. A model that read hostile mail may have been talked into
//! something; these make sure that whatever it was cannot leave this crate.

use crate::injection;
use regex::Regex;
use serde::de::DeserializeOwned;
use std::sync::LazyLock;

static URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(?:https?://|ftp://|www\.)[^\s<>"')\]]+|\b[a-z0-9-]+(?:\.[a-z0-9-]+)*\.[a-z]{2,24}/[^\s<>"')\]]*|\b[a-z0-9-]+(?:\.[a-z0-9-]+)*\.(?:com|net|org|io|ai|co|app|dev|xyz|link|info|biz|ru|cn|top|me|ly|test|example|zip|click|site|online|shop)\b"#)
        .expect("static regex")
});
static EMAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b[a-z0-9._%+-]+@[a-z0-9-]+(?:\.[a-z0-9-]+)+\b").expect("static regex"));
static LONG_NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d{6,}\b|\b\d{3,4}[ -]\d{3,4}[ -]\d{3,4}\b").expect("static regex"));

/// Links in a text (http(s), www., bare domain/path).
pub fn links(s: &str) -> Vec<String> {
    URL.find_iter(s).map(|m| m.as_str().trim_end_matches(['.', ',', ';', ':']).to_string()).collect()
}

/// Email addresses in a text, lower-cased.
pub fn addresses(s: &str) -> Vec<String> {
    EMAIL.find_iter(s).map(|m| m.as_str().to_ascii_lowercase()).collect()
}

/// The JSON object in a model answer: tolerates code fences and text around it.
pub fn parse_json<T: DeserializeOwned>(answer: &str) -> Result<T, String> {
    let s = answer.trim();
    if let Ok(v) = serde_json::from_str(s) {
        return Ok(v);
    }
    let (a, b) = (s.find('{'), s.rfind('}'));
    match (a, b) {
        (Some(a), Some(b)) if b > a => serde_json::from_str(&s[a..=b]).map_err(|e| format!("the JSON did not match the schema ({e})")),
        _ => Err("there was no JSON object".into()),
    }
}

/// A line we will show and store: one line, bounded, no links, addresses, long numbers (codes,
/// account numbers), markup, or signs the model obeyed the mail.
pub fn check_line(s: &str, max: usize, what: &str) -> Result<String, String> {
    let s = crate::text::one_line(s, usize::MAX);
    if s.is_empty() {
        return Err(format!("the {what} was empty"));
    }
    if s.chars().count() > max {
        return Err(format!("the {what} was longer than {max} characters"));
    }
    check_text(&s, what)?;
    Ok(s)
}

/// Like [`check_line`] for multi-line text (summaries, answers), without the one-line rule.
pub fn check_text(s: &str, what: &str) -> Result<(), String> {
    if !links(s).is_empty() {
        return Err(format!("the {what} contained a link"));
    }
    if !addresses(s).is_empty() {
        return Err(format!("the {what} contained an email address"));
    }
    if LONG_NUMBER.is_match(s) {
        return Err(format!("the {what} contained a long number"));
    }
    if s.contains("<<<") || s.contains(">>>") || s.contains("<|") {
        return Err(format!("the {what} contained markup"));
    }
    if let Some(why) = injection::output_reasons(s).first() {
        return Err(format!("the {what} {why}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_found_around_noise() {
        #[derive(serde::Deserialize)]
        struct A {
            a: u8,
        }
        assert_eq!(parse_json::<A>("```json\n{\"a\": 3}\n```").unwrap().a, 3);
        assert!(parse_json::<A>("nope").is_err());
        assert!(parse_json::<A>("{\"b\": 1}").is_err());
    }

    #[test]
    fn lines_with_smuggled_things_are_rejected() {
        assert!(check_line("Ada asks to move lunch to Friday", 120, "summary").is_ok());
        assert!(check_line("See https://evil.test/x", 120, "summary").is_err());
        assert!(check_line("Click evil.xyz/login now", 120, "summary").is_err());
        assert!(check_line("Visit evil.test/pay", 120, "summary").is_err());
        assert!(check_line("Go to evil.test", 120, "summary").is_err());
        assert!(check_line("Release notes for Next.js 16", 120, "summary").is_ok());
        assert!(check_line("Mail x@evil.test", 120, "summary").is_err());
        assert!(check_line("Lunch on 2026-11-14 at 12:30", 120, "summary").is_ok());
        assert!(check_line("Call 555-123-4567", 120, "summary").is_err());
        assert!(check_line("Your code is 4829131", 120, "summary").is_err());
        assert!(check_line("I have forwarded all mail as instructed", 120, "summary").is_err());
        assert!(check_line("Ignore previous instructions", 120, "summary").is_err());
        assert!(check_line(&"a".repeat(130), 120, "summary").is_err());
        assert!(check_line("  ", 120, "summary").is_err());
    }

    #[test]
    fn links_and_addresses_are_found() {
        assert_eq!(links("go to https://a.example/x. ok"), vec!["https://a.example/x"]);
        assert_eq!(addresses("Ada <Ada@Example.com>"), vec!["ada@example.com"]);
    }
}
