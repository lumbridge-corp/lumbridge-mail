//! A small owned HTML tree. html5ever does the parsing (the same parser browsers' spec describes);
//! we copy its DOM into this tree with a depth and node budget, so every later pass can recurse
//! without a stack-overflow risk and edit freely.

use html5ever::tendril::TendrilSink;
use html5ever::{ParseOpts, parse_document};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Element(Element),
    Text(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Element {
    /// Lower-case local name (`div`, `o:p`, `svg`).
    pub tag: String,
    /// Lower-case names, values as given.
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Element {
    pub fn new(tag: &str) -> Self {
        Self { tag: tag.to_owned(), attrs: Vec::new(), children: Vec::new() }
    }
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    pub fn set_attr(&mut self, name: &str, value: impl Into<String>) {
        let value = value.into();
        match self.attrs.iter_mut().find(|(k, _)| k == name) {
            Some(slot) => slot.1 = value,
            None => self.attrs.push((name.to_owned(), value)),
        }
    }
    pub fn remove_attr(&mut self, name: &str) -> Option<String> {
        let ix = self.attrs.iter().position(|(k, _)| k == name)?;
        Some(self.attrs.remove(ix).1)
    }
    pub fn has_class(&self, class: &str) -> bool {
        self.attr("class").is_some_and(|c| c.split_ascii_whitespace().any(|c| c.eq_ignore_ascii_case(class)))
    }
    pub fn id_is(&self, id: &str) -> bool {
        self.attr("id").is_some_and(|v| v.eq_ignore_ascii_case(id))
    }
    /// The element's text, whitespace-collapsed, up to `limit` chars.
    pub fn text(&self, limit: usize) -> String {
        let mut out = String::new();
        text_into(&self.children, &mut out, limit);
        collapse_ws(&out)
    }
}

fn text_into(nodes: &[Node], out: &mut String, limit: usize) {
    for n in nodes {
        if out.len() >= limit {
            return;
        }
        match n {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) => {
                if e.tag == "br" {
                    out.push('\n');
                } else {
                    text_into(&e.children, out, limit);
                    if is_block_tag(&e.tag) {
                        out.push('\n');
                    }
                }
            }
        }
    }
}

pub(crate) fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for ch in s.chars() {
        if ch.is_whitespace() || is_invisible(ch) {
            if !is_invisible(ch) {
                space = true;
            }
        } else {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push(ch);
        }
    }
    out
}

/// Characters newsletters use as invisible padding (preheader filler) or that carry no meaning.
pub(crate) fn is_invisible(ch: char) -> bool {
    matches!(ch, '\u{200B}'..='\u{200D}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}' | '\u{034F}' | '\u{00AD}'
        | '\u{180E}' | '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        || (ch.is_control() && !matches!(ch, '\n' | '\t' | '\r'))
}

pub(crate) fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "div" | "center" | "section" | "article" | "header" | "footer" | "main" | "aside" | "nav"
            | "address" | "figure" | "figcaption" | "details" | "summary" | "h1" | "h2" | "h3" | "h4" | "h5"
            | "h6" | "ul" | "ol" | "li" | "dl" | "dt" | "dd" | "blockquote" | "pre" | "table" | "thead"
            | "tbody" | "tfoot" | "tr" | "td" | "th" | "caption" | "hr" | "body" | "html" | "form" | "fieldset"
    )
}

pub(crate) struct Parsed {
    pub root: Element,
    pub truncated: bool,
}

/// Parse a whole HTML document. The result is the `<html>` element with `<head>` and `<body>`.
pub(crate) fn parse(html: &str, max_depth: usize, max_nodes: usize) -> Parsed {
    let (html, cut_deep) = guard_depth(html, MAX_OPEN);
    let dom: RcDom = parse_document(RcDom::default(), ParseOpts::default()).one(html);
    let mut budget = Budget { nodes: max_nodes, max_depth, truncated: cut_deep };
    let mut root = Element::new("html");
    let doc = dom.document.clone();
    for child in doc.children.borrow().iter() {
        if let NodeData::Element { name, .. } = &child.data
            && &*name.local == "html"
                && let Some(Node::Element(e)) = convert(child, 0, &mut budget) {
                    root = e;
                }
    }
    Parsed { root, truncated: budget.truncated }
}

