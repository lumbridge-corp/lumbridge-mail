//! The composer: new, reply, reply all, forward. From picker across identities, recipient chips with
//! suggestions (behind the anon wall, `crate::contacts`), Markdown-ish formatting, attachments (a
//! forward carries the original's), an "Ask AI to draft" box, and Send (with an undo delay, in `app.rs`).
//!
//! The editor holds only what the person writes. The original of a reply or forward is kept as a
//! `lumbridge_mail_render::Original`, previewed under the editor, and quoted by `compose` when the draft
//! is made: a `text/plain` part and, in Formatted mode, a `text/html` part.

use super::widgets::*;
use crate::app::MailApp;
use crate::keymap::{self, COMPOSER_CONTEXT};
use gpui::prelude::*;
use gpui::*;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::Sizable;
use lumbridge_mail_core::rpc::AttachmentRef;
use lumbridge_mail_core::{Account, Address, Attachment, Body, Draft, DraftAttachment, MessageId, Persona, ThreadId};
use lumbridge_mail_render::{self as render, Block, Original, Policy, QuoteKind};
use std::path::Path;

/// Recipient suggestions shown under a field.
pub const SUGGESTIONS: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeMode {
    New,
    Reply,
    ReplyAll,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    To,
    Cc,
    Bcc,
}

pub struct Composer {
    pub mode: ComposeMode,
    /// Index into `MailApp::accounts`.
    pub from: usize,
    pub from_open: bool,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub to_input: Entity<InputState>,
    pub cc_input: Entity<InputState>,
    pub bcc_input: Entity<InputState>,
    pub show_cc_bcc: bool,
    pub subject: Entity<InputState>,
    pub body: Entity<TextareaState>,
    pub formatted: bool,
    pub attachments: Vec<DraftAttachment>,
    pub ai_input: Entity<InputState>,
    pub ai_busy: bool,
    /// The conversation being answered or forwarded.
    pub thread: Option<ThreadId>,
    pub thread_account: Option<usize>,
    pub reply_to: Option<MessageId>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// The message being answered or forwarded, quoted by `compose` under what is written.
    pub original: Option<Original>,
    /// Its new content as blocks, for the preview under the editor (images left out).
    pub original_preview: Vec<Block>,
    pub show_original: bool,
    /// The original's attachments still being fetched for a forward.
    pub forward_parts: Vec<(AttachmentRef, Attachment)>,
    pub fetching: usize,
    /// The recipient field whose suggestions are showing.
    pub suggest_field: Option<Field>,
    pub local_id: String,
    pub error: Option<String>,
    _subs: Vec<Subscription>,
}

/// Split typed recipients ("Ada <ada@x.example>, bob@y.example") into addresses and what did not parse.
pub fn parse_addresses(text: &str) -> (Vec<Address>, String) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    // Split on commas and semicolons that are not inside quotes or angle brackets.
    let mut parts = Vec::new();
    let (mut cur, mut quoted, mut angle) = (String::new(), false, false);
    for ch in text.chars() {
        match ch {
            '"' => quoted = !quoted,
            '<' => angle = true,
            '>' => angle = false,
            ',' | ';' | '\n' if !quoted && !angle => {
                parts.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    parts.push(cur);
    for part in &parts {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (name, email) = match (part.find('<'), part.rfind('>')) {
            (Some(a), Some(b)) if a < b => (Some(part[..a].trim().trim_matches('"')), part[a + 1..b].trim()),
            _ => (None, part),
        };
        let valid = email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
                && !email.contains(char::is_whitespace)
        });
        if valid {
            ok.push(Address::new(name, email));
        } else {
            bad.push(part);
        }
    }
    (ok, bad.join(", "))
}

/// Strip images out of blocks (the preview of a quoted original shows text only).
fn without_images(blocks: &[Block]) -> Vec<Block> {
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::Image(_) => None,
            Block::Quote { depth, blocks } => Some(Block::Quote { depth: *depth, blocks: without_images(blocks) }),
            Block::List { ordered, start, items } => {
                Some(Block::List { ordered: *ordered, start: *start, items: items.iter().map(|i| without_images(i)).collect() })
            }
            other => Some(other.clone()),
        })
        .collect()
}

