//! The native block tree: what the GPUI reader paints instead of a browser.
//!
//! The tree is plain data (serde, `#[serde(tag = "type")]`), carries no colours, fonts or sizes
//! except image dimensions, and contains only vetted link targets. Layout tables (the way marketing
//! mail is built) are linearised into paragraphs and images; only small grids of text become
//! [`Block::Table`].

use crate::clean::{self, Report};
use crate::dom::{self, Element, Node, is_block_tag, is_invisible};
use crate::fold::{self, Parts};
use crate::links::{self, WarningKind};
use crate::{Policy, charset, cut, style};
use lumbridge_mail_core::{Attachment, AttachmentId, Body};
use serde::{Deserialize, Serialize};

/// A rendered message: the new content, then (folded away by default) signature and quoted history.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Document {
    pub blocks: Vec<Block>,
    /// Empty when there is none or folding is off.
    pub signature: Vec<Block>,
    /// The quoted history, starting with its attribution line. Empty when none or folding is off.
    pub quoted: Vec<Block>,
    pub report: Report,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    /// Running text. Span text may contain `\n` for hard line breaks.
    Paragraph { spans: Vec<Span> },
    /// `level` 1..=6.
    Heading { level: u8, spans: Vec<Span> },
    /// Each item is its own list of blocks (usually one paragraph, maybe a nested list).
    List { ordered: bool, start: u32, items: Vec<Vec<Block>> },
    /// Quoted text. `depth` is 1 for a top-level quote, 2 for a quote inside it, ... (nested quotes
    /// are `Quote` blocks inside `blocks`).
    Quote { depth: u8, blocks: Vec<Block> },
    /// Preformatted text, whitespace exact.
    Code { text: String },
    /// A small grid of text. Rows can have different lengths; `colspan` widens a cell.
    Table { rows: Vec<Vec<Cell>>, header_rows: u32 },
    Image(Image),
    /// A horizontal divider.
    Rule,
}

/// A run of text with one style.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Span {
    pub text: String,
    #[serde(skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub italic: bool,
    /// Monospace.
    #[serde(skip_serializing_if = "is_false")]
    pub code: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub strike: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<Link>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Span {
    pub fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Self::default() }
    }
    fn same_style(&self, o: &Span) -> bool {
        self.bold == o.bold && self.italic == o.italic && self.code == o.code && self.strike == o.strike && self.link == o.link
    }
}

/// A followable link: `href` is always `http(s):`, `mailto:` or `tel:`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub href: String,
    /// Set when the link looks deceptive; the reader should show the real destination before opening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<WarningKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Cell {
    pub spans: Vec<Span>,
    #[serde(skip_serializing_if = "is_false")]
    pub header: bool,
    /// At least 1.
    pub colspan: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub source: ImageSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
    /// Intrinsic size hints from the mail, in CSS px.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// The image is a link (a banner or a button).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<Link>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageSource {
    /// An inline part of this message. `attachment` is None when no part has this content id.
    Cid { content_id: String, attachment: Option<AttachmentId> },
    /// Remote content was allowed: load this (http/https) URL.
    Remote { url: String },
    /// A remote image not loaded because of the policy. Paint a placeholder (alt text, size).
    Blocked { host: String },
    /// A small inline `data:image/(png|gif|jpeg|webp);base64,` URI.
    Data { uri: String },
    /// Nothing to load (relative or unsafe source); paint the alt text if any.
    Unavailable,
}

/// Render a body as blocks: the HTML part if there is one, else the text part.
pub fn blocks(body: &Body, policy: &Policy) -> Document {
    match body.html.as_deref() {
        Some(h) if !h.trim().is_empty() => blocks_from_html(h, &body.attachments, policy),
        _ => blocks_from_text(body.text.as_deref().unwrap_or(""), policy),
    }
}

pub fn blocks_from_html(html: &str, attachments: &[Attachment], policy: &Policy) -> Document {
    let (html, cut_off) = cut(html, policy.limits.max_input_bytes);
    let html = charset::repair(html);
    let parsed = dom::parse(&html, policy.limits.max_depth, policy.limits.max_nodes);
    let cleaned = clean::clean(parsed.root, policy, attachments);
    let mut report = cleaned.report;
    let parts = if policy.fold {
        fold::split_html(cleaned.body)
    } else {
        Parts { new: cleaned.body.children, signature: Vec::new(), quoted: Vec::new() }
    };
    let mut budget = Budget::new(policy);
    let blocks = build(&parts.new, &mut budget);
    let signature = build(&parts.signature, &mut budget);
    let quoted = build(&parts.quoted, &mut budget);
    report.truncated |= cut_off || parsed.truncated || budget.truncated;
    Document { blocks, signature, quoted, report }
}

