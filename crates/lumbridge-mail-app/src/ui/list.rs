//! The thread list: a virtualised `uniform_list` over the loaded pages (more load as you scroll), with the
//! search field and its operator hints on top.

use super::widgets::*;
use crate::app::{view_title, Load, MailApp, Offer, Verb, View};
use crate::theme;
use gpui::prelude::*;
use gpui::*;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::Sizable;
use lumbridge_mail_core::{AccountId, Category, MailboxRole, ThreadSummary};
use std::collections::BTreeSet;

pub const ROW_HEIGHT: f32 = 76.;
/// Two lines (sender, subject): the "compact" density setting.
pub const COMPACT_ROW_HEIGHT: f32 = 56.;

pub const SEARCH_OPERATORS: &[(&str, &str)] = &[
    ("from:", "sender"),
    ("to:", "recipient"),
    ("subject:", "in the subject"),
    ("has:attachment", "with files"),
    ("is:unread", "unread"),
    ("is:starred", "starred"),
    ("in:archive", "a mailbox"),
    ("after:2026-11-01", "dates"),
];

impl MailApp {
    fn senders(&self, t: &ThreadSummary) -> String {
        let me = self.account(&t.account).map(|a| a.email.clone()).unwrap_or_default();
        let names: Vec<String> = t
            .participants
            .iter()
            .map(|a| {
                if a.email == me {
                    "me".to_string()
                } else if t.participants.len() > 1 {
                    a.name.as_deref().and_then(|n| n.split_whitespace().next()).unwrap_or(&a.email).to_string()
                } else {
                    a.display().to_string()
                }
            })
            .collect();
        if names.len() > 3 {
            format!("{}, {} … {}", names[0], names[1], names[names.len() - 1])
        } else {
            names.join(", ")
        }
    }

    fn render_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let p = self.pal;
        let Some(t) = self.threads.get(ix) else { return div().into_any_element() };
        let unread = t.unread_count > 0;
        let is_cursor = self.cursor == Some(ix);
        let is_open = self.open.as_ref().is_some_and(|o| o.summary.id == t.id && o.summary.account == t.account);
        let is_selected = self.selected.contains(&crate::app::key(t));
        let selecting = !self.selected.is_empty();
        let unified = !matches!(self.view, View::Mailbox { .. }) || self.search_text.is_some();
        let acct = self.account(&t.account).cloned();
        let acct_color = p.account(acct.as_ref().and_then(|a| a.color.as_deref()));
        let triage = t.triage.clone();
        let senders = self.senders(t);
        let time = crate::time::short_date(t.date, self.now);
        let subject = if t.subject.trim().is_empty() { "(no subject)".to_string() } else { t.subject.clone() };
        let summary = triage.as_ref().map(|t| t.summary.clone()).filter(|s| !s.is_empty())
            .unwrap_or_else(|| t.snippet.clone());
        let ai_line = triage.is_some();
        let bg = if is_selected {
            p.accent.opacity(if p.dark { 0.16 } else { 0.12 })
        } else if is_open || is_cursor {
            p.selected
        } else {
            p.surface
        };

