//! Quote and signature folding: split a message into the new content, the signature, and the quoted
//! history, so the reader can collapse the last two and the AI layer can skip them.
//!
//! Forwarded messages are content, not history: a "Forwarded message" block is never folded.
//! Inline replies (new text between quoted lines) are never folded either; only a quoted *tail* is.
//! Ideas from overlook's thread_history (MIT) on which markers Gmail, Apple Mail, Thunderbird,
//! Yahoo and Outlook use.

use crate::dom::{Element, Node};
use serde::{Deserialize, Serialize};

/// A plain-text body, split.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FoldedText {
    /// What the sender wrote this time.
    pub body: String,
    /// From the signature delimiter (`-- `) or a "Sent from my ..." line, if any.
    pub signature: Option<String>,
    /// The quoted history, starting at its attribution line ("On ... wrote:", "From: ... Sent:").
    pub quoted: Option<String>,
}

/// Split plain text into new content, signature and quoted history.
pub fn fold_text(text: &str) -> FoldedText {
    let text = text.replace("\r\n", "\n");
    let lines: Vec<&str> = text.split('\n').collect();
    let quote_at = quote_start(&lines);
    let (head, quoted) = match quote_at {
        Some(i) => (&lines[..i], Some(lines[i..].join("\n"))),
        None => (&lines[..], None),
    };
    let sig_at = signature_start(head);
    let (body, signature) = match sig_at {
        Some(i) => (head[..i].join("\n"), Some(head[i..].join("\n"))),
        None => (head.join("\n"), None),
    };
    let tidy = |s: String| {
        let t = s.trim_matches('\n').trim_end().to_owned();
        (!t.trim().is_empty()).then_some(t)
    };
    FoldedText {
        body: body.trim_end().trim_start_matches('\n').to_owned(),
        signature: signature.and_then(tidy),
        quoted: quoted.and_then(tidy),
    }
}

fn is_quoted(line: &str) -> bool {
    line.trim_start().starts_with('>')
}

fn is_forward_marker(line: &str) -> bool {
    let l = line.trim().to_ascii_lowercase();
    (l.contains("forwarded message") && l.starts_with('-')) || l == "begin forwarded message:"
}

/// "On Tue, 3 Nov 2026 at 09:12, Ada <ada@example.com> wrote:" and friends, possibly over two lines.
pub(crate) fn is_attribution(text: &str) -> bool {
    let t = text.trim();
    if t.len() > 400 || !t.ends_with(':') {
        return false;
    }
    let l = t.to_lowercase();
    let starts = ["on ", "le ", "am ", "el ", "il ", "op ", "em ", "den ", "w dniu ", "el día "];
    let ends = ["wrote:", "a écrit :", "a écrit:", "schrieb:", "escribió:", "ha scritto:", "schreef:", "escreveu:", "skrev:", "napisał:", "написал:", "writes:"];
    (starts.iter().any(|s| l.starts_with(s)) || l.contains(" on ")) && ends.iter().any(|e| l.ends_with(e))
        || (l.ends_with("wrote:") && l.contains('@'))
}

fn is_outlook_header(lines: &[&str], i: usize) -> bool {
    let first = lines[i].trim().trim_start_matches('*').to_ascii_lowercase();
    if !(first.starts_with("from:") || first.starts_with("de :") || first.starts_with("von:")) {
        return false;
    }
    let window: Vec<String> = lines[i + 1..lines.len().min(i + 7)].iter().map(|l| l.trim().trim_start_matches('*').to_ascii_lowercase()).collect();
    let has = |p: &[&str]| window.iter().any(|l| p.iter().any(|p| l.starts_with(p)));
    has(&["sent:", "date:", "envoyé :", "gesendet:"]) && has(&["subject:", "objet :", "betreff:", "to:"])
}

fn is_divider(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 10 && (t.chars().all(|c| c == '_') || t.chars().all(|c| c == '-'))
}