pub fn blocks_from_text(text: &str, policy: &Policy) -> Document {
    let (text, cut_off) = cut(text, policy.limits.max_input_bytes);
    // CRLF, and bare CR (old Mac line ends, which some clients still write): both are line breaks.
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let text = charset::repair(&text);
    let mut budget = Budget::new(policy);
    let mut report = Report { truncated: cut_off, ..Report::default() };
    let (blocks, signature, quoted) = if policy.fold {
        let f = fold::fold_text(&text);
        (
            text_blocks(&f.body, 0, &mut budget),
            f.signature.map(|s| text_blocks(&s, 0, &mut budget)).unwrap_or_default(),
            f.quoted.map(|s| text_blocks(&s, 0, &mut budget)).unwrap_or_default(),
        )
    } else {
        (text_blocks(&text, 0, &mut budget), Vec::new(), Vec::new())
    };
    report.truncated |= budget.truncated;
    Document { blocks, signature, quoted, report }
}

pub(crate) struct Budget {
    blocks: usize,
    chars: usize,
    pub truncated: bool,
}

impl Budget {
    pub(crate) fn new(policy: &Policy) -> Self {
        Self { blocks: policy.limits.max_blocks, chars: policy.limits.max_text_chars, truncated: false }
    }
    pub(crate) fn unlimited() -> Self {
        Self { blocks: usize::MAX, chars: usize::MAX, truncated: false }
    }
    fn spent(&self) -> bool {
        self.blocks == 0 || self.chars == 0
    }
    fn take_block(&mut self) -> bool {
        if self.blocks == 0 {
            self.truncated = true;
            return false;
        }
        self.blocks -= 1;
        true
    }
}

#[derive(Clone, Default)]
struct Style {
    bold: bool,
    italic: bool,
    code: bool,
    strike: bool,
    link: Option<Link>,
    quote_depth: u8,
    /// Inside `white-space: pre*`: a newline in the text is a line break.
    pre_lines: bool,
}

struct Builder<'b> {
    out: Vec<Block>,
    inline: Vec<Span>,
    budget: &'b mut Budget,
}

pub(crate) fn build(nodes: &[Node], budget: &mut Budget) -> Vec<Block> {
    let mut b = Builder { out: Vec::new(), inline: Vec::new(), budget };
    b.walk(nodes, &Style::default());
    b.flush();
    b.finish()
}

