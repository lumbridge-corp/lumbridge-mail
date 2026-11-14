//! Outgoing mail: Markdown (or a block tree) to a plain-text part and an HTML part for
//! `multipart/alternative`, with the original quoted for replies and forwards.
//!
//! Drafts may be written by a model, so raw HTML in the Markdown is never passed through: it is
//! shown as text. Links are only emitted for `http(s)`, `mailto` and `tel` targets.

use crate::blocks::{self, Block, Cell, Image, ImageSource, Link, Span};
use crate::dom::{escape_attr, escape_text};
use crate::text::{self, TextOptions};
use crate::{Policy, RemoteContent, links, sanitize};
use lumbridge_mail_core::{Address, Body, MessageMeta};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};

/// The two parts of an outgoing message.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Outgoing {
    /// `text/plain; charset=utf-8`, LF line endings (the MIME builder converts to CRLF).
    pub text: String,
    /// `text/html; charset=utf-8`, a complete document.
    pub html: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteKind {
    Reply,
    Forward,
}

/// The message being replied to or forwarded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Original {
    pub kind: QuoteKind,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    /// Unix seconds.
    pub date: i64,
    /// The user's UTC offset for the attribution line ("On Tue, Nov 10, 2026 at 3:04 PM ...").
    pub utc_offset_s: i32,
    pub subject: String,
    pub body: Body,
}

impl Original {
    pub fn from_meta(kind: QuoteKind, meta: &MessageMeta, body: Body, utc_offset_s: i32) -> Self {
        Self {
            kind,
            from: meta.from.first().cloned().unwrap_or_else(|| Address::new(None, "")),
            to: meta.to.clone(),
            cc: meta.cc.clone(),
            date: meta.date,
            utc_offset_s,
            subject: meta.subject.clone(),
            body,
        }
    }
}

/// `Re: ` / `Fwd: ` once, not `Re: Re: `.
pub fn subject_for(kind: QuoteKind, subject: &str) -> String {
    let s = subject.trim();
    let lower = s.to_ascii_lowercase();
    match kind {
        QuoteKind::Reply if lower.starts_with("re:") => s.to_owned(),
        QuoteKind::Reply => format!("Re: {s}"),
        QuoteKind::Forward if lower.starts_with("fwd:") || lower.starts_with("fw:") => s.to_owned(),
        QuoteKind::Forward => format!("Fwd: {s}"),
    }
}

/// Markdown draft (+ optional original) to text and HTML parts.
pub fn compose(markdown: &str, original: Option<&Original>) -> Outgoing {
    compose_blocks(&markdown_blocks(markdown), original)
}

/// Block tree draft (+ optional original) to text and HTML parts.
pub fn compose_blocks(blocks: &[Block], original: Option<&Original>) -> Outgoing {
    let opts = TextOptions { links: true, images: true, max_chars: usize::MAX, ..TextOptions::default() };
    let mut text = text::render(blocks, &opts);
    let mut html = blocks_to_html(blocks);
    if let Some(o) = original {
        let (qt, qh) = quote(o);
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&qt);
        html.push_str(&qh);
    }
    text.push('\n');
    let html = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"color-scheme\" content=\"light dark\"></head><body><div dir=\"auto\">{html}</div></body></html>"
    );
    Outgoing { text, html }
}

fn who(a: &Address) -> String {
    match &a.name {
        Some(n) => format!("{n} <{}>", a.email),
        None => a.email.clone(),
    }
}

fn who_list(list: &[Address]) -> String {
    list.iter().map(who).collect::<Vec<_>>().join(", ")
}

