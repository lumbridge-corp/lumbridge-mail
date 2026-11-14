//! The reader: a conversation on a paper card. Earlier messages fold to one line each, long threads fold
//! their middle, quoted history and signatures fold, remote content stays blocked until asked for.
//!
//! Bodies come from `lumbridge-mail-render` (see `crate::reading`); [`Painter`] is the only code that
//! turns its block tree into GPUI elements. The tree has no colours or fonts: everything here uses the
//! paper palette, so mail reads the same in both appearances.

use super::composer::ComposeMode;
use super::widgets::*;
use crate::app::{FileAction, FileState, Load, MailApp, Offer, OpenThread, Verb};
use crate::keymap;
use crate::reading::{self, ImageKey, Rendered};
use crate::remote::Picture;
use crate::theme::{self, Palette};
use gpui::prelude::*;
use gpui::*;
use lumbridge_mail_core::{Address, Attachment, Category, MessageId, MessageMeta};
use lumbridge_mail_render::{Block, Cell as TableCell, Image as MailImage, ImageSource, Link, Span};
use std::cell::Cell;
use std::collections::HashMap;

type ReaderAct = dyn Fn(&mut MailApp, &mut Window, &mut Context<MailApp>);

/// The widest an image is drawn, in px (the card's text column).
pub const MAX_IMAGE_W: f32 = 560.;
/// Quotes deeper than this are drawn without more indent.
const MAX_QUOTE_INDENT: u8 = 4;

/// Paints one message's block tree.
pub struct Painter<'a> {
    pub pal: &'a Palette,
    pub mono: SharedString,
    pub msg: &'a MessageId,
    pub images: &'a HashMap<ImageKey, Load<Picture>>,
    pub weak: WeakEntity<MailApp>,
    n: Cell<usize>,
}

impl<'a> Painter<'a> {
    pub fn new(app: &'a MailApp, msg: &'a MessageId, cx: &mut Context<MailApp>) -> Self {
        Self { pal: &app.pal, mono: app.mono_font.clone(), msg, images: &app.images, weak: cx.entity().downgrade(), n: Cell::new(0) }
    }

    fn next_id(&self, what: &str) -> ElementId {
        let n = self.n.get();
        self.n.set(n + 1);
        ElementId::Name(SharedString::from(format!("{what}-{}-{n}", self.msg)))
    }

    /// Running text with inline styles and clickable links. A deceptive link carries its warning
    /// inline, in red, right after it: you see where it goes before you click.
    pub fn spans(&self, spans: &[Span], color: Hsla) -> AnyElement {
        let pp = self.pal.paper;
        let mut text = String::new();
        let mut highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = Vec::new();
        let mut fonts: Vec<(std::ops::Range<usize>, SharedString)> = Vec::new();
        let mut links: Vec<(std::ops::Range<usize>, Link)> = Vec::new();
        for s in spans {
            if s.text.is_empty() {
                continue;
            }
            let start = text.len();
            text.push_str(&s.text);
            let end = text.len();
            let mut h = HighlightStyle::default();
            let mut styled = false;
            if s.bold {
                h.font_weight = Some(FontWeight::BOLD);
                styled = true;
            }
            if s.italic {
                h.font_style = Some(FontStyle::Italic);
                styled = true;
            }
            if s.strike {
                h.strikethrough = Some(StrikethroughStyle { thickness: px(1.), color: None });
                styled = true;
            }
            if s.code {
                h.background_color = Some(pp.code_bg);
                fonts.push((start..end, self.mono.clone()));
                styled = true;
            }
            if let Some(l) = &s.link {
                let c = if l.warning.is_some() { pp.bad } else { pp.link };
                h.color = Some(c);
                h.underline = Some(UnderlineStyle { thickness: px(1.), color: Some(c.opacity(0.45)), wavy: l.warning.is_some() });
                links.push((start..end, l.clone()));
                styled = true;
            }
            if styled {
                highlights.push((start..end, h));
            }
            if let Some(w) = s.link.as_ref().and_then(|l| l.warning.as_ref()) {
                let note = format!(" ⚠ {}", reading::warning_text(w));
                let a = text.len();
                text.push_str(&note);
                highlights.push((a..text.len(), HighlightStyle {
                    color: Some(pp.bad),
                    font_weight: Some(FontWeight::SEMIBOLD),
                    background_color: Some(pp.bad_bg),
                    ..Default::default()
                }));
            }
        }
        let styled = StyledText::new(text).with_highlights(highlights).with_font_family_overrides(fonts);
        let base = div().text_color(color).line_height(rems(1.55));
        if links.is_empty() {
            return base.child(styled).into_any_element();
        }
        let ranges: Vec<_> = links.iter().map(|(r, _)| r.clone()).collect();
        let targets: Vec<Link> = links.iter().map(|(_, l)| l.clone()).collect();
        let hover_ranges = ranges.clone();
        let hover_targets = targets.clone();
        let (w1, w2) = (self.weak.clone(), self.weak.clone());
        base.child(
            InteractiveText::new(self.next_id("text"), styled)
                .on_click(ranges, move |ix, window, cx| {
                    let link = targets[ix].clone();
                    let _ = w1.update(cx, |this, cx| this.click_link(link, window, cx));
                })
                .on_hover(move |ix, _, _, cx| {
                    let link = ix.and_then(|i| hover_ranges.iter().position(|r| r.contains(&i))).map(|k| hover_targets[k].clone());
                    let _ = w2.update(cx, |this, cx| this.hover_link(link, cx));
                }),
        )
        .into_any_element()
    }

