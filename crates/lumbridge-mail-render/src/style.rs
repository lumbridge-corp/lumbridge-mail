//! Inline `style` attributes: parsing, hidden-content detection, and a strict allowlist filter.
//! `<style>` elements are never kept (they are the CSS exfiltration vector: attribute selectors with
//! remote `url()`s); only inline declarations from the allowlist survive.

use crate::dom::Element;

/// Split `a: b; c: d` into lower-cased property names and trimmed values, respecting quotes and
/// parentheses (`url("a;b")`). Comments are removed first.
pub(crate) fn declarations(style: &str) -> Vec<(String, String)> {
    let style = strip_comments(style);
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut start = 0;
    let bytes: Vec<(usize, char)> = style.char_indices().collect();
    let mut push = |decl: &str| {
        if let Some((k, v)) = decl.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            if !k.is_empty() && !v.is_empty() {
                out.push((k, v.to_owned()));
            }
        }
    };
    for &(i, ch) in &bytes {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(ch),
            (None, '(') => depth += 1,
            (None, ')') => depth = (depth - 1).max(0),
            (None, ';') if depth == 0 => {
                push(&style[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    push(&style[start..]);
    out
}

fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        match rest[i + 2..].find("*/") {
            Some(j) => rest = &rest[i + 2 + j + 2..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn value_without_important(v: &str) -> String {
    v.to_ascii_lowercase().replace("!important", "").trim().to_owned()
}

/// A CSS length in px, when it is one (`1px`, `1`, `0.5px`, `0`). Percentages and `em` give None.
pub(crate) fn px(v: &str) -> Option<f32> {
    let v = value_without_important(v);
    let num = v.strip_suffix("px").unwrap_or(&v).trim();
    num.parse::<f32>().ok().filter(|n| n.is_finite())
}

/// Would a mail client not show this element? (hidden preheaders, mobile-only duplicates, tricks
/// used to hide text from the reader but not from a filter.)
pub(crate) fn is_hidden(el: &Element) -> bool {
    if el.attr("hidden").is_some() {
        return true;
    }
    if el.tag == "input" && el.attr("type").is_some_and(|t| t.eq_ignore_ascii_case("hidden")) {
        return true;
    }
    let Some(style) = el.attr("style") else { return false };
    let decls = declarations(style);
    let get = |name: &str| decls.iter().rev().find(|(k, _)| k == name).map(|(_, v)| value_without_important(v));
    if get("display").as_deref() == Some("none") {
        return true;
    }
    if matches!(get("visibility").as_deref(), Some("hidden" | "collapse")) {
        return true;
    }
    if get("mso-hide").as_deref() == Some("all") {
        return true;
    }
    if get("opacity").and_then(|o| o.parse::<f32>().ok()).is_some_and(|o| o <= 0.01) {
        return true;
    }
    let overflow_hidden = get("overflow").as_deref() == Some("hidden");
    let zero = |name: &str| get(name).and_then(|v| px(&v)).is_some_and(|n| n <= 0.5);
    if overflow_hidden && (zero("max-height") || zero("height") || zero("width") || zero("max-width")) {
        return true;
    }
    if zero("font-size") && (zero("line-height") || zero("max-height")) && get("color").is_some() {
        // Zero-size text with its own colour: hidden filler, not the inline-block whitespace hack.
        return true;
    }
    false
}

/// Every `url(...)` in a style value, unquoted.
pub(crate) fn urls(style: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lower = style.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find("url(") {
        let start = from + i + 4;
        let end = lower[start..].find(')').map(|j| start + j).unwrap_or(lower.len());
        let raw = style[start..end].trim().trim_matches(|c| c == '"' || c == '\'').trim();
        out.push(raw.to_owned());
        from = end.min(lower.len());
        if from >= lower.len() {
            break;
        }
    }
    out
}

const ALLOWED: &[&str] = &[
    "text-align", "text-decoration", "font-weight", "font-style", "font-size", "font-family", "line-height",
    "letter-spacing", "white-space", "vertical-align", "text-transform", "text-indent", "padding",
    "padding-top", "padding-right", "padding-bottom", "padding-left", "margin", "margin-top", "margin-right",
    "margin-bottom", "margin-left", "width", "max-width", "min-width", "height", "min-height", "border",
    "border-top", "border-right", "border-bottom", "border-left", "border-width", "border-style",
    "border-collapse", "border-spacing", "border-radius", "display", "direction", "word-break",
    "overflow-wrap", "word-wrap", "table-layout", "list-style-type",
];
const COLOURS: &[&str] = &["color", "background-color", "background", "border-color"];
const DISPLAY_OK: &[&str] =
    &["block", "inline", "inline-block", "table", "table-row", "table-cell", "list-item", "table-row-group"];

/// Keep only allowlisted declarations with harmless values. Returns None when nothing survives.
/// Colours survive only with `keep_colors`; `url()` never does (backgrounds are not painted from
/// inline CSS; a remote load through CSS is exactly what tracking and exfiltration use).
pub(crate) fn filter(style: &str, keep_colors: bool) -> Option<String> {
    let mut kept = Vec::new();
    for (k, v) in declarations(style) {
        let lv = v.to_ascii_lowercase();
        if lv.contains("url(")
            || lv.contains("expression")
            || lv.contains("javascript")
            || lv.contains('\\')
            || lv.contains('@')
            || lv.contains('<')
            || lv.contains("image-set")
            || lv.contains("-moz-binding")
            || lv.contains("behavior")
        {
            continue;
        }
        let ok = if ALLOWED.contains(&k.as_str()) {
            match k.as_str() {
                "display" => DISPLAY_OK.contains(&value_without_important(&v).as_str()),
                k if k.starts_with("border") && !keep_colors && k != "border-collapse" && k != "border-spacing" => {
                    // Keep widths and styles; drop colour tokens so borders follow the theme.
                    let cleaned: Vec<&str> = v.split_whitespace().filter(|t| !looks_like_colour(t)).collect();
                    if cleaned.is_empty() {
                        false
                    } else {
                        kept.push(format!("{k}: {}", cleaned.join(" ")));
                        continue;
                    }
                }
                _ => true,
            }
        } else {
            keep_colors && COLOURS.contains(&k.as_str())
        };
        if ok {
            kept.push(format!("{k}: {v}"));
        }
    }
    (!kept.is_empty()).then(|| kept.join("; "))
}

fn looks_like_colour(token: &str) -> bool {
    let t = token.to_ascii_lowercase();
    t.starts_with('#')
        || t.starts_with("rgb")
        || t.starts_with("hsl")
        || matches!(
            t.as_str(),
            "black" | "white" | "red" | "green" | "blue" | "gray" | "grey" | "silver" | "navy" | "orange"
                | "yellow" | "purple" | "maroon" | "teal" | "transparent" | "currentcolor"
        )
}

/// Font size in px from a style attribute, for promoting big marketing text to a heading.
pub(crate) fn font_size_px(el: &Element) -> Option<f32> {
    let style = el.attr("style")?;
    declarations(style).into_iter().rev().find(|(k, _)| k == "font-size").and_then(|(_, v)| px(&v))
}

/// Does the style make text bold / italic?
pub(crate) fn emphasis(el: &Element) -> (bool, bool) {
    let Some(style) = el.attr("style") else { return (false, false) };
    let mut bold = false;
    let mut italic = false;
    for (k, v) in declarations(style) {
        let v = value_without_important(&v);
        match k.as_str() {
            "font-weight" => bold = v == "bold" || v == "bolder" || v.parse::<u32>().is_ok_and(|n| n >= 600),
            "font-style" => italic = v == "italic" || v == "oblique",
            _ => {}
        }
    }
    (bold, italic)
}

/// The element keeps its text's line breaks (`white-space: pre | pre-wrap | pre-line | break-spaces`):
/// mail that is plain text wrapped in one HTML element, as some drafts and clients send it.
pub(crate) fn keeps_line_breaks(el: &Element) -> Option<bool> {
    let style = el.attr("style")?;
    let mut out = None;
    for (k, v) in declarations(style) {
        if k == "white-space" {
            let v = value_without_important(&v);
            out = Some(matches!(v.as_str(), "pre" | "pre-wrap" | "pre-line" | "break-spaces"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(style: &str) -> Element {
        let mut e = Element::new("div");
        e.set_attr("style", style);
        e
    }

    #[test]
    fn declarations_respect_parens_and_comments() {
        let d = declarations("color: red; /* x;y */ background: url(\"a;b\"); font-weight:bold");
        assert_eq!(d.len(), 3);
        assert_eq!(d[1], ("background".into(), "url(\"a;b\")".into()));
    }

    #[test]
    fn hidden_preheaders_are_detected() {
        assert!(is_hidden(&el("display:none !important")));
        assert!(is_hidden(&el("max-height:0;overflow:hidden;mso-hide:all")));
        assert!(is_hidden(&el("visibility:hidden")));
        assert!(is_hidden(&el("opacity:0")));
        assert!(!is_hidden(&el("font-size:0")), "the inline-block whitespace hack is not hiding (its text is dropped in clean)");
        assert!(!is_hidden(&el("display:block;height:0")));
    }

    #[test]
    fn filter_drops_urls_positions_and_colours() {
        assert_eq!(filter("position:fixed; top:0; background:url(https://t.example/p)", false), None);
        assert_eq!(filter("color:#fff; font-weight:bold", false).as_deref(), Some("font-weight: bold"));
        assert_eq!(filter("color:#fff", true).as_deref(), Some("color: #fff"));
        assert_eq!(filter("border:1px solid #ccc", false).as_deref(), Some("border: 1px solid"));
        assert_eq!(filter("width: ex\\70 ression(alert(1))", false), None);
        assert_eq!(filter("display:none", false), None);
    }

    #[test]
    fn urls_are_found() {
        assert_eq!(urls("background: URL( 'https://a.example/x.png' ) no-repeat"), vec!["https://a.example/x.png"]);
    }
}