fn quote(o: &Original) -> (String, String) {
    let date = format_date(o.date, o.utc_offset_s);
    let opts = TextOptions { quoted: true, signature: true, links: true, images: false, max_chars: 200_000 };
    let orig_text = text::plain_text(&o.body, &opts);
    let orig_html = match o.body.html.as_deref() {
        Some(h) if !h.trim().is_empty() => {
            let policy = Policy { remote: RemoteContent::Allowed, keep_colors: true, fold: false, ..Policy::default() };
            sanitize(h, &o.body.attachments, &policy).full()
        }
        _ => {
            let doc = blocks::blocks_from_text(o.body.text.as_deref().unwrap_or(""), &Policy { fold: false, ..Policy::default() });
            blocks_to_html(&doc.blocks)
        }
    };
    match o.kind {
        QuoteKind::Reply => {
            let attribution = format!("On {date}, {} wrote:", who(&o.from));
            let quoted: Vec<String> = orig_text.lines().map(|l| if l.is_empty() { ">".to_owned() } else { format!("> {l}") }).collect();
            let text = format!("{attribution}\n{}", quoted.join("\n"));
            let mut html = String::from("<div class=\"gmail_quote\"><div class=\"gmail_attr\">");
            escape_text(&attribution, &mut html);
            html.push_str("<br></div><blockquote class=\"gmail_quote\" style=\"margin:0 0 0 .8ex;border-left:1px solid #ccc;padding-left:1ex\">");
            html.push_str(&orig_html);
            html.push_str("</blockquote></div>");
            (text, html)
        }
        QuoteKind::Forward => {
            let mut head = vec![
                "---------- Forwarded message ---------".to_owned(),
                format!("From: {}", who(&o.from)),
                format!("Date: {date}"),
                format!("Subject: {}", o.subject),
            ];
            if !o.to.is_empty() {
                head.push(format!("To: {}", who_list(&o.to)));
            }
            if !o.cc.is_empty() {
                head.push(format!("Cc: {}", who_list(&o.cc)));
            }
            let text = format!("{}\n\n{orig_text}", head.join("\n"));
            let mut html = String::from("<div class=\"gmail_quote\"><div class=\"gmail_attr\">");
            for line in &head {
                escape_text(line, &mut html);
                html.push_str("<br>");
            }
            html.push_str("</div><br>");
            html.push_str(&orig_html);
            html.push_str("</div>");
            (text, html)
        }
    }
}

/// "Tue, Nov 10, 2026 at 3:04 PM" in the given UTC offset.
pub fn format_date(ts: i64, utc_offset_s: i32) -> String {
    let t = ts + utc_offset_s as i64;
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][(m - 1) as usize];
    let (h, min) = (secs / 3600, (secs % 3600) / 60);
    let (h12, ampm) = match h {
        0 => (12, "AM"),
        1..=11 => (h, "AM"),
        12 => (12, "PM"),
        _ => (h - 12, "PM"),
    };
    format!("{weekday}, {month} {d}, {y} at {h12}:{min:02} {ampm}")
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---- Markdown to blocks ----

enum Frame {
    Root(Vec<Block>),
    Quote(Vec<Block>),
    List { ordered: bool, start: u32, items: Vec<Vec<Block>> },
    Item(Vec<Block>),
    Para { spans: Vec<Span>, implicit: bool },
    Heading(u8, Vec<Span>),
    Code(String),
    Table { rows: Vec<Vec<Cell>>, header_rows: u32 },
    Row(Vec<Cell>, bool),
    Cell(Vec<Span>, bool),
}

#[derive(Default)]
struct Inline {
    bold: u32,
    italic: u32,
    strike: u32,
    link: Vec<Option<Link>>,
    image: Option<(String, String)>,
}