/// Text and HTML for what was written (Markdown-ish) plus the quoted original, through the render
/// crate's `compose`. Plain mode keeps only the text part.
pub fn outgoing(markdown: &str, original: Option<&Original>, formatted: bool) -> (String, Option<String>) {
    let out = render::compose(markdown, original);
    if formatted {
        return (out.text, Some(out.html));
    }
    // Plain: what was typed, as typed, then the plain quote.
    let quote = original.map(|o| render::compose("", Some(o)).text).unwrap_or_default();
    let typed = markdown.trim_end();
    let text = match (typed.is_empty(), quote.trim().is_empty()) {
        (_, true) => format!("{typed}\n"),
        (true, false) => quote,
        (false, false) => format!("{typed}\n\n{quote}"),
    };
    (text, None)
}

fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref() {
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("txt" | "md") => "text/plain",
        Some("csv") => "text/csv",
        Some("zip") => "application/zip",
        Some("ics") => "text/calendar",
        _ => "application/octet-stream",
    }
}

impl Composer {
    pub fn new(app: &MailApp, mode: ComposeMode, window: &mut Window, cx: &mut Context<MailApp>) -> Self {
        let input = |cx: &mut Context<MailApp>, window: &mut Window, placeholder: &'static str| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let to_input = input(cx, window, "Name or address");
        let cc_input = input(cx, window, "Cc");
        let bcc_input = input(cx, window, "Bcc");
        let ai_input = input(cx, window, "Ask AI to draft, e.g. \"say yes, Saturday works\"");

        let mut from = app.default_from();
        let mut to = Vec::new();
        let mut cc = Vec::new();
        let mut subject = String::new();
        let mut thread = None;
        let mut thread_account = None;
        let mut reply_to = None;
        let mut in_reply_to = None;
        let mut references = Vec::new();

        let mut original = None;
        let mut forward_parts = Vec::new();
        if let Some(open) = app.open.as_ref().filter(|_| mode != ComposeMode::New) {
            let msgs = open.messages.ready().cloned().unwrap_or_default();
            if let Some(acct) = app.accounts.iter().position(|a| a.id == open.summary.account) {
                from = acct;
                thread_account = Some(acct);
            }
            let me = app.accounts.get(from).map(|a| a.email.clone()).unwrap_or_default();
            thread = Some(open.summary.id.clone());
            if let Some(last) = msgs.last() {
                let body = open
                    .bodies
                    .get(&last.id)
                    .and_then(|b| b.ready())
                    .cloned()
                    .unwrap_or_else(|| Body { text: Some(last.snippet.clone()), ..Default::default() });
                let base = if last.subject.trim().is_empty() { open.summary.subject.clone() } else { last.subject.clone() };
                let kind = if mode == ComposeMode::Forward { QuoteKind::Forward } else { QuoteKind::Reply };
                subject = render::compose::subject_for(kind, &base);
                if mode == ComposeMode::Forward {
                    // A forward carries the original's files: fetched to local paths by `MailApp::open_composer`.
                    forward_parts = body
                        .attachments
                        .iter()
                        .map(|a| (AttachmentRef { account: last.account.clone(), message: last.id.clone(), attachment: a.id.clone() }, a.clone()))
                        .collect();
                } else {
                    let from_me = last.from.iter().any(|a| a.email == me);
                    to = if from_me { last.to.clone() } else { last.from.clone() };
                    if mode == ComposeMode::ReplyAll {
                        for a in last.to.iter().chain(last.from.iter()) {
                            if a.email != me && !to.contains(a) {
                                to.push(a.clone());
                            }
                        }
                        cc = last.cc.iter().filter(|a| a.email != me && !to.contains(a)).cloned().collect();
                    }
                    reply_to = Some(last.id.clone());
                    in_reply_to = last.rfc_message_id.clone();
                    references = last.references.clone();
                    references.extend(last.rfc_message_id.clone());
                }
                let offset = crate::time::offset_at(last.date) as i32;
                original = Some(Original::from_meta(kind, last, body, offset));
            }
        }
        let original_preview = original
            .as_ref()
            .map(|o| without_images(&render::blocks(&o.body, &Policy::default()).blocks))
            .unwrap_or_default();

        let subject_input = cx.new(|cx| InputState::new(window, cx).placeholder("Subject").default_value(subject));
        let body = cx.new(|cx| TextareaState::new(window, cx).placeholder("Write your message…"));

        let mut subs = Vec::new();
        for (field, state) in [(Field::To, &to_input), (Field::Cc, &cc_input), (Field::Bcc, &bcc_input)] {
            subs.push(cx.subscribe_in(state, window, move |this: &mut MailApp, _, ev: &InputEvent, window, cx| {
                // Enter on a name (not an address) takes the first suggestion.
                let first = this.suggestions(field, cx).into_iter().next();
                let Some(c) = this.composer.as_mut() else { return };
                match ev {
                    InputEvent::PressEnter { .. } => {
                        let typed = c.input(field).read(cx).value().to_string();
                        match first {
                            Some(a) if !typed.contains('@') => c.add_recipient(field, a, window, cx),
                            _ => c.commit(field, window, cx),
                        }
                    }
                    InputEvent::Blur => c.commit(field, window, cx),
                    InputEvent::Change => {
                        c.suggest_field = Some(field);
                        let v = c.input(field).read(cx).value();
                        if v.contains(',') || v.contains(';') {
                            c.commit(field, window, cx);
                        }
                    }
                    InputEvent::Focus => c.suggest_field = Some(field),
                }
                cx.notify();
            }));
        }
        subs.push(cx.subscribe_in(&ai_input, window, |this: &mut MailApp, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.ai_draft(window, cx);
            }
        }));

        let n = app.now;
        Self {
            mode,
            from,
            from_open: false,
            show_cc_bcc: !cc.is_empty(),
            to,
            cc,
            bcc: Vec::new(),
            to_input,
            cc_input,
            bcc_input,
            subject: subject_input,
            body,
            formatted: true,
            attachments: Vec::new(),
            ai_input,
            ai_busy: false,
            thread,
            thread_account,
            reply_to,
            in_reply_to,
            references,
            original,
            original_preview,
            show_original: false,
            forward_parts,
            fetching: 0,
            suggest_field: None,
            local_id: format!("draft-{n}-{}", std::process::id()),
            error: None,
            _subs: subs,
        }
    }

    pub fn input(&self, field: Field) -> &Entity<InputState> {
        match field {
            Field::To => &self.to_input,
            Field::Cc => &self.cc_input,
            Field::Bcc => &self.bcc_input,
        }
    }

    fn list_mut(&mut self, field: Field) -> &mut Vec<Address> {
        match field {
            Field::To => &mut self.to,
            Field::Cc => &mut self.cc,
            Field::Bcc => &mut self.bcc,
        }
    }

    /// Turn what was typed in a recipient field into chips; anything that did not parse stays typed.
    pub fn commit(&mut self, field: Field, window: &mut Window, cx: &mut App) {
        let text = self.input(field).read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        let (addrs, rest) = parse_addresses(&text);
        let list = self.list_mut(field);
        for a in addrs {
            if !list.contains(&a) {
                list.push(a);
            }
        }
        self.input(field).clone().update(cx, |s, cx| s.set_value(rest, window, cx));
    }

    pub fn commit_recipients(&mut self, window: &mut Window, cx: &mut App) {
        for f in [Field::To, Field::Cc, Field::Bcc] {
            self.commit(f, window, cx);
        }
    }

    /// Add one picked address (a suggestion) and clear what was typed.
    pub fn add_recipient(&mut self, field: Field, a: Address, window: &mut Window, cx: &mut App) {
        let list = self.list_mut(field);
        if !list.contains(&a) {
            list.push(a);
        }
        self.input(field).clone().update(cx, |s, cx| s.set_value("", window, cx));
    }

    pub fn check(&self, cx: &App) -> Result<(), String> {
        if self.to.is_empty() && self.cc.is_empty() && self.bcc.is_empty() {
            return Err("Add at least one recipient.".into());
        }
        if self.fetching > 0 {
            return Err("Still fetching the forwarded attachments…".into());
        }
        for f in [Field::To, Field::Cc, Field::Bcc] {
            let left = self.input(f).read(cx).value();
            if !left.trim().is_empty() {
                return Err(format!("\"{}\" is not an address.", left.trim()));
            }
        }
        Ok(())
    }

    pub fn is_empty(&self, cx: &App) -> bool {
        let body = self.body.read(cx).value();
        self.to.is_empty() && body.trim().is_empty()
            && (self.subject.read(cx).value().trim().is_empty() || self.original.is_some())
    }

    pub fn from_account<'a>(&self, app: &'a MailApp) -> &'a Account {
        &app.accounts[self.from.min(app.accounts.len().saturating_sub(1))]
    }

    pub fn to_draft(&self, cx: &App) -> Draft {
        let typed = self.body.read(cx).value().to_string();
        let (text, html) = outgoing(&typed, self.original.as_ref(), self.formatted);
        Draft {
            local_id: self.local_id.clone(),
            to: self.to.clone(),
            cc: self.cc.clone(),
            bcc: self.bcc.clone(),
            subject: self.subject.read(cx).value().to_string(),
            html,
            text,
            reply_to: self.reply_to.clone(),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
            attachments: self.attachments.clone(),
            server_id: None,
        }
    }

    pub fn fill_from_ai(&mut self, draft: Draft, window: &mut Window, cx: &mut App) {
        // The quote is not in the editor; `compose` adds it when the draft is made.
        let text = draft.text;
        self.body.update(cx, |s, cx| {
            s.set_value(text, window, cx);
            s.focus(window, cx);
        });
        if self.subject.read(cx).value().trim().is_empty() && !draft.subject.is_empty() {
            let subject = draft.subject.clone();
            self.subject.update(cx, |s, cx| s.set_value(subject, window, cx));
        }
        if self.to.is_empty() {
            self.to = draft.to;
        }
        self.ai_input.update(cx, |s, cx| s.set_value("", window, cx));
    }

    pub fn fill_from_mailto(&mut self, m: crate::mailto::Mailto, window: &mut Window, cx: &mut App) {
        self.to = m.to;
        self.cc = m.cc;
        self.bcc = m.bcc;
        self.show_cc_bcc = !self.cc.is_empty() || !self.bcc.is_empty();
        let (subject, body) = (m.subject, m.body);
        self.subject.update(cx, |s, cx| s.set_value(subject, window, cx));
        self.body.update(cx, |s, cx| s.set_value(body, window, cx));
        self.focus_first(window, cx);
    }

    /// A fetched part of the forwarded message, now a local file to attach.
    pub fn add_forwarded(&mut self, path: &Path, a: &Attachment) {
        let path_s = path.to_string_lossy().to_string();
        if !self.attachments.iter().any(|x| x.path == path_s) {
            self.attachments.push(DraftAttachment { path: path_s, filename: a.filename.clone(), mime: a.mime.clone() });
        }
    }

    pub fn add_attachment(&mut self, path: &Path) {
        let filename = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
        if self.attachments.iter().any(|a| a.path == path.to_string_lossy()) {
            return;
        }
        self.attachments.push(DraftAttachment { path: path.to_string_lossy().into(), filename, mime: mime_for(path).into() });
    }

    pub fn focus_first(&self, window: &mut Window, cx: &mut App) {
        if self.to.is_empty() {
            self.to_input.update(cx, |s, cx| s.focus(window, cx));
        } else {
            self.focus_body(window, cx);
        }
    }

    pub fn focus_body(&self, window: &mut Window, cx: &mut App) {
        self.body.update(cx, |s, cx| {
            s.focus(window, cx);
            s.set_selected_range(0..0, cx);
        });
    }

    /// Wrap the selection in a marker (`**`, `_`) or prefix the line (`- `, `> `).
    pub fn format(&mut self, mark: &str, window: &mut Window, cx: &mut App) {
        self.formatted = true;
        let mark = mark.to_string();
        self.body.update(cx, |s, cx| {
            let selected = s.selected_text().to_string();
            let text = if mark.ends_with(' ') {
                let lines: Vec<String> = if selected.is_empty() { vec![String::new()] } else {
                    selected.lines().map(str::to_owned).collect()
                };
                lines.iter().map(|l| format!("{mark}{l}")).collect::<Vec<_>>().join("\n")
            } else {
                format!("{mark}{selected}{mark}")
            };
            s.replace(text, window, cx);
            s.focus(window, cx);
        });
    }
}

