//! Clean plain text: for list-row snippets, search, and the AI layer. No tracking junk (zero-width
//! padding, hidden preheaders, tracking pixels, `utm_` parameters), no quoted history unless asked,
//! bounded length.

use crate::blocks::{self, Block, Document, ImageSource, Span};
use crate::{Policy, charset, links};
use lumbridge_mail_core::Body;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextOptions {
    /// Include the quoted history.
    pub quoted: bool,
    /// Include the signature.
    pub signature: bool,
    /// Show link targets as `text <url>` (tracking parameters removed). Off: link text only, and bare
    /// URLs in text parts are dropped.
    pub links: bool,
    /// Mention images as `[image: alt]`.
    pub images: bool,
    /// Hard cap in characters; the cut is made at a word boundary and marked with `…`.
    pub max_chars: usize,
}

impl Default for TextOptions {
    fn default() -> Self {
        Self { quoted: false, signature: false, links: false, images: false, max_chars: 20_000 }
    }
}

impl TextOptions {
    /// What the AI layer reads: the new content and its links, nothing quoted.
    pub fn for_ai() -> Self {
        Self { links: true, max_chars: 12_000, ..Self::default() }
    }
}

/// Clean text of a message. Uses the text part when it is real (not a "view this in your browser"
/// stub), else the HTML part rendered to blocks.
pub fn plain_text(body: &Body, opts: &TextOptions) -> String {
    let policy = Policy::default();
    let text_ok = body.text.as_deref().is_some_and(|t| usable_text_part(t, body.html.as_deref()));
    let doc = if text_ok || body.html.as_deref().is_none_or(|h| h.trim().is_empty()) {
        let t = crate::cut(body.text.as_deref().unwrap_or(""), policy.limits.max_input_bytes).0;
        let t = if opts.links { charset::repair(t).into_owned() } else { drop_bare_urls(&charset::repair(t)) };
        blocks::blocks_from_text(&t, &policy)
    } else {
        blocks::blocks(&Body { text: None, ..body.clone() }, &policy)
    };
    document_text(&doc, opts)
}

/// One line of up to `max` characters for a thread-list row.
pub fn snippet(body: &Body, max: usize) -> String {
    let opts = TextOptions { max_chars: max.saturating_mul(4).max(200), ..TextOptions::default() };
    let text = plain_text(body, &opts);
    let text: String = text.lines().skip_while(|l| l.trim().is_empty() || is_boilerplate(l)).collect::<Vec<_>>().join("\n");
    let line = crate::dom::collapse_ws(&text);
    truncate(&line, max)
}

/// The text of a rendered document.
pub fn document_text(doc: &Document, opts: &TextOptions) -> String {
    let mut out = render(&doc.blocks, opts);
    if opts.signature && !doc.signature.is_empty() {
        out.push_str("\n\n");
        out.push_str(&render(&doc.signature, opts));
    }
    if opts.quoted && !doc.quoted.is_empty() {
        out.push_str("\n\n");
        out.push_str(&render(&doc.quoted, opts));
    }
    truncate(out.trim(), opts.max_chars)
}

/// Blocks to text (also the plain-text part of outgoing mail).
pub fn render(blocks: &[Block], opts: &TextOptions) -> String {
    let mut out = String::new();
    for b in blocks {
        let piece = block_text(b, opts);
        if piece.trim().is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&piece);
    }
    out
}

fn block_text(b: &Block, opts: &TextOptions) -> String {
    match b {
        Block::Paragraph { spans } | Block::Heading { spans, .. } => spans_text(spans, opts),
        Block::List { ordered, start, items } => items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let marker = if *ordered { format!("{}. ", *start as usize + i) } else { "- ".to_owned() };
                let body = render(item, opts);
                let indent = " ".repeat(marker.chars().count());
                let mut lines = body.lines();
                let mut s = format!("{marker}{}", lines.next().unwrap_or(""));
                for l in lines {
                    s.push('\n');
                    if !l.is_empty() {
                        s.push_str(&indent);
                    }
                    s.push_str(l);
                }
                s
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Block::Quote { blocks, .. } => render(blocks, opts)
            .lines()
            .map(|l| if l.is_empty() { ">".to_owned() } else { format!("> {l}") })
            .collect::<Vec<_>>()
            .join("\n"),
        Block::Code { text } => text.clone(),
        Block::Table { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|c| spans_text(&c.spans, opts).replace('\n', " ")).collect::<Vec<_>>().join(" | "))
            .collect::<Vec<_>>()
            .join("\n"),
        Block::Image(img) => {
            if !opts.images {
                return String::new();
            }
            let alt = img.alt.as_deref().unwrap_or("").trim();
            match (&img.source, alt.is_empty()) {
                (_, false) => format!("[image: {alt}]"),
                (ImageSource::Remote { .. } | ImageSource::Cid { .. } | ImageSource::Data { .. }, true) => "[image]".to_owned(),
                _ => String::new(),
            }
        }
        Block::Rule => "---".to_owned(),
    }
}