        let line1 = h_row()
            .gap_1p5()
            .h(px(18.))
            .when(unified, |d| d.child(dot(acct_color, 7.)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(if unread { p.text } else { p.muted })
                    .when(unread, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .child(senders),
            )
            .when(t.message_count > 1, |d| {
                d.child(div().text_xs().text_color(p.dim).child(t.message_count.to_string()))
            })
            .when(t.has_attachments, |d| d.child(icon("paperclip", 12., p.dim)))
            .when(t.flagged, |d| d.child(icon("star-fill", 12., p.working)))
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(if unread { p.accent } else { p.dim })
                    .when(unread, |d| d.font_weight(FontWeight::MEDIUM))
                    .child(time),
            );

        let chip_el = triage.as_ref().map(|tr| {
            let (fg, bg) = p.category(tr.category);
            let label = match (tr.category, tr.needs_reply) {
                (Category::Sensitive, _) => "Sensitive".to_string(),
                (_, true) => "Reply".to_string(),
                (c, false) => theme::category_label(c).to_string(),
            };
            let (fg, bg) = if tr.needs_reply && tr.category != Category::Sensitive {
                let (f, _) = p.category(Category::Important);
                (f, f.opacity(if p.dark { 0.16 } else { 0.12 }))
            } else {
                (fg, bg)
            };
            chip(label, fg, bg)
        });

        let line2 = h_row()
            .gap_2()
            .h(px(19.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(if unread { p.text } else { p.muted })
                    .when(unread, |d| d.font_weight(FontWeight::MEDIUM))
                    .child(subject),
            )
            .when(triage.as_ref().is_some_and(|t| t.suspicious), |d| {
                // Triage.suspicious: it tries to instruct an assistant.
                d.child(div().id(("suspicious", ix)).child(icon("shield-alert", 12., p.blocked)))
            })
            .children(chip_el);

        let line3 = h_row()
            .gap_1p5()
            .h(px(17.))
            .when(ai_line, |d| d.child(icon(if triage.as_ref().is_some_and(|t| t.category == Category::Sensitive) {
                "lock" } else { "sparkle" }, 11., p.dim)))
            .child(div().flex_1().min_w_0().truncate().text_xs().text_color(p.dim).child(summary));

        let compact = self.settings.density == crate::settings::Density::Compact;
        let row_h = self.row_height();
        h_row()
            .id(("row", ix))
            .h(px(row_h))
            .w_full()
            .bg(bg)
            .border_b_1()
            .border_color(p.line)
            .when(!is_open && !is_cursor && !is_selected, |d| d.hover(|s| s.bg(p.hover)))
            .child(
                // Unread mark, or the cursor bar.
                div().w(px(3.)).h_full().flex_none().when(is_cursor || is_open, |d| d.bg(p.accent)),
            )
            .child(
                div().w(px(18.)).flex_none().flex().justify_center().pt(px(if compact { 12. } else { 14. })).h_full().child(
                    if selecting {
                        div()
                            .size(px(13.))
                            .rounded_sm()
                            .border_1()
                            .border_color(if is_selected { p.accent } else { p.strong })
                            .when(is_selected, |d| d.bg(p.accent).flex().items_center().justify_center()
                                .child(icon("check", 10., p.accent_fg)))
                            .into_any_element()
                    } else if unread && !unified {
                        dot(p.accent, 7.).mt(px(1.)).into_any_element()
                    } else {
                        div().into_any_element()
                    },
                ),
            )
            .child(v_col().flex_1().min_w_0().pr_4().py(px(if compact { 8. } else { 10. })).gap(px(2.)).child(line1).child(line2)
                .when(!compact, |d| d.child(line3)))
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| this.click_row(ix, ev.modifiers(), window, cx)))
            .into_any_element()
    }

    fn list_header(&mut self, cx: &mut Context<Self>) -> Div {
        let p = self.pal;
        let title = match &self.search_text {
            Some(q) => format!("Search: {q}"),
            None => view_title(&self.view, &self.mailboxes, &self.accounts),
        };
        let scope = match &self.view {
            View::Mailbox { account, .. } => self
                .account(account)
                .map(|a| if self.is_anon(&a.id) { format!("{} · anon, kept apart", a.email) } else { a.email.clone() })
                .unwrap_or_default(),
            _ => {
                let names: Vec<String> = self.accounts.iter().filter(|a| !self.is_anon(&a.id)).map(|a| a.label.clone()).collect();
                if names.is_empty() { String::new() } else { names.join(" + ") }
            }
        };
        let loaded = self.threads.len();
        let count = match (&self.list, self.next_page.is_some()) {
            (Load::Ready(()), true) => format!("{}+ conversations", super::sidebar::format_count(loaded as u32)),
            (Load::Ready(()), false) => format!("{} conversation{}", super::sidebar::format_count(loaded as u32), if loaded == 1 { "" } else { "s" }),
            _ => String::new(),
        };
        let selection = (!self.selected.is_empty()).then(|| {
            let n = self.selected.len();
            // Only what the selected accounts can do (`accounts.capabilities`).
            let accounts: Vec<AccountId> = self.selected.iter().map(|(a, _)| a.clone()).collect::<BTreeSet<_>>().into_iter().collect();
            let can = |verb: Verb| self.offer_for(&accounts, verb) != Offer::Hidden;
            let (archive, trash, star, read) = (can(Verb::Archive), can(Verb::Trash), can(Verb::Star), can(Verb::Read));
            h_row()
                .gap_1()
                .px_3()
                .h(px(38.))
                .bg(p.accent.opacity(if p.dark { 0.12 } else { 0.08 }))
                .border_t_1()
                .border_color(p.line)
                .child(div().flex_1().text_sm().text_color(p.text).child(format!("{n} selected")))
                .when(archive, |d| d.child(Button::new("sel-archive").ghost().xsmall().label("Archive")
                    .on_click(cx.listener(|this, _, w, cx| this.remove_targets(false, w, cx)))))
                .when(trash, |d| d.child(Button::new("sel-trash").ghost().xsmall().label("Trash")
                    .on_click(cx.listener(|this, _, w, cx| this.remove_targets(true, w, cx)))))
                .when(star, |d| d.child(Button::new("sel-star").ghost().xsmall().label("Star")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_star(cx)))))
                .when(read, |d| d.child(Button::new("sel-read").ghost().xsmall().label("Read")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_read(cx)))))
                .child(Button::new("sel-clear").ghost().xsmall().label("Clear")
                    .on_click(cx.listener(|this, _, _, cx| { this.selected.clear(); cx.notify(); })))
        });
        let hints = self.search_focused.then(|| {
            div()
                .flex()
                .flex_wrap()
                .gap_1()
                .px_3()
                .pb_2()
                .children(SEARCH_OPERATORS.iter().enumerate().map(|(i, (op, what))| {
                    let op_s = op.to_string();
                    h_row()
                        .id(("op", i))
                        .gap_1()
                        .px_1p5()
                        .h(px(20.))
                        .rounded_sm()
                        .bg(p.raised)
                        .border_1()
                        .border_color(p.line)
                        .hover(|s| s.border_color(p.accent))
                        .child(div().text_xs().font_family("JetBrains Mono").text_color(p.text).child(*op))
                        .child(div().text_size(px(10.5)).text_color(p.dim).child(*what))
                        // Mouse down, not click: a click would blur the field first and hide the hints.
                        .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            window.prevent_default();
                            this.insert_search_operator(&op_s, window, cx);
                        }))
                }))
        });
        v_col()
            .flex_none()
            .bg(p.surface)
            .border_b_1()
            .border_color(p.line)
            .child(
                h_row()
                    .gap_2()
                    .px_4()
                    .pt_3()
                    .child(
                        v_col()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().text_base().font_weight(FontWeight::SEMIBOLD).text_color(p.text).child(title))
                            .child(div().truncate().text_xs().text_color(p.dim).child(
                                if count.is_empty() { scope } else { format!("{scope}  ·  {count}") })),
                    )
                    .child(
                        div()
                            .id("refresh")
                            .size(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .hover(|s| s.bg(p.hover))
                            .child(icon("refresh-cw", 14., p.dim))
                            .on_click(cx.listener(|this, _, _, cx| this.sync_now(cx))),
                    ),
            )
            .child(
                div().key_context(crate::keymap::SEARCH_CONTEXT).px_3().py_2().child(
                    Input::new(&self.search)
                        .small()
                        .cleanable(true)
                        .prefix(icon("search", 14., p.dim))
                        .suffix(keys_hint(&p, &["/".into()])),
                ),
            )
            .children(hints)
            .children(selection)
    }

    fn list_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.pal;
        match &self.list {
            Load::Loading => v_col()
                .children((0..8).map(|i| {
                    v_col()
                        .h(px(self.row_height()))
                        .px_6()
                        .py_3()
                        .gap_2()
                        .border_b_1()
                        .border_color(p.line)
                        .child(Skeleton::new().h(px(10.)).w(relative(0.35 + 0.05 * (i % 3) as f32)))
                        .child(Skeleton::new().h(px(10.)).w(relative(0.7 - 0.06 * (i % 4) as f32)))
                        .child(Skeleton::new().secondary().h(px(8.)).w(relative(0.55)))
                }))
                .into_any_element(),
            Load::Failed(e) => v_col()
                .m_4()
                .p_4()
                .gap_2()
                .rounded_lg()
                .border_1()
                .border_color(p.blocked.opacity(0.5))
                .bg(p.blocked.opacity(0.08))
                .child(h_row().gap_2().child(icon("circle-alert", 16., p.blocked))
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.text).child("Couldn't load your mail")))
                .child(div().text_sm().text_color(p.muted).child(e.clone()))
                .child(div().text_xs().text_color(p.dim).child(self.link_hint()))
                .child(h_row().pt_1().child(Button::new("retry").small().outline().label("Try again")
                    .on_click(cx.listener(|this, _, _, cx| this.retry(cx)))))
                .into_any_element(),
            Load::Ready(()) if self.threads.is_empty() => {
                let (glyph, title, hint) = match (&self.search_text, &self.view) {
                    (Some(_), _) => ("search-x", "No matches", "Try fewer words, or an operator like from: or has:attachment."),
                    (None, View::Unified(MailboxRole::Inbox)) => ("inbox", "Inbox zero", "Nothing waiting. New mail lands here."),
                    (None, View::Category(_)) => ("sparkles", "Nothing sorted here yet", "The AI files new mail as it arrives."),
                    _ => ("mail-open", "Nothing here", "This mailbox is empty."),
                };
                v_col()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .pb(px(80.))
                    .child(div().size(px(52.)).rounded_full().bg(p.raised).flex().items_center().justify_center()
                        .child(icon(glyph, 22., p.dim)))
                    .child(div().text_base().font_weight(FontWeight::SEMIBOLD).text_color(p.text).child(title))
                    .child(div().text_sm().text_color(p.dim).max_w(px(280.)).text_center().child(hint))
                    .into_any_element()
            }
            Load::Ready(()) => {
                let count = self.threads.len();
                let more = self.loading_more;
                v_col()
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list(
                            "threads",
                            count,
                            cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                                this.maybe_load_more(range.end, cx);
                                range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
                            }),
                        )
                        .track_scroll(&self.list_scroll)
                        .flex_1(),
                    )
                    .when(more, |d| {
                        d.child(h_row().justify_center().h(px(28.)).text_xs().text_color(p.dim).child("Loading more…"))
                    })
                    .into_any_element()
            }
        }
    }

    /// What to do about the daemon link, for the error panel.
    pub fn link_hint(&self) -> String {
        use crate::backend::{LinkState, Problem};
        let log = crate::paths::daemon_log();
        match &self.link {
            Some(LinkState::Starting { program }) => format!("Starting {program}…"),
            Some(LinkState::Connecting) => "Connecting to lumbridge-maild…".into(),
            Some(LinkState::Down { problem, retry_in_s }) => {
                let what = match problem {
                    Problem::NotInstalled { .. } => "Install lumbridge-maild next to the app or on your PATH, or run the app with --demo to try it on made-up mail.".to_string(),
                    Problem::NoDaemon { .. } => "Start lumbridge-maild, or run the app with --demo to try it on made-up mail.".to_string(),
                    Problem::StartFailed { .. } => format!("Its log is {}.", log.display()),
                    Problem::Unreachable { .. } => "Check the socket's owner and permissions.".to_string(),
                    Problem::Lost { .. } => "It may be restarting.".to_string(),
                    Problem::Stopped { .. } => "Run `lbmail resume` (or start lumbridge-maild yourself) to start it again.".to_string(),
                    Problem::Remote { host, .. } => format!(
                        "Check that `ssh {host}` works without a password and that the daemon runs there (systemctl --user status lumbridge-maild)."
                    ),
                };
                format!("{what} Trying again in {retry_in_s} s.")
            }
            _ => "Is lumbridge-maild running? Start it, or run the app with --demo to try it on made-up mail.".into(),
        }
    }

    pub fn render_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.pal;
        let reader_open = self.open.is_some();
        v_col()
            .id("list-pane")
            .w(px(if reader_open { 392. } else { 420. }))
            .flex_none()
            .h_full()
            .bg(p.surface)
            .border_r_1()
            .border_color(p.line)
            .child(self.list_header(cx))
            .child(self.list_body(cx))
            .into_any_element()
    }
}
