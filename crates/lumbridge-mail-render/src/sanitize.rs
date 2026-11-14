//! Received HTML to HTML that is safe to hand to a renderer (Blitz in stage 2, a print view, an
//! outgoing quote). Two gates: our cleaning pass on the DOM (which knows about mail: trackers, hidden
//! preheaders, cid:, phishing links), then ammonia with a strict allowlist as defence in depth.
//!
//! The allowlist follows overlook's sanitizer (MIT; mail-refs gpui-mail/overlook, overlook-core/src/sanitize.rs) in
//! spirit: same family of tags, `rel="noopener noreferrer"`, no relative URLs.

use crate::clean::{self, Report};
use crate::dom::{self, Node};
use crate::fold::{self, Parts};
use crate::{Policy, charset, cut, style};
use lumbridge_mail_core::{Attachment, Body};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Sanitised HTML fragments (body content, no `<html>` wrapper), split when folding is on.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SafeHtml {
    /// The new content. With folding off, the whole message.
    pub html: String,
    pub signature: Option<String>,
    pub quoted: Option<String>,
    pub report: Report,
}

impl SafeHtml {
    /// Everything, in order (for printing or "show quoted text").
    pub fn full(&self) -> String {
        let mut s = self.html.clone();
        for part in [&self.signature, &self.quoted].into_iter().flatten() {
            s.push_str(part);
        }
        s
    }
}

/// Sanitise a message's HTML part, resolving `cid:` images against its attachments.
/// None when the body has no HTML part.
pub fn sanitize_body(body: &Body, policy: &Policy) -> Option<SafeHtml> {
    body.html.as_deref().map(|h| sanitize(h, &body.attachments, policy))
}

/// Sanitise received HTML. Never fails; hostile or broken input gives best-effort output.
pub fn sanitize(html: &str, attachments: &[Attachment], policy: &Policy) -> SafeHtml {
    let (html, cut_off) = cut(html, policy.limits.max_input_bytes);
    let html = charset::repair(html);
    let parsed = dom::parse(&html, policy.limits.max_depth, policy.limits.max_nodes);
    let cleaned = clean::clean(parsed.root, policy, attachments);
    let mut report = cleaned.report;
    report.truncated |= cut_off || parsed.truncated;
    let parts = if policy.fold {
        fold::split_html(cleaned.body)
    } else {
        Parts { new: cleaned.body.children, signature: Vec::new(), quoted: Vec::new() }
    };
    let builder = builder(policy);
    let gate = |nodes: &[Node]| {
        let mut raw = String::new();
        dom::to_html(nodes, &mut raw);
        builder.clean(&raw).to_string()
    };
    let opt = |nodes: &[Node]| (!nodes.is_empty()).then(|| gate(nodes)).filter(|s| !s.trim().is_empty());
    SafeHtml { html: gate(&parts.new), signature: opt(&parts.signature), quoted: opt(&parts.quoted), report }
}

const TAGS: &[&str] = &[
    "a", "abbr", "address", "article", "aside", "b", "big", "blockquote", "br", "caption", "center", "cite",
    "code", "col", "colgroup", "dd", "del", "details", "div", "dl", "dt", "em", "figcaption", "figure", "font",
    "footer", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hr", "i", "img", "ins", "kbd", "li", "main", "mark",
    "nav", "ol", "p", "pre", "q", "s", "samp", "section", "small", "span", "strike", "strong", "sub", "summary",
    "sup", "table", "tbody", "td", "tfoot", "th", "thead", "tr", "tt", "u", "ul", "var", "wbr",
];