fn quote_start(lines: &[&str]) -> Option<usize> {
    let mut forwarded = false;
    for i in 0..lines.len() {
        let line = lines[i];
        let t = line.trim();
        if is_forward_marker(line) {
            forwarded = true;
            continue;
        }
        if forwarded {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        if lower.starts_with("-----original message-----") || lower.starts_with("----- original message -----") {
            return Some(i);
        }
        if is_outlook_header(lines, i) {
            // Outlook's underscore divider sits right above.
            let mut j = i;
            while j > 0 && lines[j - 1].trim().is_empty() {
                j -= 1;
            }
            let start = if j > 0 && is_divider(lines[j - 1]) { j - 1 } else { i };
            return Some(start);
        }
        // "On ... wrote:" on one line or wrapped onto two, followed by quoted lines to the end.
        let attribution_len = if is_attribution(t) {
            Some(1)
        } else if i + 1 < lines.len() && lower.starts_with("on ") && is_attribution(&format!("{t} {}", lines[i + 1].trim())) {
            Some(2)
        } else {
            None
        };
        if let Some(n) = attribution_len
            && quoted_tail(&lines[i + n..]) {
                return Some(i);
            }
    }
    if forwarded {
        return None;
    }
    // A bare trailing run of "> " lines.
    let last = lines.iter().rposition(|l| !l.trim().is_empty())?;
    if !is_quoted(lines[last]) {
        return None;
    }
    let mut start = last;
    while start > 0 && (is_quoted(lines[start - 1]) || lines[start - 1].trim().is_empty()) {
        start -= 1;
    }
    while !is_quoted(lines[start]) {
        start += 1;
    }
    if start == 0 {
        return None; // The whole message is a quote: nothing new to show instead.
    }
    // Include an attribution-like line ending in ':' just above.
    let above = lines[..start].iter().rposition(|l| !l.trim().is_empty());
    if let Some(a) = above {
        let t = lines[a].trim();
        if t.ends_with(':') && t.len() < 300 && (is_attribution(t) || t.to_ascii_lowercase().contains("wrote")) {
            return Some(a);
        }
    }
    Some(start)
}

/// Everything after (ignoring blank lines) is quoted, and there is at least one quoted line.
fn quoted_tail(rest: &[&str]) -> bool {
    let mut any = false;
    for l in rest {
        if l.trim().is_empty() {
            continue;
        }
        if !is_quoted(l) {
            return false;
        }
        any = true;
    }
    any
}

const SENT_FROM: &[&str] = &["sent from my ", "get outlook for ", "sent from mail for windows", "sent from outlook", "sent from yahoo mail", "sent via ", "sent with proton mail"];

fn signature_start(lines: &[&str]) -> Option<usize> {
    if let Some(i) = lines.iter().rposition(|l| *l == "-- " || *l == "--" || *l == "-- \r")
        && (i > 0 || lines.len() > 1) {
            return Some(i);
        }
    let last = lines.iter().rposition(|l| !l.trim().is_empty())?;
    let l = lines[last].trim().to_ascii_lowercase();
    if last > 0 && SENT_FROM.iter().any(|s| l.starts_with(s)) && l.len() < 80 {
        return Some(last);
    }
    None
}

/// Which part of a message a node belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Part {
    New = 0,
    Signature = 1,
    Quoted = 2,
}

/// The three parts of an HTML body.
pub(crate) struct Parts {
    pub new: Vec<Node>,
    pub signature: Vec<Node>,
    pub quoted: Vec<Node>,
}

/// Split a cleaned body. Containers (div, table cells, ...) that straddle a boundary are duplicated
/// shallowly, so each part keeps its structure.
pub(crate) fn split_html(body: Element) -> Parts {
    let mut part = Part::New;
    let mut out = [Vec::new(), Vec::new(), Vec::new()];
    partition(body.children, &mut part, &mut out, 0);
    let [new, signature, quoted] = out;
    if !has_content(&new) && !has_content(&signature) {
        // Nothing but a quote (a bare forward-as-quote, or detection gone wrong): show it all.
        let mut all = signature;
        all.extend(quoted);
        let mut nodes = new;
        nodes.extend(all);
        return Parts { new: nodes, signature: Vec::new(), quoted: Vec::new() };
    }
    Parts { new, signature, quoted }
}