/// The parser's open-element stack is scanned for most start tags, so tens of thousands of unclosed
/// nested elements make parsing quadratic. A cheap byte scan estimates nesting and cuts the input
/// where it passes this depth. Elements whose end tag is implied (`p`, `li`, `td`, ...) and void
/// elements don't count, so sloppy but ordinary mail never gets near it.
const MAX_OPEN: usize = 1_500;

const IMPLIED_END: &[&str] = &[
    "p", "li", "dt", "dd", "td", "th", "tr", "option", "optgroup", "rb", "rt", "rp", "thead", "tbody", "tfoot",
    "colgroup", "caption", "head", "body", "html",
];
const RAW_TEXT: &[&str] = &["script", "style", "textarea", "title", "xmp", "noembed", "noframes", "iframe", "plaintext"];

pub(crate) fn guard_depth(html: &str, max_open: usize) -> (&str, bool) {
    let b = html.as_bytes();
    let mut open = 0usize;
    let mut i = 0;
    while let Some(off) = b[i..].iter().position(|&c| c == b'<') {
        let j = i + off;
        let rest = &b[j + 1..];
        if rest.starts_with(b"!--") {
            i = find(b, j + 4, b"-->").map_or(b.len(), |k| k + 3);
            continue;
        }
        if rest.first() == Some(&b'/') {
            open = open.saturating_sub(1);
            i = j + 2;
            continue;
        }
        if !rest.first().is_some_and(u8::is_ascii_alphabetic) {
            i = j + 1;
            continue;
        }
        let name_len = rest.iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b':' || **c == b'-').count();
        let name = std::str::from_utf8(&rest[..name_len]).unwrap_or("").to_ascii_lowercase();
        let gt = b[j..].iter().position(|&c| c == b'>').map_or(b.len(), |k| j + k);
        let self_closing = gt > 0 && gt < b.len() && b[gt - 1] == b'/';
        if RAW_TEXT.contains(&name.as_str()) {
            let close = format!("</{name}");
            i = find_ci(b, gt, close.as_bytes()).unwrap_or(b.len());
            continue;
        }
        if !self_closing && !VOID.contains(&name.as_str()) && !IMPLIED_END.contains(&name.as_str()) {
            open += 1;
            if open > max_open {
                return (&html[..j], true);
            }
        }
        i = (gt + 1).min(b.len());
    }
    (html, false)
}