fn builder(policy: &Policy) -> ammonia::Builder<'_> {
    let mut b = ammonia::Builder::empty();
    b.tags(TAGS.iter().copied().collect::<HashSet<_>>());
    b.clean_content_tags(["script", "style", "svg", "math", "iframe", "object", "embed", "template", "noscript", "textarea", "title"].into_iter().collect());
    b.generic_attributes(["style", "align", "valign", "dir", "lang", "title"].into_iter().collect());
    b.add_tag_attributes("a", ["href"]);
    b.add_tag_attributes("img", ["src", "alt", "width", "height", "data-lbm-kind"]);
    b.add_tag_attributes("td", ["colspan", "rowspan", "width", "height", "bgcolor"]);
    b.add_tag_attributes("th", ["colspan", "rowspan", "width", "height", "bgcolor"]);
    b.add_tag_attributes("tr", ["bgcolor"]);
    b.add_tag_attributes("table", ["width", "border", "cellpadding", "cellspacing", "bgcolor"]);
    b.add_tag_attributes("col", ["width"]);
    b.add_tag_attributes("ol", ["start", "type"]);
    b.add_tag_attributes("li", ["value"]);
    b.add_tag_attributes("font", ["color", "size", "face"]);
    b.link_rel(Some("noopener noreferrer nofollow"));
    let mut schemes: HashSet<&str> = ["http", "https", "mailto", "tel", "cid", "data"].into_iter().collect();
    // The cid_base scheme (e.g. `lbm-attachment:`), so resolved inline images survive. Lower case only.
    let cid_scheme = policy.cid_base.as_deref().and_then(|b| b.split_once(':')).map(|(s, _)| s);
    if let Some(s) = cid_scheme {
        schemes.insert(s);
    }
    b.url_schemes(schemes);
    let cid_prefix = cid_scheme.map(|s| format!("{s}:"));
    b.url_relative(ammonia::UrlRelative::Deny);
    b.strip_comments(true);
    let keep_colors = policy.keep_colors;
    let remote = policy.remote_allowed_now();
    b.attribute_filter(move |element, attribute, value| {
        match (element, attribute) {
            (_, "style") => style::filter(value, keep_colors).map(Into::into),
            (_, "bgcolor") | ("font", "color") => keep_colors.then(|| value.into()),
            ("img", "src") => {
                let lower = value.to_ascii_lowercase();
                let ok = lower.starts_with("cid:")
                    || (lower.starts_with("data:image/") && lower.contains(";base64,") && !lower.starts_with("data:image/svg"))
                    || (remote && (lower.starts_with("https:") || lower.starts_with("http:")))
                    || cid_prefix.as_deref().is_some_and(|p| lower.starts_with(p));
                ok.then(|| value.into())
            }
            ("a", "href") => (!value.to_ascii_lowercase().starts_with("data:") && !value.to_ascii_lowercase().starts_with("cid:")).then(|| value.into()),
            ("td" | "th", "colspan" | "rowspan") => {
                value.trim().parse::<u32>().ok().filter(|n| *n > 0).map(|n| n.min(64).to_string().into())
            }
            (_, "align" | "valign") => {
                let v = value.trim().to_ascii_lowercase();
                matches!(v.as_str(), "left" | "right" | "center" | "justify" | "top" | "middle" | "bottom").then(|| v.into())
            }
            _ => Some(value.into()),
        }
    });
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(html: &str) -> SafeHtml {
        sanitize(html, &[], &Policy::default())
    }

    #[test]
    fn scripts_handlers_and_js_links_go() {
        let out = s("<p onclick='x()'>hi<script>alert(1)</script><a href='javascript:alert(1)'>x</a><img src=x onerror=alert(1)></p>");
        assert!(!out.html.contains("script"));
        assert!(!out.html.contains("onclick"));
        assert!(!out.html.contains("onerror"));
        assert!(!out.html.contains("javascript"));
        assert_eq!(out.report.removed.scripts, 1);
        assert_eq!(out.report.removed.event_handlers, 2);
        assert_eq!(out.report.removed.unsafe_links, 1);
    }

    #[test]
    fn remote_images_blocked_then_allowed() {
        let html = "<img src='https://cdn.example.com/a.png' width=300 height=100><img src='https://cdn.example.com/p.gif' width=1 height=1>";
        let out = s(html);
        assert!(!out.html.contains("cdn.example.com"));
        assert_eq!(out.report.remote_images, 1);
        assert!(out.report.remote_blocked);
        assert_eq!(out.report.tracking_pixels, 1);
        let out = sanitize(html, &[], &Policy::remote_allowed());
        assert!(out.html.contains("https://cdn.example.com/a.png"));
        assert!(!out.html.contains("p.gif"), "tracking pixels never load");
    }

    #[test]
    fn links_get_rel() {
        let out = s("<a href='https://example.com/'>x</a>");
        assert!(out.html.contains("rel=\"noopener noreferrer nofollow\""));
    }
}
