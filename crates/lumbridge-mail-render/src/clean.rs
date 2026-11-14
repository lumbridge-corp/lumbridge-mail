//! The cleaning pass over the parsed tree: removes active and hidden content, classifies every image
//! and link, and records what it did in a [`Report`]. Both [`crate::sanitize`] and [`crate::blocks`]
//! start from its output; sanitised HTML additionally goes through ammonia as a second gate.
//!
//! Ideas (not code) from overlook's prepass (MIT): what mail hides, and that hidden preheaders and
//! tracking pixels are resolved on the DOM before sanitising.

use crate::dom::{Element, Node, is_invisible};
use crate::links::{self, LinkWarning};
use crate::{Policy, style};
use lumbridge_mail_core::Attachment;
use serde::{Deserialize, Serialize};

/// What rendering found and blocked. The app shows the remote-content bar from `remote_images` +
/// `remote_blocked`, and a privacy note from `tracking_pixels`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Report {
    /// Remote images that are real content (trackers not counted).
    pub remote_images: u32,
    /// True when `remote_images` were not loaded because of the policy.
    pub remote_blocked: bool,
    /// Images that exist to report an open: 1x1 / hidden images and known tracker hosts. Always dropped.
    pub tracking_pixels: u32,
    /// Remote loads through CSS (`url()` in inline styles, `<style>`, `<link rel=stylesheet>`). Always
    /// dropped, also when remote content is allowed.
    pub remote_css: u32,
    /// Inline images resolved through `cid:` to an attachment.
    pub cid_images: u32,
    /// `cid:` images with no matching attachment.
    pub cid_unresolved: u32,
    /// Links whose text shows a different domain than they go to, punycode hosts, and so on.
    pub suspicious_links: Vec<LinkWarning>,
    pub removed: Removed,
    /// A limit was hit (size, depth, node or block count); the rendering is partial.
    pub truncated: bool,
}

/// Counts of things removed because mail must not have them.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Removed {
    pub scripts: u32,
    /// `on*` attributes.
    pub event_handlers: u32,
    pub forms: u32,
    /// iframe, frame, object, embed, applet.
    pub frames: u32,
    /// `<style>` and `<link rel=stylesheet>`.
    pub stylesheets: u32,
    pub meta_refresh: u32,
    pub base_tags: u32,
    pub svg: u32,
    /// `javascript:`, `data:`, `file:` and relative link targets.
    pub unsafe_links: u32,
    /// `data:` images that were not small raster images.
    pub data_uris: u32,
    /// Elements hidden from the reader (preheaders, `display:none`).
    pub hidden: u32,
}

const MAX_WARNINGS: usize = 50;

/// Kept elements (everything else is removed with its content or unwrapped).
const KEEP: &[&str] = &[
    "a", "abbr", "address", "article", "aside", "b", "big", "blockquote", "br", "caption", "center", "cite",
    "code", "col", "colgroup", "dd", "del", "details", "div", "dl", "dt", "em", "figcaption", "figure", "font",
    "footer", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hr", "i", "img", "ins", "kbd", "li", "main", "mark",
    "nav", "ol", "p", "pre", "q", "s", "samp", "section", "small", "span", "strike", "strong", "sub", "summary",
    "sup", "table", "tbody", "td", "tfoot", "th", "thead", "tr", "tt", "u", "ul", "var", "wbr",
];

/// Removed together with everything inside.
const DROP: &[&str] = &[
    "script", "noscript", "template", "title", "style", "iframe", "frame", "frameset", "object", "embed",
    "applet", "svg", "math", "video", "audio", "canvas", "map", "area", "link", "meta", "base", "select",
    "textarea", "button", "input", "option", "optgroup", "datalist", "output", "keygen", "param", "source",
    "track", "portal", "head", "xml", "plaintext", "xmp", "noembed", "noframes",
];

pub(crate) struct Cleaned {
    pub body: Element,
    pub report: Report,
}

struct Ctx<'a> {
    policy: &'a Policy,
    attachments: &'a [Attachment],
    report: Report,
    /// Inside an element whose font size is zero, with no descendant setting it back: text here is
    /// invisible to the reader (a smuggling trick), so it is dropped. Children that set their own size
    /// (the inline-block whitespace hack) are visible again.
    zero_font: bool,
}