fn spans_text(spans: &[Span], opts: &TextOptions) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < spans.len() {
        let s = &spans[i];
        match &s.link {
            Some(link) if opts.links => {
                // Gather the whole link (it may span several styled runs).
                let mut text = String::new();
                while i < spans.len() && spans[i].link.as_ref() == Some(link) {
                    text.push_str(&spans[i].text);
                    i += 1;
                }
                let body = text.trim_end();
                out.push_str(body);
                let href = links::strip_tracking_params(&link.href);
                let shown = body.trim().trim_end_matches('/');
                let bare = href.trim_start_matches("mailto:").trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/');
                if !shown.eq_ignore_ascii_case(bare) && !shown.eq_ignore_ascii_case(href.trim_end_matches('/')) {
                    out.push_str(&format!(" <{href}>"));
                }
                out.push_str(&text[body.len()..]);
                continue;
            }
            _ => out.push_str(&s.text),
        }
        i += 1;
    }
    out
}

/// A text part is usable unless it is tiny while the HTML part is not (the "open in browser" stub).
fn usable_text_part(text: &str, html: Option<&str>) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    let Some(html) = html else { return true };
    let html_len = html.len();
    !(t.chars().count() < 80 && html_len > 2_000)
}

/// Remove link annotations from text parts: URLs in brackets (`( https://... )`, `<https://...>`) and
/// long (tracking) URLs. Short URLs in running prose stay, since they are the content.
fn drop_bare_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((a, b)) = blocks::find_url(rest) {
        let mut head = &rest[..a];
        let mut tail = &rest[b..];
        let mut bracketed = false;
        for (open, close) in [("(", ")"), ("<", ">"), ("[", "]")] {
            let h = head.trim_end();
            if h.ends_with(open) && tail.trim_start().starts_with(close) {
                head = &h[..h.len() - open.len()];
                let t = tail.trim_start();
                tail = &t[close.len()..];
                bracketed = true;
            }
        }
        if !bracketed && b - a <= 60 {
            out.push_str(&rest[..b]);
            rest = &rest[b..];
            continue;
        }
        out.push_str(head.trim_end_matches([' ', '\t']));
        rest = tail;
        if !rest.starts_with(['\n', ' ']) && !out.ends_with(['\n', ' ']) && !rest.is_empty() {
            out.push(' ');
        }
    }
    out.push_str(rest);
    out
}

/// Lines newsletters open with that say nothing about the mail.
fn is_boilerplate(line: &str) -> bool {
    let l = line.trim().to_lowercase();
    l.len() < 80
        && ((l.contains("view") && (l.contains("browser") || l.contains("online") || l.contains("web version")))
            || l.starts_with("having trouble viewing")
            || l.starts_with("can't see this email")
            || l.starts_with("trouble viewing"))
}

/// Cut to `max` characters at a word boundary, marking the cut with `…`.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let keep = max.saturating_sub(1);
    let cut: String = s.chars().take(keep).collect();
    let at = cut.rfind(char::is_whitespace).filter(|i| *i > keep / 2).unwrap_or(cut.len());
    format!("{}…", cut[..at].trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(text: Option<&str>, html: Option<&str>) -> Body {
        Body { text: text.map(Into::into), html: html.map(Into::into), ..Body::default() }
    }

    #[test]
    fn html_only_mail_gives_clean_text() {
        let b = body(None, Some("<div style='display:none'>preheader junk</div><p>Hello&nbsp;&zwnj;&nbsp;&zwnj; there</p><img src='https://t.example/o.gif' width=1 height=1><p><a href='https://example.com/a?utm_source=x'>Read</a></p>"));
        assert_eq!(plain_text(&b, &TextOptions::default()), "Hello there\n\nRead");
        assert_eq!(plain_text(&b, &TextOptions::for_ai()), "Hello there\n\nRead <https://example.com/a>");
    }

    #[test]
    fn quoted_history_is_left_out_unless_asked() {
        let b = body(Some("Yes!\n\nOn Mon, Nov 2, 2026 at 10:00 AM Ada <ada@example.com> wrote:\n> Coffee?"), None);
        assert_eq!(plain_text(&b, &TextOptions::default()), "Yes!");
        let all = plain_text(&b, &TextOptions { quoted: true, ..TextOptions::default() });
        assert!(all.contains("> Coffee?"));
    }

    #[test]
    fn snippets_are_one_bounded_line() {
        let b = body(Some(&"word ".repeat(500)), None);
        let s = snippet(&b, 100);
        assert!(s.chars().count() <= 100);
        assert!(s.ends_with('…'));
        assert!(!s.contains('\n'));
    }

    #[test]
    fn bare_urls_dropped_from_text_parts() {
        assert_eq!(drop_bare_urls("View online ( https://click.example.com/x?id=1 )\nHi"), "View online\nHi");
        assert_eq!(drop_bare_urls("Shop <https://example.com/s> now"), "Shop now");
        assert_eq!(drop_bare_urls("Menu at https://example.com/menu."), "Menu at https://example.com/menu.");
        let long = "Track: https://click.example.com/ls/click?upn=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa done";
        assert_eq!(drop_bare_urls(long), "Track: done");
    }
}