impl<'b> Builder<'b> {
    fn sub(&mut self) -> Builder<'_> {
        Builder { out: Vec::new(), inline: Vec::new(), budget: self.budget }
    }

    fn finish(mut self) -> Vec<Block> {
        // No rule at the very start or end, and never two in a row.
        while matches!(self.out.last(), Some(Block::Rule)) {
            self.out.pop();
        }
        self.out
    }

    fn push(&mut self, block: Block) {
        if matches!(block, Block::Rule) && matches!(self.out.last(), None | Some(Block::Rule)) {
            return;
        }
        if self.budget.take_block() {
            self.out.push(block);
        }
    }

    fn push_text(&mut self, text: &str, st: &Style) {
        if st.pre_lines && text.contains(['\n', '\r']) {
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            for (i, line) in text.split('\n').enumerate() {
                if i > 0 {
                    self.line_break_kept(st);
                }
                self.push_text_collapsed(line, st);
            }
            return;
        }
        self.push_text_collapsed(text, st)
    }

    /// A line break that is kept even after another (blank lines in pre-wrapped text).
    fn line_break_kept(&mut self, st: &Style) {
        if let Some(last) = self.inline.last_mut() {
            let trimmed = last.text.trim_end_matches(' ').len();
            last.text.truncate(trimmed);
        }
        let mut s = Span::plain("\n");
        s.link = st.link.clone();
        self.push_span(s);
    }

    fn push_text_collapsed(&mut self, text: &str, st: &Style) {
        if self.budget.chars == 0 {
            self.budget.truncated = true;
            return;
        }
        let mut norm = String::with_capacity(text.len());
        let mut space = self.inline.last().is_none_or(|s| s.text.ends_with([' ', '\n']));
        for ch in text.chars() {
            if is_invisible(ch) {
                continue;
            }
            if ch.is_whitespace() {
                if !space {
                    norm.push(' ');
                    space = true;
                }
            } else {
                norm.push(ch);
                space = false;
            }
        }
        if norm.is_empty() {
            return;
        }
        let n = norm.chars().count();
        if n > self.budget.chars {
            norm = norm.chars().take(self.budget.chars).collect();
            self.budget.truncated = true;
            self.budget.chars = 0;
        } else {
            self.budget.chars -= n;
        }
        self.push_span(Span { text: norm, bold: st.bold, italic: st.italic, code: st.code, strike: st.strike, link: st.link.clone() });
    }

    fn push_span(&mut self, span: Span) {
        if let Some(last) = self.inline.last_mut()
            && last.same_style(&span) {
                last.text.push_str(&span.text);
                return;
            }
        self.inline.push(span);
    }

    fn line_break(&mut self, st: &Style) {
        if let Some(last) = self.inline.last_mut() {
            let trimmed = last.text.trim_end_matches(' ').len();
            last.text.truncate(trimmed);
        }
        if !self.inline.is_empty() {
            let mut s = Span::plain("\n");
            s.link = st.link.clone();
            s.bold = st.bold;
            s.italic = st.italic;
            self.push_span(s);
        }
    }

    fn take_spans(&mut self) -> Vec<Span> {
        tidy_spans(std::mem::take(&mut self.inline))
    }

    fn flush(&mut self) {
        let spans = self.take_spans();
        if !spans.is_empty() {
            self.push(Block::Paragraph { spans });
        }
    }

    fn walk(&mut self, nodes: &[Node], st: &Style) {
        for n in nodes {
            if self.budget.spent() {
                self.budget.truncated = true;
                return;
            }
            match n {
                Node::Text(t) => self.push_text(t, st),
                Node::Element(e) => self.element(e, st),
            }
        }
    }

    fn element(&mut self, e: &Element, st: &Style) {
        let mut st2 = st.clone();
        let (bold, italic) = style::emphasis(e);
        st2.bold |= bold;
        st2.italic |= italic;
        if let Some(pre) = style::keeps_line_breaks(e) {
            st2.pre_lines = pre;
        }
        match e.tag.as_str() {
            "br" => self.line_break(st),
            "wbr" | "col" | "colgroup" => {}
            "b" | "strong" => {
                st2.bold = true;
                self.walk(&e.children, &st2);
            }
            "i" | "em" | "cite" | "var" => {
                st2.italic = true;
                self.walk(&e.children, &st2);
            }
            "code" | "kbd" | "samp" | "tt" => {
                st2.code = true;
                self.walk(&e.children, &st2);
            }
            "s" | "strike" | "del" => {
                st2.strike = true;
                self.walk(&e.children, &st2);
            }
            "a" => {
                if let Some(href) = e.attr("href") {
                    let text: String = e.text(400);
                    let warning = links::link_warning(&text, href).map(|w| w.kind);
                    st2.link = Some(Link { href: href.to_owned(), warning });
                }
                self.walk(&e.children, &st2);
            }
            "img" => self.image(e, st),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = e.tag[1..].parse().unwrap_or(2);
                self.heading(e, level, &st2);
            }
            "ul" | "ol" => {
                self.flush();
                self.list(e, &st2);
            }
            "blockquote" => {
                self.flush();
                let mut inner = st2.clone();
                inner.quote_depth = st.quote_depth.saturating_add(1);
                let blocks = self.sub_blocks(&e.children, &inner);
                if !blocks.is_empty() {
                    self.push(Block::Quote { depth: inner.quote_depth, blocks });
                }
            }
            "pre" => {
                self.flush();
                let mut text = String::new();
                raw_text(&e.children, &mut text);
                let text = text.trim_matches('\n').to_owned();
                if !text.trim().is_empty() {
                    let n = text.chars().count();
                    self.budget.chars = self.budget.chars.saturating_sub(n);
                    self.push(Block::Code { text });
                }
            }
            "hr" => {
                self.flush();
                self.push(Block::Rule);
            }
            "table" => {
                self.flush();
                match data_table(e) {
                    Some((rows, header_rows)) => self.push(Block::Table { rows, header_rows }),
                    None => self.walk_blocks(&e.children, &st2),
                }
                self.flush();
            }
            "dt" => {
                self.flush();
                st2.bold = true;
                self.walk(&e.children, &st2);
                self.flush();
            }
            tag if is_block_tag(tag) => {
                if let Some(level) = promoted_heading(e) {
                    self.heading(e, level, &st2);
                    return;
                }
                self.walk_blocks(&e.children, &st2);
            }
            _ => self.walk(&e.children, &st2),
        }
    }

    fn walk_blocks(&mut self, children: &[Node], st: &Style) {
        self.flush();
        self.walk(children, st);
        self.flush();
    }

    fn heading(&mut self, e: &Element, level: u8, st: &Style) {
        self.flush();
        let spans = {
            let mut sub = self.sub();
            sub.walk(&e.children, st);
            // A heading holding an image (a banner) keeps the image as its own block.
            let spans = sub.take_spans();
            let extra = sub.finish();
            (spans, extra)
        };
        for b in spans.1 {
            self.push(b);
        }
        if !spans.0.is_empty() {
            let spans = spans.0.into_iter().map(|mut s| { s.bold = false; s }).collect::<Vec<_>>();
            self.push(Block::Heading { level: level.clamp(1, 6), spans: tidy_spans(spans) });
        }
    }

    fn list(&mut self, e: &Element, st: &Style) {
        let ordered = e.tag == "ol";
        let start = e.attr("start").and_then(|s| s.trim().parse().ok()).unwrap_or(1);
        let mut items: Vec<Vec<Block>> = Vec::new();
        let mut stray: Vec<Node> = Vec::new();
        for child in &e.children {
            match child {
                Node::Element(li) if li.tag == "li" => {
                    self.stray_into(&mut stray, &mut items, st);
                    let blocks = self.sub_blocks(&li.children, st);
                    items.push(blocks);
                }
                other => stray.push(other.clone()),
            }
        }
        self.stray_into(&mut stray, &mut items, st);
        items.retain(|i| !i.is_empty());
        if !items.is_empty() {
            self.push(Block::List { ordered, start, items });
        }
    }

    fn sub_blocks(&mut self, nodes: &[Node], st: &Style) -> Vec<Block> {
        let mut sub = self.sub();
        sub.walk(nodes, st);
        sub.flush();
        sub.finish()
    }

    /// Text or elements directly inside a list but outside any `<li>` join the previous item.
    fn stray_into(&mut self, stray: &mut Vec<Node>, items: &mut Vec<Vec<Block>>, st: &Style) {
        if stray.is_empty() {
            return;
        }
        let blocks = self.sub_blocks(stray, st);
        stray.clear();
        if blocks.is_empty() {
            return;
        }
        match items.last_mut() {
            Some(last) => last.extend(blocks),
            None => items.push(blocks),
        }
    }

    fn image(&mut self, e: &Element, st: &Style) {
        let width = e.attr("width").and_then(|w| w.parse().ok());
        let height = e.attr("height").and_then(|h| h.parse().ok());
        let alt = e.attr("alt").map(str::to_owned);
        // Small icons (social buttons, emoji images) read better as their alt text, inline.
        if matches!((width, height), (Some(w), Some(h)) if w <= 32 && h <= 32) {
            if let Some(alt) = &alt {
                let plain = Style { link: None, quote_depth: st.quote_depth, ..Style::default() };
                self.push_text(" ", &plain);
                self.push_text(alt, st);
                self.push_text(" ", &plain);
            }
            return;
        }
        let source = match e.attr("data-lbm-kind") {
            Some("cid") => ImageSource::Cid {
                content_id: e.attr("data-lbm-cid").unwrap_or_default().to_owned(),
                attachment: e.attr("data-lbm-att").map(AttachmentId::new),
            },
            Some("missing") if e.attr("data-lbm-cid").is_some() => {
                ImageSource::Cid { content_id: e.attr("data-lbm-cid").unwrap_or_default().to_owned(), attachment: None }
            }
            Some("remote") => ImageSource::Remote { url: e.attr("src").unwrap_or_default().to_owned() },
            Some("blocked") => ImageSource::Blocked { host: e.attr("data-lbm-host").unwrap_or_default().to_owned() },
            Some("data") => ImageSource::Data { uri: e.attr("src").unwrap_or_default().to_owned() },
            _ => ImageSource::Unavailable,
        };
        if matches!(source, ImageSource::Unavailable) && alt.is_none() {
            return;
        }
        self.flush();
        self.push(Block::Image(Image { source, alt, width, height, link: st.link.clone() }));
    }
}