/// Clean a parsed `<html>` root. Returns the cleaned `<body>`.
pub(crate) fn clean(root: Element, policy: &Policy, attachments: &[Attachment]) -> Cleaned {
    let mut ctx = Ctx { policy, attachments, report: Report::default(), zero_font: false };
    let mut body = Element::new("body");
    for node in root.children {
        if let Node::Element(e) = node {
            match e.tag.as_str() {
                "head" => count_dropped(&e, &mut ctx),
                "body" => {
                    body_background(&e, &mut ctx);
                    body.children.extend(clean_nodes(e.children, &mut ctx));
                }
                _ => body.children.extend(clean_nodes(vec![Node::Element(e)], &mut ctx)),
            }
        }
    }
    Cleaned { body, report: ctx.report }
}

fn body_background(e: &Element, ctx: &mut Ctx) {
    if let Some(bg) = e.attr("background") {
        count_css_url(bg, ctx);
    }
    if let Some(s) = e.attr("style") {
        for u in style::urls(s) {
            count_css_url(&u, ctx);
        }
    }
}

fn is_remote(url: &str) -> bool {
    let l = url.trim().to_ascii_lowercase();
    l.starts_with("http:") || l.starts_with("https:") || l.starts_with("//")
}

fn count_css_url(url: &str, ctx: &mut Ctx) {
    if is_remote(url) {
        if links::is_tracker_url(&links::safe_href(url).unwrap_or_default()) {
            ctx.report.tracking_pixels += 1;
        } else {
            ctx.report.remote_css += 1;
        }
    }
}

/// Count what a dropped subtree contained (for the report), without keeping any of it.
fn count_dropped(e: &Element, ctx: &mut Ctx) {
    match e.tag.as_str() {
        "script" => ctx.report.removed.scripts += 1,
        "style" => {
            ctx.report.removed.stylesheets += 1;
            let css: String = e.children.iter().filter_map(|n| if let Node::Text(t) = n { Some(t.as_str()) } else { None }).collect();
            for u in style::urls(&css) {
                count_css_url(&u, ctx);
            }
            // `@import "https://..."` (without url()) is a remote load too.
            let lower = css.to_ascii_lowercase();
            for part in lower.split("@import").skip(1) {
                let target = part.trim_start().trim_start_matches(['"', '\'']);
                if is_remote(target) {
                    ctx.report.remote_css += 1;
                }
            }
        }
        "link" => {
            if e.attr("rel").is_some_and(|r| r.to_ascii_lowercase().contains("stylesheet")) {
                ctx.report.removed.stylesheets += 1;
                if e.attr("href").is_some_and(is_remote) {
                    ctx.report.remote_css += 1;
                }
            }
        }
        "meta" => {
            if e.attr("http-equiv").is_some_and(|h| h.eq_ignore_ascii_case("refresh")) {
                ctx.report.removed.meta_refresh += 1;
            }
        }
        "base" => ctx.report.removed.base_tags += 1,
        "iframe" | "frame" | "frameset" | "object" | "embed" | "applet" => ctx.report.removed.frames += 1,
        "svg" | "math" => ctx.report.removed.svg += 1,
        "form" => ctx.report.removed.forms += 1,
        _ => {}
    }
    for (k, _) in &e.attrs {
        if k.starts_with("on") {
            ctx.report.removed.event_handlers += 1;
        }
    }
    for c in &e.children {
        if let Node::Element(c) = c {
            count_dropped(c, ctx);
        }
    }
}

fn clean_nodes(nodes: Vec<Node>, ctx: &mut Ctx) -> Vec<Node> {
    let mut out = Vec::with_capacity(nodes.len());
    for node in nodes {
        match node {
            Node::Text(t) if ctx.zero_font && !t.trim().is_empty() => ctx.report.removed.hidden += 1,
            Node::Text(t) => out.push(Node::Text(t)),
            Node::Element(e) => clean_element(e, ctx, &mut out),
        }
    }
    out
}

fn clean_element(e: Element, ctx: &mut Ctx, out: &mut Vec<Node>) {
    let tag = e.tag.clone();
    if DROP.contains(&tag.as_str()) {
        count_dropped(&e, ctx);
        return;
    }
    if style::is_hidden(&e) {
        ctx.report.removed.hidden += 1;
        count_hidden(&e, ctx);
        return;
    }
    let outer_zero_font = ctx.zero_font;
    if let Some(size) = style::font_size_px(&e) {
        ctx.zero_font = size <= 0.5;
    }
    clean_visible_element(e, tag, ctx, out);
    ctx.zero_font = outer_zero_font;
}