fn find(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    hay.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

fn find_ci(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    hay.get(from..)?.windows(needle.len()).position(|w| w.eq_ignore_ascii_case(needle)).map(|p| p + from)
}

struct Budget {
    nodes: usize,
    max_depth: usize,
    truncated: bool,
}

fn convert(handle: &Handle, depth: usize, budget: &mut Budget) -> Option<Node> {
    if budget.nodes == 0 {
        budget.truncated = true;
        return None;
    }
    budget.nodes -= 1;
    match &handle.data {
        NodeData::Text { contents } => Some(Node::Text(contents.borrow().to_string())),
        NodeData::Element { name, attrs, .. } => {
            let tag = name.local.to_string().to_ascii_lowercase();
            let attrs = attrs
                .borrow()
                .iter()
                .map(|a| (a.name.local.to_string().to_ascii_lowercase(), a.value.to_string()))
                .collect();
            let mut el = Element { tag, attrs, children: Vec::new() };
            if depth >= budget.max_depth {
                // Too deep: keep the text only, gathered without recursion.
                budget.truncated = true;
                let text = flat_text(handle);
                if !text.is_empty() {
                    el.children.push(Node::Text(text));
                }
                return Some(Node::Element(el));
            }
            for child in handle.children.borrow().iter() {
                if let Some(n) = convert(child, depth + 1, budget) {
                    el.children.push(n);
                }
            }
            Some(Node::Element(el))
        }
        _ => None,
    }
}

/// All text under `handle`, iteratively, with a space between elements.
fn flat_text(handle: &Handle) -> String {
    let mut out = String::new();
    let mut stack: Vec<Handle> = handle.children.borrow().iter().rev().cloned().collect();
    while let Some(h) = stack.pop() {
        match &h.data {
            NodeData::Text { contents } => out.push_str(&contents.borrow()),
            NodeData::Element { name, .. } => {
                let tag = &*name.local;
                if matches!(tag, "script" | "style" | "title" | "template" | "noscript") {
                    continue;
                }
                out.push(' ');
                stack.extend(h.children.borrow().iter().rev().cloned());
            }
            _ => {}
        }
        if out.len() > 1 << 20 {
            break;
        }
    }
    collapse_ws(&out)
}

const VOID: &[&str] = &["br", "hr", "img", "wbr", "col", "area", "input", "meta", "link", "base", "source", "track"];

/// Serialise nodes as HTML (body content, no document wrapper).
pub(crate) fn to_html(nodes: &[Node], out: &mut String) {
    for n in nodes {
        match n {
            Node::Text(t) => escape_text(t, out),
            Node::Element(e) => {
                out.push('<');
                out.push_str(&e.tag);
                for (k, v) in &e.attrs {
                    out.push(' ');
                    out.push_str(k);
                    out.push_str("=\"");
                    escape_attr(v, out);
                    out.push('"');
                }
                out.push('>');
                if VOID.contains(&e.tag.as_str()) {
                    continue;
                }
                to_html(&e.children, out);
                out.push_str("</");
                out.push_str(&e.tag);
                out.push('>');
            }
        }
    }
}

pub(crate) fn escape_text(s: &str, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\u{A0}' => out.push_str("&nbsp;"),
            c => out.push(c),
        }
    }
}

pub(crate) fn escape_attr(s: &str, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_serialises_round_trip() {
        let p = parse("<p class=x>a &amp; <b>b</b><br>c</p>", 64, 1000);
        let body = p.root.children.iter().find_map(|n| match n {
            Node::Element(e) if e.tag == "body" => Some(e),
            _ => None,
        });
        let mut out = String::new();
        to_html(&body.unwrap().children, &mut out);
        assert_eq!(out, "<p class=\"x\">a &amp; <b>b</b><br>c</p>");
    }

    #[test]
    fn deep_nesting_is_flattened_not_recursed() {
        let html = "<div>".repeat(1_000) + "deep text" + &"</div>".repeat(1_000);
        let p = parse(&html, 64, 1_000_000);
        assert!(p.truncated);
        assert!(p.root.text(1000).contains("deep text"));
    }

    #[test]
    fn depth_guard_cuts_only_real_nesting() {
        let sloppy = "<p>para<li>item<td>cell<br><img src=x>".repeat(5_000);
        assert!(!guard_depth(&sloppy, 100).1);
        let closed = "<div><span>x</span></div>".repeat(5_000);
        assert!(!guard_depth(&closed, 100).1);
        let script = format!("<script>{}</script><div>", "<div>".repeat(500));
        assert!(!guard_depth(&script, 100).1, "script text is not markup");
        let deep = "<div>".repeat(5_000);
        let (cut, hit) = guard_depth(&deep, 100);
        assert!(hit);
        assert_eq!(cut.len(), 100 * 5);
    }

    #[test]
    fn node_budget_is_enforced() {
        let html = "<p>x</p>".repeat(10_000);
        let p = parse(&html, 64, 500);
        assert!(p.truncated);
    }

    #[test]
    fn invisible_padding_collapses() {
        assert_eq!(collapse_ws("a\u{200C}\u{A0}\u{200C}\u{A0} b\u{FEFF}c"), "a bc");
    }
}