/// Trim the paragraph's ends, drop empty spans, and cap blank lines.
fn tidy_spans(mut spans: Vec<Span>) -> Vec<Span> {
    for s in &mut spans {
        while s.text.contains("\n\n\n") {
            s.text = s.text.replace("\n\n\n", "\n\n");
        }
        if s.text.contains(" \n") || s.text.contains("\n ") {
            s.text = s.text.replace(" \n", "\n").replace("\n ", "\n");
        }
    }
    while let Some(first) = spans.first_mut() {
        let t = first.text.trim_start().to_owned();
        if t.is_empty() {
            spans.remove(0);
        } else {
            first.text = t;
            break;
        }
    }
    while let Some(last) = spans.last_mut() {
        let t = last.text.trim_end().to_owned();
        if t.is_empty() {
            spans.pop();
        } else {
            last.text = t;
            break;
        }
    }
    spans.retain(|s| !s.text.is_empty());
    spans
}

fn raw_text(nodes: &[Node], out: &mut String) {
    for n in nodes {
        match n {
            Node::Text(t) => out.extend(t.chars().filter(|c| !is_invisible(*c) || *c == '\t')),
            Node::Element(e) if e.tag == "br" => out.push('\n'),
            Node::Element(e) => {
                raw_text(&e.children, out);
                if matches!(e.tag.as_str(), "p" | "div" | "li" | "tr") {
                    out.push('\n');
                }
            }
        }
    }
}