    fn heading(&self, level: u8, spans: &[Span]) -> AnyElement {
        let size = match level {
            1 => 22.,
            2 => 19.,
            3 => 17.,
            _ => 15.,
        };
        div()
            .pt(px(if level <= 2 { 6. } else { 2. }))
            .text_size(px(size))
            .font_weight(if level <= 3 { FontWeight::BOLD } else { FontWeight::SEMIBOLD })
            .line_height(rems(1.3))
            .child(self.spans(spans, self.pal.paper.text))
            .into_any_element()
    }

    fn picture(&self, key: &ImageKey) -> Option<&Load<Picture>> {
        self.images.get(key)
    }

    /// A box the size the image would be, for an image that is not shown (blocked, loading, failed).
    fn placeholder(&self, w: f32, h: f32, glyph: &str, line: String, detail: Option<String>) -> Div {
        let pp = self.pal.paper;
        let h = h.clamp(44., 200.);
        h_row()
            .w(px(w.max(180.)))
            .max_w_full()
            .h(px(h))
            .gap_2()
            .px_3()
            .justify_center()
            .rounded_md()
            .border_1()
            .border_dashed()
            .border_color(pp.grid)
            .bg(pp.well)
            .child(icon(glyph, 16., pp.dim))
            .child(v_col().min_w_0()
                .child(div().truncate().text_sm().text_color(pp.muted).child(line))
                .when_some(detail, |d, t| d.child(div().truncate().text_xs().text_color(pp.dim).child(t))))
    }

    fn image(&self, im: &MailImage) -> AnyElement {
        let pp = self.pal.paper;
        let key = ImageKey::of(&im.source);
        let loaded = key.as_ref().and_then(|k| self.picture(k));
        let (iw, ih) = match (im.width, im.height, loaded) {
            (Some(w), Some(h), _) if w > 0 && h > 0 => (w as f32, h as f32),
            (_, _, Some(Load::Ready(p))) => (p.width as f32, p.height as f32),
            (Some(w), None, _) if w > 0 => (w as f32, w as f32 * 0.5),
            _ => (MAX_IMAGE_W, 160.),
        };
        let scale = (MAX_IMAGE_W / iw).min(1.);
        let (w, h) = (iw * scale, ih * scale);
        let alt = im.alt.clone().filter(|a| !a.trim().is_empty());
        let el: AnyElement = match (&im.source, loaded) {
            (ImageSource::Blocked { host }, _) => {
                self.placeholder(w, h, "image-off", alt.unwrap_or_else(|| "Image".into()), Some(format!("from {host}, not loaded"))).into_any_element()
            }
            (ImageSource::Unavailable, _) | (ImageSource::Cid { attachment: None, .. }, _) => {
                match alt {
                    Some(a) => div().text_sm().italic().text_color(pp.dim).child(format!("[{a}]")).into_any_element(),
                    None => div().into_any_element(),
                }
            }
            (_, Some(Load::Ready(p))) => img(p.image.clone()).w(px(w)).h(px(h)).rounded_sm().into_any_element(),
            (_, Some(Load::Failed(e))) => {
                self.placeholder(w, h, "image-off", alt.unwrap_or_else(|| "Image".into()), Some(format!("couldn't load: {e}"))).into_any_element()
            }
            _ => self.placeholder(w, h, "image", alt.unwrap_or_else(|| "Image".into()), Some("loading…".into())).into_any_element(),
        };
        match &im.link {
            Some(l) => {
                let link = l.clone();
                let weak = self.weak.clone();
                div()
                    .id(self.next_id("imglink"))
                    .cursor_pointer()
                    .child(el)
                    .on_click(move |_, window, cx| {
                        let link = link.clone();
                        let _ = weak.update(cx, |this, cx| this.click_link(link, window, cx));
                    })
                    .into_any_element()
            }
            None => el,
        }
    }

    fn table(&self, rows: &[Vec<TableCell>], header_rows: u32, color: Hsla) -> AnyElement {
        let pp = self.pal.paper;
        let ncols = rows.iter().map(|r| r.iter().map(|c| c.colspan.max(1)).sum::<u32>()).max().unwrap_or(1).max(1);
        v_col()
            .w_full()
            .max_w(px(MAX_IMAGE_W + 80.))
            .rounded_md()
            .border_1()
            .border_color(pp.grid)
            .overflow_hidden()
            .children(rows.iter().enumerate().map(|(ri, row)| {
                let head = (ri as u32) < header_rows || (!row.is_empty() && row.iter().all(|c| c.header));
                div()
                    .flex()
                    .flex_row()
                    .w_full()
                    .when(ri > 0, |d| d.border_t_1().border_color(pp.grid))
                    .when(head, |d| d.bg(pp.well))
                    .children(row.iter().enumerate().map(|(ci, c)| {
                        let frac = c.colspan.max(1) as f32 / ncols as f32;
                        div()
                            .w(relative(frac))
                            .min_w_0()
                            .px_3()
                            .py(px(6.))
                            .when(ci > 0, |d| d.border_l_1().border_color(pp.grid))
                            .text_sm()
                            .when(head || c.header, |d| d.font_weight(FontWeight::SEMIBOLD))
                            .child(self.spans(&c.spans, color))
                    }))
            }))
            .into_any_element()
    }

