//! A received message turned into what the reader paints, through `lumbridge-mail-render` only
//! (CONTRIBUTING.md rule 5): the block tree ([`Document`]: blocks, folded signature and quoted history, the
//! [`Report`] of what was blocked) and the header facts ([`HeaderInfo`]: unsubscribe, Reply-To,
//! sender authentication). The painter is `ui/reader.rs`.

use lumbridge_mail_core::{Address, AttachmentId, Body};
use lumbridge_mail_render::{self as render, Block, Document, HeaderInfo, ImageSource, Policy, Trust};
pub use lumbridge_mail_render::Report;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub doc: Document,
    pub info: HeaderInfo,
    /// Rendered with remote content allowed.
    pub remote: bool,
}

/// Render a body. `remote`: the person allowed remote images for this message (or its sender).
pub fn render(body: &Body, from: Option<&Address>, remote: bool) -> Rendered {
    let policy = if remote { Policy::remote_allowed() } else { Policy::default() };
    Rendered { doc: render::blocks(body, &policy), info: render::header_info(body, from), remote }
}

/// An image the reader has to fetch or decode, and the key it is cached under.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ImageKey {
    Remote(String),
    Part(AttachmentId),
    Data(String),
}

impl ImageKey {
    pub fn of(source: &ImageSource) -> Option<ImageKey> {
        match source {
            ImageSource::Remote { url } => Some(ImageKey::Remote(url.clone())),
            ImageSource::Cid { attachment: Some(id), .. } => Some(ImageKey::Part(id.clone())),
            ImageSource::Data { uri } => Some(ImageKey::Data(uri.clone())),
            _ => None,
        }
    }
}

fn walk<'a>(blocks: &'a [Block], out: &mut Vec<&'a ImageSource>) {
    for b in blocks {
        match b {
            Block::Image(i) => out.push(&i.source),
            Block::List { items, .. } => items.iter().for_each(|it| walk(it, out)),
            Block::Quote { blocks, .. } => walk(blocks, out),
            _ => {}
        }
    }
}

/// Every image source in the document, in reading order (new content, then signature, then quoted).
pub fn images(doc: &Document) -> Vec<&ImageSource> {
    let mut out = Vec::new();
    walk(&doc.blocks, &mut out);
    walk(&doc.signature, &mut out);
    walk(&doc.quoted, &mut out);
    out
}

/// The "remote content" bar's sentence, or None when there is nothing to say.
pub fn blocked_summary(r: &Report) -> Option<String> {
    let mut parts = Vec::new();
    let plural = |n: u32, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    if r.remote_blocked && r.remote_images > 0 {
        parts.push(format!("{} blocked", plural(r.remote_images, "remote image", "remote images")));
    }
    if r.tracking_pixels > 0 {
        parts.push(format!("{} removed", plural(r.tracking_pixels, "tracker", "trackers")));
    }
    if r.remote_css > 0 {
        parts.push(format!("{} blocked", plural(r.remote_css, "remote style", "remote styles")));
    }
    (!parts.is_empty()).then(|| parts.join("  ·  "))
}