impl MailApp {
    /// People to suggest for what is typed in `field`, from the account being written from (the anon
    /// wall is in `Contacts::suggest`).
    pub fn suggestions(&self, field: Field, cx: &App) -> Vec<Address> {
        let Some(c) = &self.composer else { return Vec::new() };
        let Some(acct) = self.accounts.get(c.from) else { return Vec::new() };
        let typed = c.input(field).read(cx).value().to_string();
        let skip: Vec<Address> = c.to.iter().chain(&c.cc).chain(&c.bcc).cloned().collect();
        self.contacts.suggest(&self.accounts, &acct.id, &typed, &skip, SUGGESTIONS)
    }

    fn suggestion_list(&self, field: Field, cx: &mut Context<Self>) -> Option<Div> {
        let p = self.pal;
        let c = self.composer.as_ref()?;
        if c.suggest_field != Some(field) {
            return None;
        }
        let people = self.suggestions(field, cx);
        if people.is_empty() {
            return None;
        }
        Some(
            v_col()
                .mx_4()
                .my_1()
                .ml(px(80.))
                .p_1()
                .rounded_lg()
                .bg(p.raised)
                .border_1()
                .border_color(p.strong)
                .shadow_md()
                .children(people.into_iter().enumerate().map(|(i, a)| {
                    let name = a.name.clone().unwrap_or_else(|| a.email.clone());
                    let email = a.email.clone();
                    h_row()
                        .id(SharedString::from(format!("suggest-{field:?}-{i}")))
                        .gap_2()
                        .px_2()
                        .h(px(32.))
                        .rounded_md()
                        .when(i == 0, |d| d.bg(p.selected))
                        .hover(|s| s.bg(p.hover))
                        .child(avatar(&name, person_color(&a.email), 20.))
                        .child(div().text_sm().text_color(p.text).child(name))
                        .child(div().text_xs().text_color(p.dim).child(email))
                        .when(i == 0, |d| d.child(div().flex_1()).child(keys_hint(&p, &["Enter".into()])))
                        .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            if let Some(c) = this.composer.as_mut() {
                                c.add_recipient(field, a.clone(), window, cx);
                                c.input(field).clone().update(cx, |s, cx| s.focus(window, cx));
                            }
                            cx.notify();
                        }))
                })),
        )
    }

    fn recipient_row(&self, field: Field, label: &'static str, cx: &mut Context<Self>) -> AnyElement {
        let p = self.pal;
        let Some(c) = &self.composer else { return div().into_any_element() };
        let chips = match field {
            Field::To => &c.to,
            Field::Cc => &c.cc,
            Field::Bcc => &c.bcc,
        };
        let anon_from = self.accounts.get(c.from).is_some_and(|a| a.persona == Persona::Anon);
        h_row()
            .gap_2()
            .px_4()
            .min_h(px(40.))
            .border_b_1()
            .border_color(p.line)
            .child(div().w(px(56.)).flex_none().text_sm().text_color(p.dim).child(label))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .flex_1()
                    .items_center()
                    .gap_1()
                    .py_1()
                    .children(chips.iter().enumerate().map(|(i, a)| {
                        let title = a.name.clone().unwrap_or_else(|| a.email.clone());
                        div()
                            .id(SharedString::from(format!("chip-{field:?}-{i}")))
                            .flex()
                            .items_center()
                            .gap_1()
                            .pl_2()
                            .pr_1()
                            .h(px(24.))
                            .rounded_full()
                            .bg(if anon_from { p.anon.opacity(0.18) } else { p.raised })
                            .border_1()
                            .border_color(p.line)
                            .text_sm()
                            .text_color(p.text)
                            .child(title)
                            .when(a.name.is_some(), |d| d.child(div().text_xs().text_color(p.dim).child(a.email.clone())))
                            .child(
                                div()
                                    .id(SharedString::from(format!("chip-x-{field:?}-{i}")))
                                    .size(px(16.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .hover(|s| s.bg(p.hover))
                                    .child(icon("x", 10., p.dim))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(c) = this.composer.as_mut() {
                                            let l = c.list_mut(field);
                                            if i < l.len() {
                                                l.remove(i);
                                            }
                                        }
                                        cx.notify();
                                    })),
                            )
                    }))
                    .child(div().flex_1().min_w(px(160.)).child(Input::new(c.input(field)).appearance(false).small())),
            )
            .when(field == Field::To && !c.show_cc_bcc, |row| {
                row.child(
                    div()
                        .id("show-cc")
                        .text_sm()
                        .text_color(p.dim)
                        .hover(|s| s.text_color(p.text))
                        .child("Cc Bcc")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(c) = this.composer.as_mut() {
                                c.show_cc_bcc = true;
                            }
                            cx.notify();
                        })),
                )
            })
            .into_any_element()
    }

    /// The original under the editor: who wrote it and when, then its text behind a quote bar.
    fn original_preview(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.pal;
        let c = self.composer.as_ref()?;
        let o = c.original.as_ref()?;
        let when = crate::time::long_date(o.date);
        let head = match o.kind {
            QuoteKind::Reply => format!("On {when}, {} wrote:", o.from.display()),
            QuoteKind::Forward => format!("Forwarded message from {}, {when}", o.from.display()),
        };
        let open = c.show_original;
        let id = MessageId::new("composer-original");
        let painter = super::reader::Painter::new(self, &id, cx);
        let body = painter.blocks(&c.original_preview, true);
        Some(
            v_col()
                .mx_4()
                .mb_2()
                .px_3()
                .py_2()
                .gap_2()
                .rounded_lg()
                .bg(p.paper.card)
                .border_1()
                .border_color(p.line)
                .child(
                    h_row()
                        .id("composer-original-toggle")
                        .gap_2()
                        .child(icon(if open { "chevron-down" } else { "chevron-right" }, 12., p.paper.dim))
                        .child(div().flex_1().truncate().text_xs().text_color(p.paper.muted).child(head))
                        .child(div().text_xs().text_color(p.paper.dim).child(if open { "Hide" } else { "Show quoted text" }))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(c) = this.composer.as_mut() {
                                c.show_original = !c.show_original;
                            }
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .pl_3()
                        .border_l_2()
                        .border_color(p.paper.quote)
                        .text_sm()
                        .when(!open, |d| d.max_h(px(64.)).overflow_hidden())
                        .when(open, |d| d.max_h(px(220.)).overflow_hidden())
                        .child(body),
                )
                .into_any_element(),
        )
    }

    pub fn render_composer(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = self.pal;
        let Some(c) = &self.composer else { return div().into_any_element() };
        let title = match c.mode {
            ComposeMode::New => "New message",
            ComposeMode::Reply => "Reply",
            ComposeMode::ReplyAll => "Reply all",
            ComposeMode::Forward => "Forward",
        };
        let from = self.accounts.get(c.from).cloned();
        let mismatch = c.thread_account.is_some_and(|t| t != c.from);
        let undo_secs = self.opts.undo_send_secs;

        let from_row = {
            let a = from.clone();
            h_row()
                .gap_2()
                .px_4()
                .h(px(40.))
                .border_b_1()
                .border_color(p.line)
                .child(div().w(px(56.)).flex_none().text_sm().text_color(p.dim).child("From"))
                .child(
                    div()
                        .id("from-picker")
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .h(px(28.))
                        .rounded_md()
                        .hover(|s| s.bg(p.hover))
                        .when_some(a, |d, a| {
                            d.child(dot(p.account(a.color.as_deref()), 8.))
                                .child(div().text_sm().text_color(p.text).child(format!("{} <{}>", a.name, a.email)))
                                .child(persona_tag(&p, &a))
                        })
                        .child(icon("chevron-down", 12., p.dim))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(c) = this.composer.as_mut() {
                                c.from_open = !c.from_open;
                            }
                            cx.notify();
                        })),
                )
                .when(mismatch, |row| {
                    row.child(div().text_xs().text_color(p.working).child("Not the account this conversation came to"))
                })
        };

        let from_menu = c.from_open.then(|| {
            v_col()
                .absolute()
                .top(px(84.))
                .left(px(72.))
                .w(px(420.))
                .p_1()
                .rounded_lg()
                .bg(if p.dark { rgb(0x1a2029).into() } else { p.raised })
                .border_1()
                .border_color(p.strong)
                .shadow_lg()
                .children(self.accounts.iter().enumerate().map(|(i, a)| {
                    let selected = i == c.from;
                    h_row()
                        .id(SharedString::from(format!("from-{i}")))
                        .gap_2()
                        .px_2()
                        .h(px(34.))
                        .rounded_md()
                        .when(selected, |d| d.bg(p.selected))
                        .hover(|s| s.bg(p.hover))
                        .child(dot(p.account(a.color.as_deref()), 8.))
                        .child(div().flex_1().text_sm().text_color(p.text).child(format!("{} <{}>", a.name, a.email)))
                        .child(persona_tag(&p, a))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(c) = this.composer.as_mut() {
                                c.from = i;
                                c.from_open = false;
                            }
                            cx.notify();
                        }))
                }))
        });

        let tool = |id: &'static str, glyph: &'static str, _tip: &'static str, mark: &'static str, cx: &mut Context<Self>| {
            div()
                .id(id)
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .hover(|s| s.bg(p.hover))
                .child(icon(glyph, 14., p.muted))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if let Some(c) = this.composer.as_mut() {
                        c.format(mark, window, cx);
                    }
                    cx.notify();
                }))
        };

        let toolbar = h_row()
            .gap_1()
            .px_3()
            .h(px(38.))
            .border_b_1()
            .border_color(p.line)
            .child(tool("fmt-bold", "bold", "Bold", "**", cx))
            .child(tool("fmt-italic", "italic", "Italic", "_", cx))
            .child(tool("fmt-list", "list", "Bulleted list", "- ", cx))
            .child(tool("fmt-quote", "text-quote", "Quote", "> ", cx))
            .child(div().w(px(1.)).h(px(18.)).mx_1().bg(p.line))
            .child(segmented(&p, "fmt-mode", &["Plain", "Formatted"], usize::from(c.formatted), cx.listener(|this, ix: &usize, _, cx| {
                if let Some(c) = this.composer.as_mut() {
                    c.formatted = *ix == 1;
                }
                cx.notify();
            })))
            .child(div().flex_1())
            .child(
                Button::new("attach")
                    .ghost()
                    .small()
                    .icon(gpui_kit::component::Icon::default().path("icons/paperclip.svg"))
                    .label("Attach")
                    .on_click(cx.listener(|this, _, _, cx| this.attach_files(cx))),
            );

        let fetching = c.fetching;
        let attachments = (!c.attachments.is_empty() || fetching > 0).then(|| {
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .px_4()
                .py_2()
                .border_t_1()
                .border_color(p.line)
                .children(c.attachments.iter().enumerate().map(|(i, a)| {
                    h_row()
                        .id(SharedString::from(format!("att-{i}")))
                        .gap_2()
                        .pl_2()
                        .pr_1()
                        .h(px(28.))
                        .rounded_md()
                        .bg(p.raised)
                        .border_1()
                        .border_color(p.line)
                        .child(icon("paperclip", 12., p.dim))
                        .child(div().text_sm().text_color(p.text).child(a.filename.clone()))
                        .child(
                            div()
                                .id(SharedString::from(format!("att-x-{i}")))
                                .size(px(18.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_md()
                                .hover(|s| s.bg(p.hover))
                                .child(icon("x", 10., p.dim))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(c) = this.composer.as_mut().filter(|c| i < c.attachments.len()) {
                                        c.attachments.remove(i);
                                    }
                                    cx.notify();
                                })),
                        )
                }))
                .when(fetching > 0, |d| d.child(h_row().gap_1().h(px(28.)).child(icon("loader", 12., p.dim))
                    .child(div().text_xs().text_color(p.dim).child(format!("Fetching {fetching} file{} from the original…",
                        if fetching == 1 { "" } else { "s" })))))
        });

        let ai_box = h_row()
            .gap_2()
            .mx_4()
            .my_3()
            .px_3()
            .py_2()
            .rounded_lg()
            .bg(p.accent.opacity(if p.dark { 0.08 } else { 0.06 }))
            .border_1()
            .border_color(p.accent.opacity(0.35))
            .child(icon("sparkles", 14., p.accent))
            .child(div().flex_1().child(Input::new(&c.ai_input).appearance(false).small()))
            .child(
                Button::new("ai-draft")
                    .small()
                    .primary()
                    .loading(c.ai_busy)
                    .label(if c.ai_busy { "Drafting…" } else { "Draft" })
                    .on_click(cx.listener(|this, _, window, cx| this.ai_draft(window, cx))),
            );

        let footer = h_row()
            .gap_3()
            .px_4()
            .h(px(56.))
            .border_t_1()
            .border_color(p.line)
            .child(
                Button::new("send")
                    .primary()
                    .icon(gpui_kit::component::Icon::default().path("icons/send.svg"))
                    .label("Send")
                    .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
            )
            .child(keys_hint(&p, &keymap::display_keys("secondary-enter")))
            .child(div().text_xs().text_color(p.dim).child(format!("Undo for {undo_secs} s after sending")))
            .child(div().flex_1())
            .when_some(c.error.clone(), |row, e| row.child(div().text_sm().text_color(p.blocked).child(e)))
            .child(
                Button::new("discard")
                    .ghost()
                    .small()
                    .icon(gpui_kit::component::Icon::default().path("icons/trash.svg"))
                    .tooltip("Discard")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.composer = None;
                        this.focus.focus(window, cx);
                        cx.notify();
                    })),
            );

        let sheet = v_col()
            .id("composer")
            .key_context(COMPOSER_CONTEXT)
            .relative()
            .w(px(760.))
            .max_w(relative(0.92))
            .h(relative(0.86))
            .max_h(px(780.))
            .rounded_xl()
            .overflow_hidden()
            .bg(p.surface)
            .border_1()
            .border_color(p.strong)
            .shadow_2xl()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                h_row()
                    .px_4()
                    .h(px(44.))
                    .bg(p.chrome)
                    .border_b_1()
                    .border_color(p.line)
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.text).child(title))
                    .child(div().flex_1())
                    .child(keys_hint(&p, &["Esc".into()]))
                    .child(div().text_xs().text_color(p.dim).ml_1().child("saves a draft")),
            )
            .child(from_row)
            .child(self.recipient_row(Field::To, "To", cx))
            .children(self.suggestion_list(Field::To, cx))
            .when(c.show_cc_bcc, |d| {
                d.child(self.recipient_row(Field::Cc, "Cc", cx))
                    .children(self.suggestion_list(Field::Cc, cx))
                    .child(self.recipient_row(Field::Bcc, "Bcc", cx))
                    .children(self.suggestion_list(Field::Bcc, cx))
            })
            .child(
                h_row()
                    .gap_2()
                    .px_4()
                    .h(px(40.))
                    .border_b_1()
                    .border_color(p.line)
                    .child(div().w(px(56.)).flex_none().text_sm().text_color(p.dim).child("Subject"))
                    .child(div().flex_1().child(Input::new(&c.subject).appearance(false).small())),
            )
            .child(toolbar)
            .child(div().flex_1().min_h(px(120.)).px_2().py_1().child(Textarea::new(&c.body).appearance(false).h_full()))
            .children(self.original_preview(cx))
            .children(attachments)
            .child(ai_box)
            .child(footer)
            .children(from_menu);

        div()
            .id("composer-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(p.backdrop)
            .child(sheet)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{mime_for, outgoing, parse_addresses};
    use lumbridge_mail_core::{Address, Body};
    use lumbridge_mail_render::{Original, QuoteKind};
    use std::path::Path;

    #[test]
    fn addresses_parse_with_and_without_names() {
        let (ok, bad) = parse_addresses("Maya Chen <maya.chen@postbox.example>, jonah@parkfamily.example; nope");
        assert_eq!(ok.len(), 2);
        assert_eq!(ok[0].name.as_deref(), Some("Maya Chen"));
        assert_eq!(ok[1].email, "jonah@parkfamily.example");
        assert_eq!(bad, "nope");
        let (ok, bad) = parse_addresses("\"Rivera, Sam\" <SAM@Inkwell.example>");
        assert_eq!(ok, vec![Address::new(Some("Rivera, Sam"), "sam@inkwell.example")]);
        assert!(bad.is_empty());
        assert!(parse_addresses("a@b").0.is_empty());
        assert!(parse_addresses("a b@c.example").0.is_empty());
    }

    fn original(kind: QuoteKind) -> Original {
        Original {
            kind,
            from: Address::new(Some("Maya Chen"), "maya.chen@postbox.example"),
            to: vec![Address::new(Some("Sam Rivera"), "sam.rivera@inkwell.example")],
            cc: vec![],
            date: 1_794_328_920,
            utc_offset_s: 0,
            subject: "Dinner Saturday?".into(),
            body: Body { html: Some("<p>Booked <b>Olive &amp; Thyme</b> for 7:30.</p>".into()), ..Default::default() },
        }
    }

    #[test]
    fn replies_are_markdown_plus_the_quoted_original() {
        let o = original(QuoteKind::Reply);
        let (text, html) = outgoing("Saturday is **perfect**.\n\n- I'll bring dessert", Some(&o), true);
        let html = html.unwrap();
        assert!(html.contains("<strong>perfect</strong>"), "{html}");
        assert!(html.contains("<li>"), "{html}");
        assert!(html.contains("<blockquote"), "the original is quoted in HTML: {html}");
        assert!(html.contains("Olive &amp; Thyme"), "{html}");
        assert!(text.starts_with("Saturday is"), "{text}");
        assert!(text.contains("Maya Chen <maya.chen@postbox.example> wrote:"), "{text}");
        assert!(text.contains("> Booked Olive & Thyme for 7:30."), "{text}");
        // Raw HTML typed (or drafted by a model) stays text.
        let (_, html) = outgoing("<script>x</script>", None, true);
        assert!(!html.unwrap().contains("<script>"));
    }

    #[test]
    fn plain_mode_sends_only_text_as_typed() {
        let o = original(QuoteKind::Reply);
        let (text, html) = outgoing("Yes, **7:30** works", Some(&o), false);
        assert!(html.is_none());
        assert!(text.starts_with("Yes, **7:30** works\n\n"), "{text}");
        assert!(text.contains("wrote:"));
        let (text, _) = outgoing("Just this", None, false);
        assert_eq!(text, "Just this\n");
    }

    #[test]
    fn forwards_carry_the_header_block() {
        let o = original(QuoteKind::Forward);
        let (text, html) = outgoing("FYI", Some(&o), true);
        assert!(text.contains("---------- Forwarded message ---------"), "{text}");
        assert!(text.contains("Subject: Dinner Saturday?"));
        assert!(html.unwrap().contains("Forwarded message"));
        assert_eq!(lumbridge_mail_render::compose::subject_for(QuoteKind::Forward, "Re: x"), "Fwd: Re: x");
    }

    #[test]
    fn mime_types() {
        assert_eq!(mime_for(Path::new("x.PDF")), "application/pdf");
        assert_eq!(mime_for(Path::new("x")), "application/octet-stream");
    }
}