fn has_content(nodes: &[Node]) -> bool {
    nodes.iter().any(|n| match n {
        Node::Text(t) => t.chars().any(|c| !c.is_whitespace() && !crate::dom::is_invisible(c)),
        Node::Element(e) => e.tag == "img" || has_content(&e.children),
    })
}

fn boundary(e: &Element, next: Option<&Element>) -> Option<Part> {
    let quoted = || Some(Part::Quoted);
    if e.has_class("gmail_quote") || e.has_class("gmail_quote_container") {
        let attr = e.text(300).to_ascii_lowercase();
        if attr.contains("forwarded message") && attr.find("forwarded message") < Some(120) {
            return None;
        }
        return quoted();
    }
    if e.has_class("moz-cite-prefix") || e.has_class("yahoo_quoted") || e.id_is("divRplyFwdMsg") || e.id_is("appendonsend")
        || e.has_class("OutlookMessageHeader") || e.has_class("zmail_extra") || e.has_class("protonmail_quote")
    {
        if e.has_class("moz-cite-prefix") && e.text(200).to_ascii_lowercase().contains("forwarded message") {
            return None;
        }
        return quoted();
    }
    if e.tag == "blockquote" && e.attr("type").is_some_and(|t| t.eq_ignore_ascii_case("cite")) && next.is_none() {
        return quoted();
    }
    if e.tag == "hr" && next.is_some_and(|n| n.id_is("divRplyFwdMsg")) {
        return quoted();
    }
    if matches!(e.tag.as_str(), "div" | "p" | "span" | "font") {
        let text = e.text(600);
        let lower = text.to_ascii_lowercase();
        if lower.starts_with("-----original message-----") || lower.starts_with("----- original message -----") {
            return quoted();
        }
        // Outlook desktop's header block: "From: ... Sent: ... To: ... Subject: ..."
        if lower.starts_with("from:") && (lower.contains("sent:") || lower.contains("date:")) && lower.contains("subject:") && text.len() < 600 {
            return quoted();
        }
        // Apple Mail / generic: an attribution line right before a blockquote.
        if is_attribution(&text) && next.is_some_and(|n| n.tag == "blockquote") {
            return quoted();
        }
        if e.has_class("gmail_signature") || e.attr("data-smartmail").is_some_and(|v| v == "gmail_signature")
            || e.has_class("moz-signature") || e.id_is("Signature") || e.id_is("signature") || e.has_class("signature")
        {
            return Some(Part::Signature);
        }
        if text == "--" || text == "-- " {
            return Some(Part::Signature);
        }
    }
    None
}

fn is_container(tag: &str) -> bool {
    matches!(tag, "div" | "center" | "section" | "article" | "main" | "table" | "tbody" | "thead" | "tfoot" | "tr" | "td" | "th" | "font" | "span" | "body" | "header" | "footer" | "aside")
}

fn contains_boundary(e: &Element, depth: usize) -> bool {
    if depth > 64 {
        return false;
    }
    let kids: Vec<&Element> = e.children.iter().filter_map(|n| if let Node::Element(e) = n { Some(e) } else { None }).collect();
    kids.iter().enumerate().any(|(i, k)| {
        boundary(k, kids.get(i + 1).copied()).is_some() || (is_container(&k.tag) && contains_boundary(k, depth + 1))
    })
}