/// Parse Markdown (CommonMark + tables + strikethrough) to blocks. Raw HTML becomes text.
pub fn markdown_blocks(md: &str) -> Vec<Block> {
    let mut stack = vec![Frame::Root(Vec::new())];
    let mut st = Inline::default();
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    for ev in Parser::new_ext(md, opts) {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    close_implicit(&mut stack);
                    stack.push(Frame::Para { spans: Vec::new(), implicit: false });
                }
                Tag::Heading { level, .. } => {
                    close_implicit(&mut stack);
                    let l = match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        HeadingLevel::H4 => 4,
                        HeadingLevel::H5 => 5,
                        HeadingLevel::H6 => 6,
                    };
                    stack.push(Frame::Heading(l, Vec::new()));
                }
                Tag::BlockQuote(_) => {
                    close_implicit(&mut stack);
                    stack.push(Frame::Quote(Vec::new()));
                }
                Tag::CodeBlock(_) => {
                    close_implicit(&mut stack);
                    stack.push(Frame::Code(String::new()));
                }
                Tag::List(start) => {
                    close_implicit(&mut stack);
                    stack.push(Frame::List { ordered: start.is_some(), start: start.unwrap_or(1).min(u32::MAX as u64) as u32, items: Vec::new() });
                }
                Tag::Item => stack.push(Frame::Item(Vec::new())),
                Tag::Table(_) => {
                    close_implicit(&mut stack);
                    stack.push(Frame::Table { rows: Vec::new(), header_rows: 0 });
                }
                Tag::TableHead => stack.push(Frame::Row(Vec::new(), true)),
                Tag::TableRow => stack.push(Frame::Row(Vec::new(), false)),
                Tag::TableCell => {
                    let head = matches!(stack.last(), Some(Frame::Row(_, true)));
                    stack.push(Frame::Cell(Vec::new(), head));
                }
                Tag::Emphasis => st.italic += 1,
                Tag::Strong => st.bold += 1,
                Tag::Strikethrough => st.strike += 1,
                Tag::Link { dest_url, .. } => st.link.push(links::safe_href(&dest_url).map(|href| Link { href, warning: None })),
                Tag::Image { dest_url, .. } => st.image = Some((dest_url.to_string(), String::new())),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::BlockQuote(_) | TagEnd::CodeBlock | TagEnd::List(_) | TagEnd::Item | TagEnd::Table => {
                    close_implicit(&mut stack);
                    pop_into_parent(&mut stack);
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    if let Some(Frame::Row(cells, head)) = stack.pop()
                        && let Some(Frame::Table { rows, header_rows }) = stack.last_mut() {
                            if head {
                                *header_rows += 1;
                            }
                            rows.push(cells);
                        }
                }
                TagEnd::TableCell => {
                    if let Some(Frame::Cell(spans, header)) = stack.pop()
                        && let Some(Frame::Row(cells, _)) = stack.last_mut() {
                            cells.push(Cell { spans: trim(spans), header, colspan: 1 });
                        }
                }
                TagEnd::Emphasis => st.italic = st.italic.saturating_sub(1),
                TagEnd::Strong => st.bold = st.bold.saturating_sub(1),
                TagEnd::Strikethrough => st.strike = st.strike.saturating_sub(1),
                TagEnd::Link => {
                    st.link.pop();
                }
                TagEnd::Image => {
                    if let Some((url, alt)) = st.image.take() {
                        let shown = if alt.trim().is_empty() { url.clone() } else { alt };
                        let link = links::safe_href(&url).map(|href| Link { href, warning: None });
                        push_span(&mut stack, &st, shown, false, link);
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                if let Some(Frame::Code(code)) = stack.last_mut() {
                    code.push_str(&t);
                } else if let Some((_, alt)) = st.image.as_mut() {
                    alt.push_str(&t);
                } else {
                    push_span(&mut stack, &st, t.to_string(), false, None);
                }
            }
            Event::Code(t) => push_span(&mut stack, &st, t.to_string(), true, None),
            Event::Html(t) | Event::InlineHtml(t) => {
                if let Some(Frame::Code(code)) = stack.last_mut() {
                    code.push_str(&t);
                } else {
                    let t = t.trim_end_matches('\n').to_string();
                    push_span(&mut stack, &st, t, false, None);
                }
            }
            Event::SoftBreak => push_span(&mut stack, &st, "\n".into(), false, None),
            Event::HardBreak => push_span(&mut stack, &st, "\n".into(), false, None),
            Event::Rule => {
                close_implicit(&mut stack);
                push_block(&mut stack, Block::Rule);
            }
            Event::TaskListMarker(done) => push_span(&mut stack, &st, if done { "[x] ".into() } else { "[ ] ".into() }, false, None),
            _ => {}
        }
    }
    while stack.len() > 1 {
        close_implicit(&mut stack);
        if stack.len() > 1 {
            pop_into_parent(&mut stack);
        }
    }
    match stack.pop() {
        Some(Frame::Root(b)) => b,
        _ => Vec::new(),
    }
}

fn trim(spans: Vec<Span>) -> Vec<Span> {
    let mut spans: Vec<Span> = spans.into_iter().filter(|s| !s.text.is_empty()).collect();
    if let Some(f) = spans.first_mut() {
        f.text = f.text.trim_start().to_owned();
    }
    if let Some(l) = spans.last_mut() {
        l.text = l.text.trim_end().to_owned();
    }
    spans.retain(|s| !s.text.is_empty());
    spans
}