/// Active content the sanitiser took out, in a few words ("1 script, 1 form"), or None.
pub fn removed_summary(r: &Report) -> Option<String> {
    let m = &r.removed;
    let mut parts = Vec::new();
    for (n, one, many) in [
        (m.scripts, "script", "scripts"),
        (m.event_handlers, "event handler", "event handlers"),
        (m.forms, "form", "forms"),
        (m.frames, "frame", "frames"),
        (m.meta_refresh, "redirect", "redirects"),
        (m.unsafe_links, "unsafe link", "unsafe links"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {}", if n == 1 { one } else { many }));
        }
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Warnings about the sender for the message header: Reply-To elsewhere, failed authentication.
pub fn sender_warnings(info: &HeaderInfo) -> Vec<String> {
    let mut out = Vec::new();
    if info.reply_to_differs {
        let to = info.reply_to.first().map(|a| a.domain().to_string()).unwrap_or_default();
        out.push(format!("Replies go to {to}"));
    }
    if info.auth.as_ref().is_some_and(|a| a.trust == Trust::Failed) {
        out.push("Sender not verified: authentication failed".to_string());
    }
    out
}

pub fn sender_verified(info: &HeaderInfo) -> bool {
    info.auth.as_ref().is_some_and(|a| a.trust == Trust::Verified)
}

/// Text for a link's warning: where it really goes, said plainly.
pub fn warning_text(w: &render::WarningKind) -> String {
    use render::WarningKind as W;
    match w {
        W::TextShowsOtherDomain { shown, actual } => format!("shows {shown}, goes to {actual}"),
        W::Punycode { host, unicode } => format!("look-alike address {unicode} ({host})"),
        W::IpAddress { host } => format!("goes to a bare IP address {host}"),
        W::Userinfo { actual } => format!("really goes to {actual}"),
    }
}

/// The host a link goes to, for the hover line.
pub fn link_host(href: &str) -> String {
    render::links::host_of(href).unwrap_or_else(|| href.split(':').next().unwrap_or("").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumbridge_mail_core::Attachment;

    fn html(h: &str) -> Body {
        Body { html: Some(h.into()), ..Default::default() }
    }

    #[test]
    fn remote_images_are_blocked_until_allowed_and_trackers_always_go() {
        let b = html(r#"<p>Hi</p><img src="https://cdn.x.example/hero.png" alt="Hero" width="600" height="300"><img src="https://t.x.example/o.gif" width="1" height="1">"#);
        let r = render(&b, None, false);
        assert!(matches!(images(&r.doc)[..], [ImageSource::Blocked { .. }]));
        assert_eq!(blocked_summary(&r.doc.report).as_deref(), Some("1 remote image blocked  ·  1 tracker removed"));
        let r = render(&b, None, true);
        assert!(matches!(images(&r.doc)[..], [ImageSource::Remote { .. }]));
        assert_eq!(ImageKey::of(images(&r.doc)[0]), Some(ImageKey::Remote("https://cdn.x.example/hero.png".into())));
        assert_eq!(blocked_summary(&r.doc.report).as_deref(), Some("1 tracker removed"));
    }

    #[test]
    fn inline_parts_resolve_to_their_attachment() {
        let mut b = html(r#"<p><img src="cid:plan@x.example" alt="Plan" width="400" height="200"></p>"#);
        b.attachments.push(Attachment { id: AttachmentId::new("a0"), filename: "plan.png".into(), mime: "image/png".into(),
                                        size: 10, content_id: Some("plan@x.example".into()) });
        let r = render(&b, None, false);
        assert_eq!(ImageKey::of(images(&r.doc)[0]), Some(ImageKey::Part(AttachmentId::new("a0"))));
    }

    #[test]
    fn hostile_mail_is_summarised_and_its_sender_flagged() {
        let mut b = html(r#"<script>x()</script><form><input></form><a href="https://evil.example/login">https://bank.example/login</a>"#);
        b.headers = vec![
            ("Reply-To".into(), "x@evil.example".into()),
            ("Authentication-Results".into(), "mx.example; spf=fail smtp.mailfrom=bank-alerts.example; dmarc=fail header.from=bank-alerts.example".into()),
        ];
        let from = Address::new(Some("Bank"), "security@bank-alerts.example");
        let r = render(&b, Some(&from), false);
        assert_eq!(removed_summary(&r.doc.report).as_deref(), Some("1 script, 1 form"));
        let w = sender_warnings(&r.info);
        assert_eq!(w, vec!["Replies go to evil.example".to_string(), "Sender not verified: authentication failed".to_string()]);
        let Block::Paragraph { spans } = &r.doc.blocks[0] else { panic!("{:?}", r.doc.blocks) };
        let link = spans[0].link.as_ref().unwrap();
        assert_eq!(link.href, "https://evil.example/login");
        assert!(warning_text(link.warning.as_ref().unwrap()).contains("evil.example"));
        assert_eq!(link_host(&link.href), "evil.example");
    }
}