fn partition(nodes: Vec<Node>, part: &mut Part, out: &mut [Vec<Node>; 3], depth: usize) {
    // Index of the next element sibling for each node (whitespace text between them is ignored).
    let mut next_el: Vec<Option<usize>> = vec![None; nodes.len()];
    let mut following: Option<usize> = None;
    for i in (0..nodes.len()).rev() {
        next_el[i] = following;
        match &nodes[i] {
            Node::Element(_) => following = Some(i),
            Node::Text(t) if !t.trim().is_empty() => following = None,
            Node::Text(_) => {}
        }
    }
    let refs: Vec<&Node> = nodes.iter().collect();
    let mut decisions: Vec<(Option<Part>, bool)> = Vec::with_capacity(nodes.len());
    for (i, n) in refs.iter().enumerate() {
        match n {
            Node::Element(e) => {
                let next = next_el[i].and_then(|j| if let Node::Element(e) = refs[j] { Some(e) } else { None });
                let b = boundary(e, next);
                let split = b.is_none() && *part != Part::Quoted && is_container(&e.tag) && depth < 64 && contains_boundary(e, depth);
                decisions.push((b, split));
            }
            Node::Text(_) => decisions.push((None, false)),
        }
    }
    for (node, (b, split)) in nodes.into_iter().zip(decisions) {
        if let Some(b) = b
            && b > *part {
                *part = b;
            }
        match node {
            Node::Element(mut e) if split => {
                let children = std::mem::take(&mut e.children);
                let mut parts = [Vec::new(), Vec::new(), Vec::new()];
                partition(children, part, &mut parts, depth + 1);
                for (ix, p) in parts.into_iter().enumerate() {
                    if !p.is_empty() {
                        out[ix].push(Node::Element(Element { tag: e.tag.clone(), attrs: e.attrs.clone(), children: p }));
                    }
                }
            }
            n => out[*part as usize].push(n),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmail_plain_reply() {
        let f = fold_text("Sounds good.\n\nOn Tue, Nov 3, 2026 at 9:12 AM Ada Example <ada@example.com> wrote:\n> Lunch Friday?\n> A\n");
        assert_eq!(f.body, "Sounds good.");
        assert!(f.quoted.unwrap().starts_with("On Tue"));
    }

    #[test]
    fn wrapped_attribution() {
        let f = fold_text("Yes.\n\nOn Tue, Nov 3, 2026 at 9:12 AM Ada Example\n<ada@example.com> wrote:\n\n> Lunch?\n");
        assert_eq!(f.body, "Yes.");
        assert!(f.quoted.is_some());
    }

    #[test]
    fn inline_replies_are_not_folded() {
        let t = "On Mon, Ada wrote:\n> Question one?\nAnswer one.\n> Question two?\nAnswer two.";
        let f = fold_text(t);
        assert_eq!(f.quoted, None);
        assert_eq!(f.body, t);
    }

    #[test]
    fn outlook_header_block() {
        let t = "Thanks, will do.\n\n________________________________\nFrom: Ada Example <ada@example.com>\nSent: Tuesday, November 3, 2026 9:12 AM\nTo: Bo <bo@example.com>\nSubject: Report\n\nPlease send the report.";
        let f = fold_text(t);
        assert_eq!(f.body, "Thanks, will do.");
        assert!(f.quoted.unwrap().starts_with("____"));
    }

    #[test]
    fn signature_and_sent_from() {
        let f = fold_text("Hi\n\n-- \nBo Example\nExample Co");
        assert_eq!(f.body, "Hi");
        assert_eq!(f.signature.as_deref(), Some("-- \nBo Example\nExample Co"));
        let f = fold_text("On my way\n\nSent from my phone");
        assert_eq!(f.signature.as_deref(), Some("Sent from my phone"));
    }

    #[test]
    fn forwards_stay_visible() {
        let t = "FYI\n\n---------- Forwarded message ---------\nFrom: Ada <ada@example.com>\nDate: Mon, Nov 2, 2026\nSubject: Plans\nTo: Bo <bo@example.com>\n\nThe plans.";
        let f = fold_text(t);
        assert_eq!(f.quoted, None);
        assert_eq!(f.body, t);
    }

    #[test]
    fn trailing_quote_without_attribution() {
        let f = fold_text("New text\n\n> old\n> older");
        assert_eq!(f.body, "New text");
        assert_eq!(f.quoted.as_deref(), Some("> old\n> older"));
        let f = fold_text("> only a quote");
        assert_eq!(f.quoted, None);
    }
}