/// Marketing mail sets headings with a big font-size on a cell or div instead of `<h1>`.
fn promoted_heading(e: &Element) -> Option<u8> {
    let size = style::font_size_px(e)?;
    if size < 22.0 {
        return None;
    }
    let text = e.text(300);
    if text.is_empty() || text.chars().count() > 140 || has_block_descendant(e) {
        return None;
    }
    Some(if size >= 28.0 { 1 } else { 2 })
}

fn has_block_descendant(e: &Element) -> bool {
    e.children.iter().any(|n| match n {
        Node::Element(c) => is_block_tag(&c.tag) || c.tag == "img" || has_block_descendant(c),
        Node::Text(_) => false,
    })
}

fn elements(nodes: &[Node]) -> impl Iterator<Item = &Element> {
    nodes.iter().filter_map(|n| if let Node::Element(e) = n { Some(e) } else { None })
}

/// A table that is data (a grid of short text), not layout. Layout tables return None and are
/// linearised by the caller.
fn data_table(t: &Element) -> Option<(Vec<Vec<Cell>>, u32)> {
    if t.attr("role").is_some_and(|r| r.eq_ignore_ascii_case("presentation") || r.eq_ignore_ascii_case("none")) {
        return None;
    }
    let mut rows: Vec<(&Element, bool)> = Vec::new();
    for c in elements(&t.children) {
        match c.tag.as_str() {
            "tr" => rows.push((c, false)),
            "thead" | "tbody" | "tfoot" => {
                rows.extend(elements(&c.children).filter(|r| r.tag == "tr").map(|r| (r, c.tag == "thead")))
            }
            _ => {}
        }
    }
    if rows.len() < 2 || rows.len() > 300 {
        return None;
    }
    let mut grid = Vec::with_capacity(rows.len());
    let mut max_cols = 0u32;
    let mut header_rows = 0u32;
    let mut leading_header = true;
    for (row, in_thead) in rows {
        let cells: Vec<&Element> = elements(&row.children).filter(|c| c.tag == "td" || c.tag == "th").collect();
        let mut out = Vec::with_capacity(cells.len());
        let mut cols = 0u32;
        let mut all_th = !cells.is_empty();
        for c in cells {
            if layout_cell(c, 0) {
                return None;
            }
            let colspan = c.attr("colspan").and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(1).clamp(1, 64);
            cols += colspan;
            let header = c.tag == "th" || in_thead;
            all_th &= header;
            let mut budget = Budget::unlimited();
            let mut b = Builder { out: Vec::new(), inline: Vec::new(), budget: &mut budget };
            b.walk_cell(&c.children);
            let spans = b.take_spans();
            if spans.iter().map(|s| s.text.chars().count()).sum::<usize>() > 300 {
                return None;
            }
            out.push(Cell { spans, header, colspan });
        }
        if out.is_empty() {
            continue;
        }
        if leading_header && all_th {
            header_rows += 1;
        } else {
            leading_header = false;
        }
        max_cols = max_cols.max(cols);
        grid.push(out);
    }
    if !(2..=12).contains(&max_cols) || grid.len() < 2 {
        return None;
    }
    // A grid where every row is a single cell is layout too.
    if grid.iter().filter(|r| r.len() >= 2).count() * 2 < grid.len() {
        return None;
    }
    // Mostly-empty grids (spacers) are layout.
    let filled = grid.iter().flatten().filter(|c| !c.spans.is_empty()).count();
    let total = grid.iter().map(Vec::len).sum::<usize>();
    if filled * 2 < total {
        return None;
    }
    Some((grid, header_rows))
}