fn clean_visible_element(mut e: Element, tag: String, ctx: &mut Ctx, out: &mut Vec<Node>) {
    // Event handlers go first, whatever happens to the element.
    let before = e.attrs.len();
    e.attrs.retain(|(k, _)| !k.starts_with("on"));
    ctx.report.removed.event_handlers += (before - e.attrs.len()) as u32;

    if tag == "form" {
        ctx.report.removed.forms += 1;
    }
    if !KEEP.contains(&tag.as_str()) {
        // Unknown, namespaced (`o:p`) or structural-only elements: keep the content, lose the element.
        let children = std::mem::take(&mut e.children);
        let block = matches!(tag.as_str(), "form" | "fieldset" | "body" | "html" | "dialog" | "legend");
        if block {
            let mut div = Element::new("div");
            div.children = clean_nodes(children, ctx);
            out.push(Node::Element(div));
        } else {
            out.extend(clean_nodes(children, ctx));
        }
        return;
    }

    if let Some(bg) = e.remove_attr("background") {
        count_css_url(&bg, ctx);
    }
    if let Some(s) = e.attr("style").map(str::to_owned) {
        for u in style::urls(&s) {
            count_css_url(&u, ctx);
        }
        match style::filter(&s, ctx.policy.keep_colors) {
            Some(f) => e.set_attr("style", f),
            None => {
                e.remove_attr("style");
            }
        }
    }
    if tag == "img" {
        if let Some(img) = clean_img(e, ctx) {
            out.push(Node::Element(img));
        }
        return;
    }
    keep_attrs(&mut e, ctx.policy.keep_colors);
    if tag == "a"
        && let Some(href) = e.remove_attr("href") {
            match links::safe_href(&href) {
                Some(safe) => e.set_attr("href", safe),
                None => ctx.report.removed.unsafe_links += 1,
            }
        }
    e.children = clean_nodes(std::mem::take(&mut e.children), ctx);
    if tag == "a"
        && let Some(href) = e.attr("href")
            && ctx.report.suspicious_links.len() < MAX_WARNINGS {
                let text: String = e.text(400).chars().filter(|c| !is_invisible(*c)).collect();
                if let Some(w) = links::link_warning(&text, href) {
                    ctx.report.suspicious_links.push(w);
                }
            }
    out.push(Node::Element(e));
}

/// Hidden subtrees still count their trackers (a hidden remote image is a tracker by definition).
fn count_hidden(e: &Element, ctx: &mut Ctx) {
    if e.tag == "img" && e.attr("src").is_some_and(is_remote) {
        ctx.report.tracking_pixels += 1;
    }
    for c in &e.children {
        if let Node::Element(c) = c {
            if DROP.contains(&c.tag.as_str()) {
                count_dropped(c, ctx);
            } else {
                count_hidden(c, ctx);
            }
        }
    }
}

/// Attributes kept on non-image elements. `class`, `id` and `data-smartmail` stay in the tree for
/// quote and signature detection; ammonia drops them from sanitised HTML.
fn keep_attrs(e: &mut Element, keep_colors: bool) {
    let tag = e.tag.clone();
    e.attrs.retain(|(k, v)| {
        let k = k.as_str();
        match k {
            "style" | "align" | "valign" | "dir" | "lang" | "title" | "class" | "id" | "data-smartmail" => true,
            "href" => tag == "a",
            "colspan" | "rowspan" => matches!(tag.as_str(), "td" | "th"),
            "width" | "height" => matches!(tag.as_str(), "td" | "th" | "table" | "col"),
            "border" | "cellpadding" | "cellspacing" | "role" => tag == "table",
            "bgcolor" => keep_colors && matches!(tag.as_str(), "td" | "th" | "table" | "tr"),
            "color" => keep_colors && tag == "font",
            "size" | "face" => tag == "font",
            "start" | "type" => tag == "ol" || (tag == "blockquote" && v.eq_ignore_ascii_case("cite")),
            "value" => tag == "li",
            _ => false,
        }
    });
}

fn dim(v: Option<&str>) -> Option<u32> {
    let v = v?.trim().trim_end_matches("px").trim();
    v.parse::<f32>().ok().filter(|n| n.is_finite() && *n >= 0.0).map(|n| n.min(100_000.0) as u32)
}

const RASTER: &[&str] = &["data:image/png;base64,", "data:image/gif;base64,", "data:image/jpeg;base64,", "data:image/jpg;base64,", "data:image/webp;base64,"];