    /// A list of blocks. `quoted`: inside a quote (muted text).
    pub fn blocks(&self, list: &[Block], quoted: bool) -> Div {
        let pp = self.pal.paper;
        let color = if quoted { pp.muted } else { pp.text };
        v_col().gap_3().min_w_0().children(list.iter().map(|b| match b {
            Block::Paragraph { spans } => self.spans(spans, color),
            Block::Heading { level, spans } => self.heading(*level, spans),
            Block::List { ordered, start, items } => v_col()
                .gap_1()
                .children(items.iter().enumerate().map(|(i, item)| {
                    let marker = if *ordered { format!("{}.", *start as usize + i) } else { "•".into() };
                    h_row()
                        .items_start()
                        .gap_2()
                        .child(div().w(px(22.)).flex_none().text_right().text_color(pp.dim).line_height(rems(1.55)).child(marker))
                        .child(div().flex_1().min_w_0().child(self.blocks(item, quoted).gap_1()))
                }))
                .into_any_element(),
            Block::Quote { depth, blocks } => {
                let inner = self.blocks(blocks, true);
                if *depth > MAX_QUOTE_INDENT {
                    inner.into_any_element()
                } else {
                    let bar = [pp.quote, pp.link.opacity(0.45), pp.good.opacity(0.45), pp.warn.opacity(0.5)][(*depth as usize).saturating_sub(1) % 4];
                    div().pl_3().border_l_2().border_color(bar).child(inner).into_any_element()
                }
            }
            Block::Code { text } => div()
                .w_full()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(pp.code_bg)
                .font_family(self.mono.clone())
                .text_size(px(12.5))
                .line_height(rems(1.45))
                .text_color(color)
                .child(text.clone())
                .into_any_element(),
            Block::Table { rows, header_rows } => self.table(rows, *header_rows, color),
            Block::Image(im) => self.image(im),
            Block::Rule => div().h(px(1.)).w_full().bg(pp.grid).into_any_element(),
        }))
    }
}

fn file_icon(mime: &str) -> &'static str {
    if mime.starts_with("image/") {
        "file-image"
    } else if mime == "application/pdf" {
        "file-text"
    } else if mime.contains("spreadsheet") || mime == "text/csv" {
        "file-spreadsheet"
    } else if mime.contains("zip") {
        "file-archive"
    } else {
        "file"
    }
}

fn names(addrs: &[Address], me: &str) -> String {
    addrs.iter().map(|a| if a.email == me { "me".to_string() } else { a.display().to_string() }).collect::<Vec<_>>().join(", ")
}

/// A small text button on the paper card.
fn paper_link(p: &Palette, id: impl Into<ElementId>, label: impl Into<SharedString>, color: Hsla) -> Stateful<Div> {
    let _ = p;
    div()
        .id(id.into())
        .px_2()
        .h(px(24.))
        .flex()
        .items_center()
        .rounded_sm()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .hover(|s| s.bg(color.opacity(0.1)))
        .child(label.into())
}