fn layout_cell(c: &Element, depth: usize) -> bool {
    if depth > 16 {
        return true;
    }
    elements(&c.children).any(|e| {
        matches!(e.tag.as_str(), "table" | "img" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol" | "blockquote" | "pre" | "hr")
            || layout_cell(e, depth + 1)
    })
}

impl Builder<'_> {
    /// Inline content of a table cell: block elements become line breaks.
    fn walk_cell(&mut self, nodes: &[Node]) {
        let st = Style::default();
        for n in nodes {
            match n {
                Node::Text(t) => self.push_text(t, &st),
                Node::Element(e) if is_block_tag(&e.tag) && !matches!(e.tag.as_str(), "td" | "th") => {
                    self.line_break(&st);
                    self.walk_cell(&e.children);
                    self.line_break(&st);
                }
                Node::Element(e) => self.element(e, &st),
            }
        }
    }
}

/// Plain text to blocks: paragraphs at blank lines, `>` quotes by depth, `-`/`*`/`1.` lists,
/// ``` fences as code, and bare URLs as links.
pub(crate) fn text_blocks(text: &str, depth: u8, budget: &mut Budget) -> Vec<Block> {
    let mut out = Vec::new();
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0;
    let mut para: Vec<&str> = Vec::new();
    let flush = |para: &mut Vec<&str>, out: &mut Vec<Block>, budget: &mut Budget| {
        if para.is_empty() {
            return;
        }
        let joined = para.join("\n");
        para.clear();
        let spans = linkify(&joined, budget);
        if !spans.is_empty() && budget.take_block() {
            out.push(Block::Paragraph { spans });
        }
    };
    while i < lines.len() {
        if budget.spent() {
            budget.truncated = true;
            break;
        }
        let line = lines[i];
        let t = line.trim_start();
        if t.starts_with('>') {
            flush(&mut para, &mut out, budget);
            let mut inner = Vec::new();
            while i < lines.len() && (lines[i].trim_start().starts_with('>') || (lines[i].trim().is_empty() && lines.get(i + 1).is_some_and(|n| n.trim_start().starts_with('>')))) {
                let l = lines[i].trim_start();
                let l = l.strip_prefix('>').unwrap_or(l);
                inner.push(l.strip_prefix(' ').unwrap_or(l));
                i += 1;
            }
            let d = depth.saturating_add(1);
            let blocks = text_blocks(&inner.join("\n"), d, budget);
            if !blocks.is_empty() && budget.take_block() {
                out.push(Block::Quote { depth: d, blocks });
            }
            continue;
        }
        if t.starts_with("```") {
            flush(&mut para, &mut out, budget);
            let mut code = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                code.push(lines[i]);
                i += 1;
            }
            i += 1;
            if budget.take_block() {
                out.push(Block::Code { text: code.join("\n") });
            }
            continue;
        }
        if let Some((ordered, n, _)) = list_marker(line) {
            flush(&mut para, &mut out, budget);
            let mut items = Vec::new();
            while i < lines.len() {
                match list_marker(lines[i]) {
                    Some((o, _, rest)) if o == ordered => {
                        let spans = linkify(rest, budget);
                        items.push(vec![Block::Paragraph { spans }]);
                        i += 1;
                    }
                    _ => break,
                }
            }
            if budget.take_block() {
                out.push(Block::List { ordered, start: n, items });
            }
            continue;
        }
        if line.trim().is_empty() {
            flush(&mut para, &mut out, budget);
        } else {
            para.push(line);
        }
        i += 1;
    }
    flush(&mut para, &mut out, budget);
    out
}

