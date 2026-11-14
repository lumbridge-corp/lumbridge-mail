//! The text that is embedded for one message.
//!
//! Each chunk is a short head (subject and sender name) and a window of the new part of the body:
//! quoted history is never embedded (it belongs to the message it quotes). Windows overlap so a
//! sentence cut at a boundary is whole in one of them, are cut at a paragraph, sentence or word
//! where possible, and there are at most `max_chunks` of them (long newsletters keep their start).
//! Every chunk carries the sha256 of its text: a message whose chunks did not change is not sent to
//! the embedding endpoint again.

use lumbridge_mail_ai::text;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// How text is cut.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ChunkOptions {
    /// Characters per chunk, head included (about 4 characters per token: 3,000 is ~750 tokens).
    pub chars: usize,
    /// Characters shared by neighbouring chunks.
    pub overlap: usize,
    /// Chunks per message at most.
    pub max_chunks: usize,
}

impl Default for ChunkOptions {
    fn default() -> Self {
        Self {
            chars: 3000,
            overlap: 300,
            max_chunks: 4,
        }
    }
}

/// One chunk to embed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub index: i32,
    pub text: String,
    /// sha256 (hex) of `text`.
    pub hash: String,
}

pub fn hash(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The chunks of a message: `subject`, the sender's name (or address when it has none) and
/// `body_text` without its quoted history.
pub fn chunks(
    subject: &str,
    from_name: Option<&str>,
    from_email: &str,
    body_text: &str,
    o: &ChunkOptions,
) -> Vec<Chunk> {
    let from = from_name
        .map(|n| text::one_line(n, 80))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| text::one_line(from_email, 80));
    let head = format!(
        "Subject: {}\nFrom: {from}\n\n",
        text::one_line(subject, 300)
    );
    let body: Vec<char> = text::clean(&text::strip_quoted(body_text))
        .trim()
        .chars()
        .collect();
    let head_len = head.chars().count();
    let room = o.chars.saturating_sub(head_len).max(200);
    let overlap = o.overlap.min(room / 2);
    let max = o.max_chunks.max(1);
    let mut out = Vec::new();
    let make = |i: usize, part: &[char]| {
        let mut t = head.clone();
        t.extend(part.iter());
        let t = t.trim_end().to_string();
        Chunk {
            index: i as i32,
            hash: hash(&t),
            text: t,
        }
    };
    if body.is_empty() {
        out.push(make(0, &[]));
        return out;
    }
    let mut start = 0usize;
    while start < body.len() && out.len() < max {
        let mut end = (start + room).min(body.len());
        if end < body.len() {
            end = cut_point(&body, start, end);
        }
        out.push(make(out.len(), &body[start..end]));
        if end >= body.len() {
            break;
        }
        // Step back by the overlap, to the start of a word, but always forward.
        let mut next = end.saturating_sub(overlap).max(start + 1);
        while next < end && !body[next - 1].is_whitespace() {
            next += 1;
        }
        if next >= end {
            next = end;
        }
        start = next;
    }
    out
}

/// Where to end a window `[start, end)`: the last paragraph break, else sentence end, else space in
/// its last fifth; `end` itself when there is none.
fn cut_point(body: &[char], start: usize, end: usize) -> usize {
    let floor = end - (end - start) / 5;
    let find = |pred: &dyn Fn(usize) -> bool| (floor..end).rev().find(|&i| pred(i));
    if let Some(i) = find(&|i| body[i] == '\n' && i > 0 && body[i - 1] == '\n') {
        return i + 1;
    }
    if let Some(i) = find(&|i| matches!(body[i - 1], '.' | '!' | '?') && body[i].is_whitespace()) {
        return i + 1;
    }
    if let Some(i) = find(&|i| body[i].is_whitespace()) {
        return i + 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_mail_is_one_chunk_with_subject_and_sender() {
        let c = chunks(
            "Dinner Saturday?",
            Some("Maya Chen"),
            "maya@friends.example",
            "Are you free on Saturday?\n\nOn Fri, Sam wrote:\n> old words",
            &ChunkOptions::default(),
        );
        assert_eq!(c.len(), 1);
        assert_eq!(
            c[0].text,
            "Subject: Dinner Saturday?\nFrom: Maya Chen\n\nAre you free on Saturday?"
        );
        assert!(
            !c[0].text.contains("old words"),
            "quoted history is not embedded"
        );
        assert_eq!(c[0].hash, hash(&c[0].text));
        assert_eq!(c[0].index, 0);
    }

    #[test]
    fn a_sender_without_a_name_is_their_address_and_an_empty_body_still_embeds_the_subject() {
        let c = chunks(
            "Hello",
            None,
            "ada@example.com",
            "",
            &ChunkOptions::default(),
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].text, "Subject: Hello\nFrom: ada@example.com");
    }

    #[test]
    fn long_mail_is_cut_with_overlap_at_sentences_and_capped() {
        let sentence = "The quarterly report covers revenue, hiring and the new office. ";
        let body = sentence.repeat(200); // ~13,000 characters
        let o = ChunkOptions {
            chars: 1000,
            overlap: 100,
            max_chunks: 4,
        };
        let c = chunks("Report", Some("Ada"), "ada@example.com", &body, &o);
        assert_eq!(c.len(), 4, "capped");
        for (i, ch) in c.iter().enumerate() {
            assert_eq!(ch.index, i as i32);
            assert!(ch.text.starts_with("Subject: Report\nFrom: Ada\n\n"));
            assert!(ch.text.chars().count() <= 1000, "{}", ch.text.len());
            assert!(
                ch.text.ends_with('.'),
                "cut after a sentence: {:?}",
                &ch.text[ch.text.len() - 20..]
            );
        }
        // Neighbours share text.
        let tail: String = c[0]
            .text
            .chars()
            .rev()
            .take(40)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert!(c[1].text.contains(tail.trim()), "overlap");
        // Same input, same chunks; a change changes the hash.
        let again = chunks("Report", Some("Ada"), "ada@example.com", &body, &o);
        assert_eq!(again, c);
        let other = chunks("Report!", Some("Ada"), "ada@example.com", &body, &o);
        assert_ne!(other[0].hash, c[0].hash);
    }

    #[test]
    fn text_without_spaces_still_makes_progress() {
        let body = "x".repeat(5000);
        let o = ChunkOptions {
            chars: 1000,
            overlap: 100,
            max_chunks: 10,
        };
        let c = chunks("S", None, "a@b.example", &body, &o);
        assert!(c.len() >= 5 && c.len() <= 7, "{}", c.len());
    }

    #[test]
    fn invisible_characters_are_not_embedded() {
        let c = chunks(
            "Hi",
            Some("Ada"),
            "a@b.example",
            "pay\u{200B}ment due",
            &ChunkOptions::default(),
        );
        assert!(c[0].text.ends_with("payment due"));
    }
}