impl MailApp {
    /// The message's files: click a chip to open it (or to save it, when it could run code), the
    /// download button to save a copy to Downloads. While a file is fetched (or copied from a daemon on
    /// another machine) its chip shows how far it got.
    fn attachments_row(&self, m: &MessageMeta, list: &[&Attachment], cx: &mut Context<Self>) -> Div {
        let pp = self.pal.paper;
        div().flex().flex_wrap().gap_2().pt_1().children(list.iter().map(|a| {
            let key = (m.account.clone(), m.id.clone(), a.id.clone());
            let danger = lumbridge_mail_core::files::dangerous_to_open(&a.filename, &a.mime);
            let (line, line_color) = match self.files.get(&key) {
                Some(FileState::Copying { done, total }) if *total > 0 => {
                    (format!("Fetching… {}%", (done.saturating_mul(100) / total).min(100)), pp.muted)
                }
                Some(FileState::Copying { .. }) => ("Fetching…".to_string(), pp.muted),
                Some(FileState::Failed(e)) => (format!("Failed: {e}"), pp.bad),
                _ if danger.is_some() => (format!("{} · can run code, save only", crate::time::size(a.size)), pp.warn),
                _ => (crate::time::size(a.size), pp.dim),
            };
            let progress = match self.files.get(&key) {
                Some(FileState::Copying { done, total }) if *total > 0 => Some((*done as f32 / *total as f32).clamp(0., 1.)),
                _ => None,
            };
            let what = if danger.is_some() { FileAction::Save } else { FileAction::Open };
            let (acct, msg, att) = (m.account.clone(), m.id.clone(), (*a).clone());
            let (acct2, msg2, att2) = (m.account.clone(), m.id.clone(), (*a).clone());
            h_row()
                .id(SharedString::from(format!("file-{}-{}", m.id, a.id)))
                .relative()
                .gap_2()
                .w(px(240.))
                .h(px(44.))
                .pl_2()
                .pr_1()
                .rounded_md()
                .border_1()
                .border_color(if danger.is_some() { pp.warn.opacity(0.5) } else { pp.grid })
                .bg(pp.edge)
                .cursor_pointer()
                .hover(|s| s.bg(pp.hover))
                .child(div().size(px(28.)).flex_none().rounded_sm().bg(pp.well).flex().items_center().justify_center()
                    .child(icon(if danger.is_some() { "shield-alert" } else { file_icon(&a.mime) }, 14.,
                                if danger.is_some() { pp.warn } else { pp.muted })))
                .child(v_col().flex_1().min_w_0()
                    .child(div().truncate().text_sm().text_color(pp.text).child(a.filename.clone()))
                    .child(div().truncate().text_xs().text_color(line_color).child(line)))
                .child(
                    div()
                        .id(SharedString::from(format!("save-{}-{}", m.id, a.id)))
                        .size(px(26.))
                        .flex_none()
                        .rounded_sm()
                        .flex()
                        .items_center()
                        .justify_center()
                        .hover(|s| s.bg(pp.grid))
                        .child(icon("download", 14., pp.muted))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.file_action(acct2.clone(), msg2.clone(), att2.clone(), FileAction::Save, cx);
                        })),
                )
                .children(progress.map(|f| {
                    div().absolute().left_0().bottom_0().h(px(2.)).w(relative(f)).rounded_sm().bg(pp.link)
                }))
                .on_click(cx.listener(move |this, _, _, cx| this.file_action(acct.clone(), msg.clone(), att.clone(), what, cx)))
        }))
    }

    fn collapsed_message(&self, m: &MessageMeta, cx: &mut Context<Self>) -> AnyElement {
        let pp = self.pal.paper;
        let from = m.from.first().cloned().unwrap_or_else(|| Address::new(None, "unknown@invalid"));
        let id = m.id.clone();
        h_row()
            .id(SharedString::from(format!("msg-{}", m.id)))
            .gap_3()
            .px_5()
            .h(px(48.))
            .border_t_1()
            .border_color(pp.grid)
            .hover(|s| s.bg(pp.hover))
            .child(avatar(from.display(), person_color(&from.email), 28.))
            .child(div().w(px(150.)).flex_none().truncate().text_sm().font_weight(FontWeight::MEDIUM)
                .text_color(if m.flags.seen { pp.muted } else { pp.text }).child(from.display().to_string()))
            .child(div().flex_1().min_w_0().truncate().text_sm().text_color(pp.dim).child(m.snippet.clone()))
            .child(div().flex_none().text_xs().text_color(pp.dim).child(crate::time::short_date(m.date, self.now)))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_message(&id, cx)))
            .into_any_element()
    }

    /// The bar above a message's body: what was blocked or removed, and how to load images.
    fn content_bars(&self, open: &OpenThread, m: &MessageMeta, r: &Rendered, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let pp = self.pal.paper;
        let report = &r.doc.report;
        let mut out = Vec::new();
        let sender = m.from.first().map(|a| a.email.clone()).unwrap_or_default();
        let anon = self.is_anon(&m.account);
        let bar = |bg: Hsla| h_row().gap_2().px_3().min_h(px(34.)).py_1().rounded_md().bg(bg);

        if r.remote {
            let by_sender = !anon && self.settings.sender_allowed(&sender) && !open.remote_ok.contains(&m.id);
            let mut text = "Images from the web are shown".to_string();
            if by_sender {
                text.push_str(&format!(" (you allowed {sender})"));
            }
            if report.tracking_pixels > 0 {
                text.push_str(&format!("  ·  {} tracker{} still removed", report.tracking_pixels, if report.tracking_pixels == 1 { "" } else { "s" }));
            }
            let s2 = sender.clone();
            out.push(
                bar(pp.well)
                    .child(icon("image", 14., pp.dim))
                    .child(div().flex_1().text_sm().text_color(pp.muted).child(text))
                    .when(by_sender, |d| {
                        d.child(paper_link(&self.pal, SharedString::from(format!("stop-remote-{}", m.id)), "Stop for this sender", pp.muted)
                            .on_click(cx.listener(move |this, _, _, cx| this.forget_remote_sender(&s2, cx))))
                    })
                    .into_any_element(),
            );
        } else if let Some(text) = reading::blocked_summary(report) {
            let can_load = report.remote_blocked && report.remote_images > 0;
            let confirming = open.confirm_remote.as_ref() == Some(&m.id);
            let (id1, id2, id3) = (m.id.clone(), m.id.clone(), m.id.clone());
            let mut b = bar(pp.warn_bg).child(icon("shield-check", 14., pp.warn));
            if confirming {
                b = b
                    .child(div().flex_1().text_sm().text_color(pp.warn).child(
                        "Anon account: loading images tells the sender you opened this, and from which address."))
                    .child(paper_link(&self.pal, SharedString::from(format!("anon-load-{}", m.id)), "Load anyway", pp.warn)
                        .on_click(cx.listener(move |this, _, _, cx| this.allow_remote(&id1, false, true, cx))))
                    .child(paper_link(&self.pal, SharedString::from(format!("anon-cancel-{}", m.id)), "Cancel", pp.muted)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(o) = this.open.as_mut() {
                                o.confirm_remote = None;
                            }
                            cx.notify();
                        })));
            } else {
                b = b.child(div().flex_1().text_sm().text_color(pp.warn).child(text)).when(can_load, |d| {
                    d.child(paper_link(&self.pal, SharedString::from(format!("load-remote-{}", m.id)), "Load images", pp.warn)
                        .on_click(cx.listener(move |this, _, _, cx| this.allow_remote(&id2, false, false, cx))))
                        .when(!anon && !sender.is_empty(), |d| {
                            d.child(paper_link(&self.pal, SharedString::from(format!("always-remote-{}", m.id)), "Always for this sender", pp.warn)
                                .on_click(cx.listener(move |this, _, _, cx| this.allow_remote(&id3, true, false, cx))))
                        })
                });
            }
            out.push(b.into_any_element());
        }
        let deceptive = report.suspicious_links.len();
        if let Some(removed) = reading::removed_summary(report) {
            out.push(
                bar(pp.bad_bg)
                    .child(icon("shield-alert", 14., pp.bad))
                    .child(div().flex_1().text_sm().text_color(pp.bad).child(format!("Removed from this message: {removed}")))
                    .into_any_element(),
            );
        }
        if deceptive > 0 {
            out.push(
                bar(pp.bad_bg)
                    .child(icon("triangle-alert", 14., pp.bad))
                    .child(div().flex_1().text_sm().text_color(pp.bad).child(format!(
                        "{deceptive} link{} here {} not go where {} say{}. Hover a link to see its real address.",
                        if deceptive == 1 { "" } else { "s" }, if deceptive == 1 { "does" } else { "do" },
                        if deceptive == 1 { "it" } else { "they" }, if deceptive == 1 { "s" } else { "" })))
                    .into_any_element(),
            );
        }
        out
    }

    fn expanded_message(&self, open: &OpenThread, m: &MessageMeta, me: &str, cx: &mut Context<Self>) -> AnyElement {
        let p = self.pal;
        let pp = p.paper;
        let from = m.from.first().cloned().unwrap_or_else(|| Address::new(None, "unknown@invalid"));
        let id = m.id.clone();
        let id2 = m.id.clone();
        let to_line = {
            let mut s = format!("to {}", names(&m.to, me));
            if !m.cc.is_empty() {
                s.push_str(&format!("  ·  cc {}", names(&m.cc, me)));
            }
            s
        };
        let rendered = open.rendered.get(&m.id);
        let info = rendered.map(|r| &r.info);
        let warnings = info.map(reading::sender_warnings).unwrap_or_default();
        let verified = info.is_some_and(reading::sender_verified);
        let list_mail = info.is_some_and(|i| i.unsubscribe.is_some());
        let mid = m.id.clone();

        let header = h_row()
            .items_start()
            .gap_3()
            .child(avatar(from.display(), person_color(&from.email), 36.))
            .child(
                v_col()
                    .id(SharedString::from(format!("msgh-{}", m.id)))
                    .flex_1()
                    .min_w_0()
                    .child(h_row().gap_2()
                        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(pp.text).child(from.display().to_string()))
                        .child(div().truncate().text_xs().text_color(pp.dim).child(format!("<{}>", from.email)))
                        .when(verified, |d| d.child(h_row().gap_0p5().child(icon("badge-check", 12., pp.good))
                            .child(div().text_xs().text_color(pp.good).child("verified")))))
                    .child(div().truncate().text_xs().text_color(pp.dim).child(to_line))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_message(&id, cx))),
            )
            .when(list_mail, |d| {
                d.child(
                    paper_button(&p, SharedString::from(format!("unsub-{}", m.id)), "mail-x", Some("Unsubscribe"))
                        .on_click(cx.listener(move |this, _, w, cx| this.unsubscribe(&mid, w, cx))),
                )
            })
            .child(div().flex_none().pt(px(4.)).text_xs().text_color(pp.dim).child(crate::time::long_date(m.date)));

        let warning_chips = (!warnings.is_empty()).then(|| {
            h_row().gap_2().flex_wrap().pl(px(48.)).children(warnings.into_iter().map(|w| {
                h_row()
                    .gap_1()
                    .px_2()
                    .h(px(22.))
                    .rounded(px(11.))
                    .bg(pp.bad_bg)
                    .border_1()
                    .border_color(pp.bad.opacity(0.35))
                    .child(icon("triangle-alert", 12., pp.bad))
                    .child(div().text_xs().font_weight(FontWeight::MEDIUM).text_color(pp.bad).child(w))
            }))
        });

        let body = match (open.bodies.get(&m.id), rendered) {
            (Some(Load::Failed(e)), _) => {
                div().text_sm().text_color(p.blocked).child(format!("Couldn't load this message: {e}")).into_any_element()
            }
            (Some(Load::Ready(b)), Some(r)) => {
                let painter = Painter::new(self, &m.id, cx);
                let show_quoted = open.show_quoted.contains(&m.id);
                let folded = !r.doc.quoted.is_empty() || !r.doc.signature.is_empty();
                let inline: Vec<String> = reading::images(&r.doc)
                    .into_iter()
                    .filter_map(|s| match s {
                        ImageSource::Cid { attachment: Some(a), .. } => Some(a.to_string()),
                        _ => None,
                    })
                    .collect();
                let files: Vec<&Attachment> = b.attachments.iter().filter(|a| !inline.contains(&a.id.to_string())).collect();
                let bars = self.content_bars(open, m, r, cx);
                let main = painter.blocks(&r.doc.blocks, false);
                // The folded parts: the signature in dim text, the quoted history behind a bar.
                let rest = show_quoted.then(|| {
                    v_col()
                        .gap_3()
                        .when(!r.doc.signature.is_empty(), |d| d.child(div().text_sm().child(painter.blocks(&r.doc.signature, true))))
                        .when(!r.doc.quoted.is_empty(), |d| {
                            d.child(div().pl_3().border_l_2().border_color(pp.quote).text_sm().child(painter.blocks(&r.doc.quoted, true)))
                        })
                });
                v_col()
                    .gap_3()
                    .children(bars)
                    .child(main)
                    .when(folded, |d| {
                        d.child(
                            h_row()
                                .id(SharedString::from(format!("quoted-{}", m.id)))
                                .gap_1()
                                .px_2()
                                .h(px(20.))
                                .w(px(if show_quoted { 120. } else { 136. }))
                                .rounded_sm()
                                .bg(pp.well)
                                .text_xs()
                                .text_color(pp.muted)
                                .hover(|s| s.bg(pp.grid))
                                .child(icon(if show_quoted { "chevron-up" } else { "ellipsis" }, 12., pp.muted))
                                .child(if show_quoted { "Hide quoted" } else { "Show quoted text" })
                                .on_click(cx.listener(move |this, _, _, cx| this.toggle_quoted(&id2, cx))),
                        )
                    })
                    .children(rest)
                    .when(!files.is_empty(), |d| d.child(self.attachments_row(m, &files, cx)))
                    .into_any_element()
            }
            _ => v_col().gap_2().pt_1()
                .child(div().h(px(10.)).w(relative(0.8)).rounded_sm().bg(pp.well))
                .child(div().h(px(10.)).w(relative(0.6)).rounded_sm().bg(pp.well))
                .child(div().h(px(10.)).w(relative(0.7)).rounded_sm().bg(pp.well))
                .into_any_element(),
        };

        v_col()
            .gap_3()
            .px_5()
            .py_4()
            .border_t_1()
            .border_color(pp.grid)
            .child(header)
            .children(warning_chips)
            .child(div().pl(px(48.)).child(body))
            .into_any_element()
    }

    fn reader_toolbar(&self, open: &OpenThread, cx: &mut Context<Self>) -> Div {
        let p = self.pal;
        let pp = p.paper;
        let flagged = open.summary.flagged;
        // With the AI panel open the card is narrow: icons only.
        let wide = !self.ai_panel;
        let action = |id: &'static str, glyph: &'static str, label: Option<&'static str>, keys: &'static str,
                      act: Box<ReaderAct>, cx: &mut Context<Self>| {
            let keys = keymap::display_keys(keys).join(" ");
            paper_button(&p, id, glyph, label)
                .child(div().text_size(px(10.)).text_color(pp.dim).child(keys))
                .on_click(cx.listener(move |this, _, window, cx| act(this, window, cx)))
        };
        // What this account can do (`accounts.capabilities`): what it cannot is not offered, what changes
        // only this device's copy says so, and a read-only account shows its actions disabled with why.
        let acct = open.summary.account.clone();
        let changes = [
            ("r-archive", "archive", "e", Verb::Archive, "archive"),
            ("r-trash", "trash", "#", Verb::Trash, "trash"),
            ("r-star", if flagged { "star-fill" } else { "star" }, "s", Verb::Star, "star"),
            ("r-unread", "mail", "u", Verb::Read, "read state"),
        ];
        let mut row = h_row().gap_1p5().px_5().h(px(52.));
        let mut local: Vec<&str> = Vec::new();
        let mut disabled = false;
        for (id, glyph, keys, verb, what) in changes {
            let act: Box<ReaderAct> = match verb {
                Verb::Archive => Box::new(|this, w, cx| this.remove_targets(false, w, cx)),
                Verb::Trash => Box::new(|this, w, cx| this.remove_targets(true, w, cx)),
                Verb::Star => Box::new(|this, _, cx| this.toggle_star(cx)),
                _ => Box::new(|this, _, cx| this.toggle_read(cx)),
            };
            match self.offer(&acct, verb) {
                Offer::Hidden => {}
                Offer::Disabled(_) => {
                    let keys = keymap::display_keys(keys).join(" ");
                    row = row.child(
                        paper_button(&p, id, glyph, None)
                            .opacity(0.45)
                            .cursor_not_allowed()
                            .child(div().text_size(px(10.)).text_color(pp.dim).child(keys)),
                    );
                    disabled = true;
                }
                Offer::Local => {
                    local.push(what);
                    row = row.child(action(id, glyph, None, keys, act, cx));
                }
                Offer::Server => row = row.child(action(id, glyph, None, keys, act, cx)),
            }
        }
        let note = |id: &'static str, glyph: &'static str, text: String| {
            h_row()
                .id(id)
                .gap_1p5()
                .px_2()
                .h(px(26.))
                .min_w_0()
                .rounded_md()
                .bg(pp.well)
                .child(icon(glyph, 12., pp.muted))
                .child(div().truncate().text_xs().text_color(pp.muted).child(text))
        };
        if disabled {
            // The full reason is the toast a key press shows; the bar has room for the gist.
            row = row.child(note("r-readonly", "lock", "Read-only account".into()));
        } else if !local.is_empty() {
            row = row.child(note("r-local", "monitor-smartphone", format!("On this device only: {}", local.join(", "))));
        }
        let reply = self.offer(&acct, Verb::Reply) != Offer::Hidden;
        row
            .child(div().flex_1())
            .when(reply, |row| {
                row.child(action("r-reply", "reply", wide.then_some("Reply"), "r", Box::new(|this, w, cx| this.open_composer(ComposeMode::Reply, w, cx)), cx))
                    .child(action("r-replyall", "reply-all", wide.then_some("Reply all"), "a", Box::new(|this, w, cx| this.open_composer(ComposeMode::ReplyAll, w, cx)), cx))
                    .child(action("r-forward", "forward", wide.then_some("Forward"), "f", Box::new(|this, w, cx| this.open_composer(ComposeMode::Forward, w, cx)), cx))
            })
            .child(
                paper_button(&p, "r-ai", "sparkles", None)
                    .when(self.ai_panel, |d| d.bg(p.accent.opacity(0.12)).border_color(p.accent.opacity(0.5)))
                    .child(div().text_size(px(10.)).text_color(pp.dim).child("I"))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_ai_panel(cx))),
            )
            .child(
                paper_button(&p, "r-close", "x", None)
                    .on_click(cx.listener(|this, _, w, cx| this.close_reader(w, cx))),
            )
    }

    /// Where the link under the mouse really goes, at the foot of the card (like a browser's status line).
    fn link_line(&self) -> Option<Div> {
        let pp = self.pal.paper;
        let link = self.hovered_link.as_ref()?;
        let warn = link.warning.as_ref().map(reading::warning_text);
        Some(
            h_row()
                .absolute()
                .bottom(px(8.))
                .left(px(12.))
                .max_w(relative(0.9))
                .gap_2()
                .px_2()
                .h(px(24.))
                .rounded_md()
                .bg(if warn.is_some() { pp.bad_bg } else { pp.edge })
                .border_1()
                .border_color(if warn.is_some() { pp.bad.opacity(0.4) } else { pp.grid })
                .shadow_sm()
                .child(icon(if warn.is_some() { "triangle-alert" } else { "link" }, 12., if warn.is_some() { pp.bad } else { pp.dim }))
                .child(div().truncate().text_xs().text_color(if warn.is_some() { pp.bad } else { pp.muted }).child(link.href.clone()))
                .when_some(warn, |d, w| d.child(div().flex_none().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(pp.bad).child(w))),
        )
    }

    /// The "this link is not what it says" dialog, before opening a deceptive link.
    pub fn render_link_confirm(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.pal;
        let pp = p.paper;
        let link = self.confirm_link.clone()?;
        let warn = link.warning.as_ref().map(reading::warning_text).unwrap_or_default();
        Some(
            div()
                .id("link-confirm-backdrop")
                .absolute()
                .inset_0()
                .bg(p.backdrop)
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.confirm_link = None;
                    cx.notify();
                }))
                .child(
                    v_col()
                        .id("link-confirm")
                        .w(px(520.))
                        .gap_3()
                        .p_5()
                        .rounded_xl()
                        .bg(pp.card)
                        .border_1()
                        .border_color(pp.bad.opacity(0.5))
                        .shadow_2xl()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(h_row().gap_2().child(icon("triangle-alert", 18., pp.bad))
                            .child(div().text_base().font_weight(FontWeight::SEMIBOLD).text_color(pp.text).child("This link is not what it says")))
                        .child(div().text_sm().text_color(pp.muted).child(format!("It {warn}.")))
                        .child(div().px_3().py_2().rounded_md().bg(pp.well).text_sm().font_family(self.mono_font.clone())
                            .text_color(pp.text).child(link.href.clone()))
                        .child(h_row().gap_2().justify_end()
                            .child(paper_button(&p, "link-cancel", "x", Some("Don't open"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.confirm_link = None;
                                    cx.notify();
                                })))
                            .child(paper_button(&p, "link-open", "external-link", Some("Open anyway"))
                                .text_color(pp.bad)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(l) = this.confirm_link.take() {
                                        cx.open_url(&l.href);
                                    }
                                    cx.notify();
                                })))),
                )
                .into_any_element(),
        )
    }

    pub fn render_reader(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.pal;
        let pp = p.paper;
        let Some(open) = &self.open else {
            return v_col()
                .flex_1()
                .h_full()
                .items_center()
                .justify_center()
                .gap_3()
                .bg(p.canvas)
                .child(icon("mail-open", 28., p.dim))
                .child(div().text_sm().text_color(p.muted).child("Pick a conversation"))
                .child(h_row().gap_2().text_xs().text_color(p.dim)
                    .child(keys_hint(&p, &["J".into()])).child(keys_hint(&p, &["K".into()])).child("to move")
                    .child(keys_hint(&p, &["Enter".into()])).child("to open")
                    .child(keys_hint(&p, &keymap::display_keys("secondary-k"))).child("for everything else"))
                .into_any_element();
        };
        let s = &open.summary;
        let acct = self.account(&s.account).cloned();
        let me = acct.as_ref().map(|a| a.email.clone()).unwrap_or_default();
        let triage = s.triage.clone();
        let sensitive = triage.as_ref().is_some_and(|t| t.category == Category::Sensitive);

        let chips = {
            let mut row = h_row().gap_2().flex_wrap();
            if let Some(a) = &acct {
                row = row.child(h_row().gap_1p5().child(dot(p.account(a.color.as_deref()), 7.))
                    .child(div().text_xs().text_color(pp.muted).child(format!("{} · {}", a.label, a.email))));
            }
            if let Some(t) = &triage {
                let (fg, _) = Palette::light().category(t.category);
                row = row.child(chip(theme::category_label(t.category), fg, fg.opacity(0.1)));
                if t.needs_reply {
                    let (fg, _) = Palette::light().category(Category::Important);
                    row = row.child(chip("Needs a reply", fg, fg.opacity(0.1)));
                }
                if let Some(due) = t.due {
                    row = row.child(chip(format!("Due {}", crate::time::due(due, self.now)), pp.warn, pp.warn_bg));
                }
                if t.suspicious {
                    let red = Palette::light().blocked;
                    row = row.child(chip("Tries to instruct an assistant", red, red.opacity(0.1)));
                }
            }
            row
        };
        // Triage.suspicious: the mail seems to give orders to an AI. Say it plainly, above the summary.
        let suspicious_band = triage.as_ref().filter(|t| t.suspicious).map(|_| {
            let red = Palette::light().blocked;
            h_row()
                .items_start()
                .gap_2()
                .mx_5()
                .mt_1()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(red.opacity(0.08))
                .child(div().pt(px(2.)).child(icon("shield-alert", 13., red)))
                .child(div().flex_1().min_w_0().text_sm().text_color(pp.text).child(
                    "This mail seems to try to give instructions to an AI assistant. Treat it with care: nothing in it is followed.",
                ))
        });

        let ai_band = triage.as_ref().filter(|t| !t.summary.is_empty()).map(|t| {
            h_row()
                .items_start()
                .gap_2()
                .mx_5()
                .mt_1()
                .mb_3()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(if sensitive { pp.warn_bg } else { pp.well })
                .child(div().pt(px(2.)).child(icon(if sensitive { "lock" } else { "sparkles" }, 13., if sensitive { pp.warn } else { p.accent })))
                .child(div().flex_1().min_w_0().text_sm().text_color(if sensitive { pp.warn } else { pp.muted }).child(if sensitive {
                    format!("{}. Kept away from AI, notifications and logs.", t.summary)
                } else {
                    t.summary.clone()
                }))
                .when(!sensitive, |d| d.child(div().text_xs().text_color(pp.dim).child(t.by.clone())))
        });

        let messages: AnyElement = match &open.messages {
            Load::Loading => v_col().p_5().gap_2()
                .child(div().h(px(12.)).w(relative(0.5)).rounded_sm().bg(pp.well))
                .child(div().h(px(12.)).w(relative(0.8)).rounded_sm().bg(pp.well))
                .into_any_element(),
            Load::Failed(e) => div().p_5().text_sm().text_color(p.blocked).child(format!("Couldn't open this conversation: {e}")).into_any_element(),
            Load::Ready(msgs) => {
                let n = msgs.len();
                let fold_middle = n > 4 && !open.show_middle;
                let mut col = v_col();
                for (i, m) in msgs.iter().enumerate() {
                    if fold_middle && i > 0 && i < n - 2 {
                        if i == 1 {
                            let hidden = n - 3;
                            col = col.child(
                                h_row()
                                    .id("show-middle")
                                    .gap_3()
                                    .px_5()
                                    .h(px(34.))
                                    .border_t_1()
                                    .border_color(pp.grid)
                                    .hover(|s| s.bg(pp.hover))
                                    .child(div().w(px(28.)).flex().justify_center().child(
                                        div().size(px(22.)).rounded_full().border_1().border_color(pp.grid).bg(pp.edge)
                                            .flex().items_center().justify_center().text_xs().text_color(pp.muted).child(hidden.to_string())))
                                    .child(div().text_sm().text_color(pp.muted).child(format!("{hidden} earlier messages")))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(o) = this.open.as_mut() {
                                            o.show_middle = true;
                                        }
                                        cx.notify();
                                    })),
                            );
                        }
                        continue;
                    }
                    col = col.child(if open.expanded.contains(&m.id) {
                        self.expanded_message(open, m, &me, cx)
                    } else {
                        self.collapsed_message(m, cx)
                    });
                }
                col.into_any_element()
            }
        };

        let quick = h_row()
            .gap_2()
            .px_5()
            .py_4()
            .border_t_1()
            .border_color(pp.grid)
            .child(paper_button(&p, "q-reply", "reply", Some("Reply"))
                .on_click(cx.listener(|this, _, w, cx| this.open_composer(ComposeMode::Reply, w, cx))))
            .child(paper_button(&p, "q-replyall", "reply-all", Some("Reply all"))
                .on_click(cx.listener(|this, _, w, cx| this.open_composer(ComposeMode::ReplyAll, w, cx))))
            .child(paper_button(&p, "q-forward", "forward", Some("Forward"))
                .on_click(cx.listener(|this, _, w, cx| this.open_composer(ComposeMode::Forward, w, cx))));

        let card = v_col()
            .id("reader-card")
            .relative()
            .flex_1()
            .min_h_0()
            .rounded_lg()
            .bg(pp.card)
            .border_1()
            .border_color(pp.edge)
            .shadow_md()
            .overflow_hidden()
            .child(self.reader_toolbar(open, cx))
            .child(
                div()
                    .id("reader-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.reader_scroll)
                    .child(
                        v_col()
                            .flex_none()
                            .pb_2()
                            .child(v_col().gap_2().px_5().pt_1().pb_3()
                                .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).text_color(pp.text).line_height(rems(1.35))
                                    .child(if s.subject.is_empty() { "(no subject)".to_string() } else { s.subject.clone() }))
                                .child(chips))
                            .children(suspicious_band)
                            .children(ai_band)
                            .child(messages)
                            .child(quick),
                    ),
            )
            .children(self.link_line());

        div().flex_1().min_w_0().h_full().p_3().bg(p.canvas).flex().flex_col().child(card).into_any_element()
    }
}