fn list_marker(line: &str) -> Option<(bool, u32, &str)> {
    let t = line.trim_start();
    if line.len() - t.len() > 3 {
        return None;
    }
    for m in ["- ", "* ", "• "] {
        if let Some(rest) = t.strip_prefix(m) {
            return (!rest.trim().is_empty()).then_some((false, 1, rest));
        }
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if (1..=3).contains(&digits) {
        let rest = &t[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return (!rest.trim().is_empty()).then(|| (true, t[..digits].parse().unwrap_or(1), rest));
        }
    }
    None
}

/// Text with bare `http(s)://` and `www.` URLs turned into link spans.
pub(crate) fn linkify(text: &str, budget: &mut Budget) -> Vec<Span> {
    let clean: String = text.chars().filter(|c| !is_invisible(*c) || *c == '\n' || *c == '\t').collect();
    let n = clean.chars().count();
    let clean = if n > budget.chars {
        budget.truncated = true;
        clean.chars().take(budget.chars).collect()
    } else {
        clean
    };
    budget.chars = budget.chars.saturating_sub(n);
    let mut spans: Vec<Span> = Vec::new();
    let mut rest = clean.as_str();
    while let Some((start, end)) = find_url(rest) {
        if start > 0 {
            spans.push(Span::plain(&rest[..start]));
        }
        let raw = &rest[start..end];
        let target = if raw.to_ascii_lowercase().starts_with("www.") { format!("https://{raw}") } else { raw.to_owned() };
        match links::safe_href(&target) {
            Some(href) => spans.push(Span { text: raw.to_owned(), link: Some(Link { href, warning: None }), ..Span::default() }),
            None => spans.push(Span::plain(raw)),
        }
        rest = &rest[end..];
    }
    if !rest.is_empty() {
        spans.push(Span::plain(rest));
    }
    // Merge plain neighbours.
    let mut merged: Vec<Span> = Vec::new();
    for s in spans {
        match merged.last_mut() {
            Some(l) if l.same_style(&s) => l.text.push_str(&s.text),
            _ => merged.push(s),
        }
    }
    tidy_spans(merged)
}

/// Byte range of the first URL in `s`.
pub(crate) fn find_url(s: &str) -> Option<(usize, usize)> {
    // One forward pass (no lower-casing of the whole rest: callers loop over long texts).
    let b = s.as_bytes();
    let starts_ci = |at: usize, p: &[u8]| b.len() >= at + p.len() && b[at..at + p.len()].eq_ignore_ascii_case(p);
    let start = (0..b.len()).find(|&at| match b[at] {
        b'h' | b'H' => starts_ci(at, b"https://") || starts_ci(at, b"http://"),
        // `www.` must start a word.
        b'w' | b'W' => starts_ci(at, b"www.") && (at == 0 || !(b[at - 1].is_ascii_alphanumeric() || b[at - 1] == b'.' || b[at - 1] == b'/' || b[at - 1] >= 0x80)),
        _ => false,
    })?;
    let tail = &s[start..];
    let mut end = tail.find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`' | '\u{A0}')).unwrap_or(tail.len());
    // Trailing punctuation is the sentence's, not the URL's (keep a ')' when it closes a '(').
    while let Some(last) = tail[..end].chars().last() {
        let opens = tail[..end].matches('(').count();
        let closes = tail[..end].matches(')').count();
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | ']' | '*') || (last == ')' && closes > opens) {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    let min = if tail.as_bytes()[..4.min(tail.len())].eq_ignore_ascii_case(b"www.") { 5 } else { 9 };
    if end < min {
        return None;
    }
    Some((start, start + end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(h: &str) -> Document {
        blocks_from_html(h, &[], &Policy::default())
    }

    fn texts(d: &Document) -> Vec<String> {
        d.blocks
            .iter()
            .map(|b| match b {
                Block::Paragraph { spans } => spans.iter().map(|s| s.text.as_str()).collect(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn line_breaks_in_drafts_survive() {
        // Plain text wrapped in one element with `white-space: pre-wrap`: its newlines are line breaks.
        let d = html("<div style=\"white-space: pre-wrap\">Hi Maya,\n\nSaturday works.\nBringing dessert.</div>");
        assert_eq!(texts(&d), ["Hi Maya,\n\nSaturday works.\nBringing dessert."]);
        // Without it, newlines in HTML text are spaces, as in a browser.
        assert_eq!(texts(&html("<div>Hi\nthere</div>")), ["Hi there"]);
        // A child can turn it off again.
        let d = html("<div style=\"white-space:pre\">a\nb<span style=\"white-space:normal\">c\nd</span></div>");
        assert_eq!(texts(&d), ["a\nbc d"]);
        // Plain text with bare CR line ends (old Mac style) or CRLF keeps its lines.
        for t in ["Hi Maya,\r\rSaturday works.\rBringing dessert.", "Hi Maya,\r\n\r\nSaturday works.\r\nBringing dessert."] {
            let d = blocks_from_text(t, &Policy::default());
            assert_eq!(texts(&d), ["Hi Maya,", "Saturday works.\nBringing dessert."], "{t:?}");
        }
    }

    #[test]
    fn paragraphs_and_inline_styles() {
        let d = html("<p>Hello <b>bold</b> and <a href='https://example.com/x'>a link</a>.</p><p>Two</p>");
        assert_eq!(d.blocks.len(), 2);
        let Block::Paragraph { spans } = &d.blocks[0] else { panic!() };
        assert_eq!(spans[0].text, "Hello ");
        assert!(spans[1].bold);
        assert_eq!(spans[3].link.as_ref().unwrap().href, "https://example.com/x");
    }

    #[test]
    fn nested_quotes_have_depth() {
        let d = blocks_from_html("<p>top</p><blockquote>one<blockquote>two</blockquote></blockquote>", &[], &Policy { fold: false, ..Policy::default() });
        let Block::Quote { depth, blocks } = &d.blocks[1] else { panic!("{:?}", d.blocks) };
        assert_eq!(*depth, 1);
        assert!(matches!(blocks[1], Block::Quote { depth: 2, .. }));
    }

    #[test]
    fn layout_tables_linearise_and_data_tables_stay() {
        let d = html("<table width=100%><tr><td><img src='https://cdn.example.com/hero.png' width=600 height=200 alt='Sale'></td></tr><tr><td><p>Big sale</p></td></tr></table>");
        assert!(matches!(d.blocks[0], Block::Image(Image { source: ImageSource::Blocked { .. }, .. })));
        assert!(matches!(d.blocks[1], Block::Paragraph { .. }));
        let d = html("<table border=1><tr><th>Item</th><th>Price</th></tr><tr><td>Tea</td><td>$3</td></tr></table>");
        let Block::Table { rows, header_rows } = &d.blocks[0] else { panic!("{:?}", d.blocks) };
        assert_eq!(*header_rows, 1);
        assert_eq!(rows[1][1].spans[0].text, "$3");
    }

    #[test]
    fn text_bodies() {
        let d = blocks_from_text("Hi,\nsee https://example.com/a.\n\n- one\n- two\n\n> quoted\n>> deeper", &Policy { fold: false, ..Policy::default() });
        let Block::Paragraph { spans } = &d.blocks[0] else { panic!() };
        assert_eq!(spans[1].text, "https://example.com/a");
        assert!(matches!(d.blocks[1], Block::List { ordered: false, .. }));
        let Block::Quote { blocks, .. } = &d.blocks[2] else { panic!() };
        assert!(matches!(blocks[1], Block::Quote { depth: 2, .. }));
    }

    #[test]
    fn urls_end_before_punctuation() {
        let s = "see (https://example.com/a_(b)) now.";
        let (a, b) = find_url(s).unwrap();
        assert_eq!(&s[a..b], "https://example.com/a_(b)");
        let s = "go to www.example.com.";
        let (a, b) = find_url(s).unwrap();
        assert_eq!(&s[a..b], "www.example.com");
    }

    #[test]
    fn wire_shape_is_tagged() {
        let d = html("<h2>Title</h2><hr><p>x</p>");
        let j = serde_json::to_value(&d.blocks).unwrap();
        assert_eq!(j[0]["type"], "heading");
        assert_eq!(j[0]["level"], 2);
        assert_eq!(j[1]["type"], "rule");
        let back: Vec<Block> = serde_json::from_value(j).unwrap();
        assert_eq!(back, d.blocks);
    }
}