/// Markers set on cleaned `<img>` for the block builder: `data-lbm-kind` is `cid`, `remote`, `blocked`,
/// `data` or `missing`.
fn clean_img(e: Element, ctx: &mut Ctx) -> Option<Element> {
    let src = e.attr("src").unwrap_or("").trim().to_owned();
    let style = e.attr("style").map(str::to_owned);
    let sw = style.as_deref().and_then(|s| style::declarations(s).into_iter().rev().find(|(k, _)| k == "width").and_then(|(_, v)| style::px(&v)));
    let sh = style.as_deref().and_then(|s| style::declarations(s).into_iter().rev().find(|(k, _)| k == "height").and_then(|(_, v)| style::px(&v)));
    let w = dim(e.attr("width")).or(sw.map(|n| n as u32));
    let h = dim(e.attr("height")).or(sh.map(|n| n as u32));
    let mut img = Element::new("img");
    if let Some(alt) = e.attr("alt") {
        let alt: String = alt.chars().filter(|c| !is_invisible(*c)).take(300).collect();
        if !alt.trim().is_empty() {
            img.set_attr("alt", alt.trim());
        }
    }
    if let Some(w) = w {
        img.set_attr("width", w.to_string());
    }
    if let Some(h) = h {
        img.set_attr("height", h.to_string());
    }
    if let Some(t) = e.attr("title") {
        img.set_attr("title", t);
    }
    let lower = src.to_ascii_lowercase();
    if let Some(cid) = lower.strip_prefix("cid:").map(|_| &src[4..]) {
        let cid = normalise_cid(cid);
        img.set_attr("data-lbm-cid", cid.clone());
        match ctx.attachments.iter().find(|a| a.content_id.as_deref().is_some_and(|c| normalise_cid(c).eq_ignore_ascii_case(&cid))) {
            Some(att) => {
                ctx.report.cid_images += 1;
                img.set_attr("data-lbm-kind", "cid");
                img.set_attr("data-lbm-att", att.id.as_str());
                let url = match &ctx.policy.cid_base {
                    Some(base) => format!("{base}{}", percent(att.id.as_str())),
                    None => format!("cid:{}", percent(&cid)),
                };
                img.set_attr("src", url);
            }
            None => {
                ctx.report.cid_unresolved += 1;
                img.set_attr("data-lbm-kind", "missing");
            }
        }
        return Some(img);
    }
    if lower.starts_with("data:") {
        let ok = RASTER.iter().any(|p| lower.starts_with(p)) && src.len() <= ctx.policy.limits.max_data_uri_bytes;
        if !ok {
            ctx.report.removed.data_uris += 1;
            return None;
        }
        img.set_attr("data-lbm-kind", "data");
        img.set_attr("src", src);
        return Some(img);
    }
    if is_remote(&src) {
        let url = links::safe_href(&src)?;
        let tiny = match (w, h) {
            (Some(w), Some(h)) => w <= 2 && h <= 2,
            (Some(x), None) | (None, Some(x)) => x <= 1,
            (None, None) => false,
        };
        if tiny || links::is_tracker_url(&url) {
            ctx.report.tracking_pixels += 1;
            return None;
        }
        ctx.report.remote_images += 1;
        if ctx.policy.remote_allowed_now() {
            img.set_attr("data-lbm-kind", "remote");
            img.set_attr("src", url);
        } else {
            ctx.report.remote_blocked = true;
            img.set_attr("data-lbm-kind", "blocked");
            img.set_attr("data-lbm-host", links::host_of(&url).unwrap_or_default());
        }
        return Some(img);
    }
    // Relative, javascript:, file: ... nothing to show but the alt text.
    img.attr("alt")?;
    img.set_attr("data-lbm-kind", "missing");
    Some(img)
}

fn normalise_cid(cid: &str) -> String {
    let cid = cid.trim().trim_start_matches('<').trim_end_matches('>');
    percent_decode(cid)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(a), Some(b)) = (hex(bytes.get(i + 1).copied()), hex(bytes.get(i + 2).copied())) {
                out.push(a * 16 + b);
                i += 3;
                continue;
            }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: Option<u8>) -> Option<u8> {
    match b? {
        c @ b'0'..=b'9' => Some(c - b'0'),
        c @ b'a'..=b'f' => Some(c - b'a' + 10),
        c @ b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn percent(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~@$!*+,;=:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