fn push_span(stack: &mut Vec<Frame>, st: &Inline, text: String, code: bool, link: Option<Link>) {
    let span = Span {
        text,
        bold: st.bold > 0,
        italic: st.italic > 0,
        code,
        strike: st.strike > 0,
        link: link.or_else(|| st.link.last().cloned().flatten()),
    };
    let target = match stack.last_mut() {
        Some(Frame::Para { spans, .. } | Frame::Heading(_, spans) | Frame::Cell(spans, _)) => spans,
        _ => {
            stack.push(Frame::Para { spans: Vec::new(), implicit: true });
            match stack.last_mut() {
                Some(Frame::Para { spans, .. }) => spans,
                _ => return,
            }
        }
    };
    match target.last_mut() {
        Some(l) if l.bold == span.bold && l.italic == span.italic && l.code == span.code && l.strike == span.strike && l.link == span.link => {
            l.text.push_str(&span.text)
        }
        _ => target.push(span),
    }
}

fn close_implicit(stack: &mut Vec<Frame>) {
    if matches!(stack.last(), Some(Frame::Para { implicit: true, .. })) {
        pop_into_parent(stack);
    }
}

fn push_block(stack: &mut [Frame], block: Block) {
    match stack.last_mut() {
        Some(Frame::Root(b) | Frame::Quote(b) | Frame::Item(b)) => b.push(block),
        Some(Frame::List { items, .. }) => items.push(vec![block]),
        _ => {}
    }
}

fn pop_into_parent(stack: &mut Vec<Frame>) {
    let Some(frame) = stack.pop() else { return };
    let block = match frame {
        Frame::Para { spans, .. } => {
            let spans = trim(spans);
            if spans.is_empty() {
                return;
            }
            Block::Paragraph { spans }
        }
        Frame::Heading(level, spans) => Block::Heading { level, spans: trim(spans) },
        Frame::Quote(blocks) => Block::Quote { depth: quote_depth(stack) + 1, blocks },
        Frame::Code(text) => Block::Code { text: text.trim_end_matches('\n').to_owned() },
        Frame::List { ordered, start, items } => Block::List { ordered, start, items },
        Frame::Item(blocks) => {
            if let Some(Frame::List { items, .. }) = stack.last_mut() {
                items.push(blocks);
            }
            return;
        }
        Frame::Table { rows, header_rows } => Block::Table { rows, header_rows },
        Frame::Root(b) => {
            stack.push(Frame::Root(b));
            return;
        }
        Frame::Row(..) | Frame::Cell(..) => return,
    };
    push_block(stack, block);
}

fn quote_depth(stack: &[Frame]) -> u8 {
    stack.iter().filter(|f| matches!(f, Frame::Quote(_))).count().min(255) as u8
}

// ---- Blocks to HTML ----

/// Blocks to an HTML fragment. Everything is escaped; only safe link targets become `<a>`.
pub fn blocks_to_html(blocks: &[Block]) -> String {
    let mut out = String::new();
    for b in blocks {
        block_html(b, &mut out);
    }
    out
}

fn block_html(b: &Block, out: &mut String) {
    match b {
        Block::Paragraph { spans } => {
            out.push_str("<p>");
            spans_html(spans, out);
            out.push_str("</p>");
        }
        Block::Heading { level, spans } => {
            let l = (*level).clamp(1, 6);
            out.push_str(&format!("<h{l}>"));
            spans_html(spans, out);
            out.push_str(&format!("</h{l}>"));
        }
        Block::List { ordered, start, items } => {
            match (*ordered, *start) {
                (true, 1) => out.push_str("<ol>"),
                (true, n) => out.push_str(&format!("<ol start=\"{n}\">")),
                (false, _) => out.push_str("<ul>"),
            }
            for item in items {
                out.push_str("<li>");
                match item.as_slice() {
                    [Block::Paragraph { spans }] => spans_html(spans, out),
                    blocks => out.push_str(&blocks_to_html(blocks)),
                }
                out.push_str("</li>");
            }
            out.push_str(if *ordered { "</ol>" } else { "</ul>" });
        }
        Block::Quote { blocks, .. } => {
            out.push_str("<blockquote style=\"margin:0 0 0 .8ex;border-left:1px solid #ccc;padding-left:1ex\">");
            out.push_str(&blocks_to_html(blocks));
            out.push_str("</blockquote>");
        }
        Block::Code { text } => {
            out.push_str("<pre><code>");
            escape_text(text, out);
            out.push_str("</code></pre>");
        }
        Block::Table { rows, header_rows } => {
            out.push_str("<table border=\"1\" cellpadding=\"4\" cellspacing=\"0\" style=\"border-collapse:collapse\">");
            for (i, row) in rows.iter().enumerate() {
                out.push_str("<tr>");
                for c in row {
                    let tag = if c.header || (i as u32) < *header_rows { "th" } else { "td" };
                    if c.colspan > 1 {
                        out.push_str(&format!("<{tag} colspan=\"{}\">", c.colspan.min(64)));
                    } else {
                        out.push_str(&format!("<{tag}>"));
                    }
                    spans_html(&c.spans, out);
                    out.push_str(&format!("</{tag}>"));
                }
                out.push_str("</tr>");
            }
            out.push_str("</table>");
        }
        Block::Image(Image { source, alt, width, height, link }) => {
            let src = match source {
                ImageSource::Remote { url } => links::safe_href(url),
                ImageSource::Data { uri } => Some(uri.clone()),
                ImageSource::Cid { content_id, .. } => Some(format!("cid:{content_id}")),
                ImageSource::Blocked { .. } | ImageSource::Unavailable => None,
            };
            let Some(src) = src else {
                if let Some(alt) = alt {
                    escape_text(alt, out);
                }
                return;
            };
            if let Some(l) = link {
                out.push_str("<a href=\"");
                escape_attr(&l.href, out);
                out.push_str("\">");
            }
            out.push_str("<img src=\"");
            escape_attr(&src, out);
            out.push('"');
            if let Some(alt) = alt {
                out.push_str(" alt=\"");
                escape_attr(alt, out);
                out.push('"');
            }
            if let Some(w) = width {
                out.push_str(&format!(" width=\"{w}\""));
            }
            if let Some(h) = height {
                out.push_str(&format!(" height=\"{h}\""));
            }
            out.push('>');
            if link.is_some() {
                out.push_str("</a>");
            }
        }
        Block::Rule => out.push_str("<hr>"),
    }
}

fn spans_html(spans: &[Span], out: &mut String) {
    for s in spans {
        let href = s.link.as_ref().and_then(|l| links::safe_href(&l.href));
        if let Some(h) = &href {
            out.push_str("<a href=\"");
            escape_attr(h, out);
            out.push_str("\">");
        }
        let tags: Vec<&str> = [(s.bold, "strong"), (s.italic, "em"), (s.strike, "s"), (s.code, "code")]
            .into_iter()
            .filter(|(on, _)| *on)
            .map(|(_, t)| t)
            .collect();
        for t in &tags {
            out.push_str(&format!("<{t}>"));
        }
        let mut first = true;
        for line in s.text.split('\n') {
            if !first {
                out.push_str("<br>");
            }
            first = false;
            escape_text(line, out);
        }
        for t in tags.iter().rev() {
            out.push_str(&format!("</{t}>"));
        }
        if href.is_some() {
            out.push_str("</a>");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_to_parts() {
        let o = compose("Hi **Ada**,\n\n- one\n- [two](https://example.com/2)\n\n<script>alert(1)</script>", None);
        assert!(o.html.contains("<strong>Ada</strong>"));
        assert!(o.html.contains("<a href=\"https://example.com/2\">two</a>"));
        assert!(!o.html.contains("<script>"));
        assert!(o.html.contains("&lt;script&gt;"));
        assert!(o.text.contains("- two <https://example.com/2>"));
    }

    #[test]
    fn javascript_links_in_drafts_are_text() {
        let o = compose("[click](javascript:alert(1))", None);
        assert!(!o.html.contains("javascript"));
        assert!(o.html.contains("click"));
    }

    #[test]
    fn dates() {
        assert_eq!(format_date(0, 0), "Thu, Jan 1, 1970 at 12:00 AM");
        assert_eq!(format_date(1_794_323_040, 0), "Tue, Nov 10, 2026 at 3:04 PM");
        assert_eq!(format_date(1_794_323_040, -5 * 3600), "Tue, Nov 10, 2026 at 10:04 AM");
    }

    #[test]
    fn subjects() {
        assert_eq!(subject_for(QuoteKind::Reply, "Lunch"), "Re: Lunch");
        assert_eq!(subject_for(QuoteKind::Reply, "RE: Lunch"), "RE: Lunch");
        assert_eq!(subject_for(QuoteKind::Forward, "Lunch"), "Fwd: Lunch");
    }
}
