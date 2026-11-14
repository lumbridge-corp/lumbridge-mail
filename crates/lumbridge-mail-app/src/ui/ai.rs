//! The AI side panel: the open thread's summary and action items, and "ask your mail".

use super::widgets::*;
use crate::app::{Load, MailApp, View};
use gpui::prelude::*;
use gpui::*;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::Sizable;
use lumbridge_mail_core::{AiPolicy, Owner, Persona};

impl MailApp {
    fn panel_section(&self, title: &str, glyph: &str) -> Div {
        let p = self.pal;
        h_row()
            .gap_1p5()
            .px_4()
            .pt_4()
            .pb_2()
            .child(icon(glyph, 12., p.dim))
            .child(div().text_size(px(10.5)).font_weight(FontWeight::MEDIUM).text_color(p.dim).child(title.to_uppercase()))
    }

    pub fn render_ai_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.pal;
        let open = self.open.as_ref();
        let acct = open.and_then(|o| self.account(&o.summary.account)).cloned();
        let place = match &acct {
            Some(a) if a.persona == Persona::Anon => "Local model only",
            Some(a) if a.ai == AiPolicy::Off => "AI off",
            Some(a) if a.ai == AiPolicy::LocalOnly => "Local model only",
            _ => "Local model first",
        };
        let scope = match &self.view {
            View::Mailbox { account, .. } => self.account(account).map(|a| a.label.clone()).unwrap_or_default(),
            _ => "all accounts except anon".into(),
        };

        let summary: AnyElement = match (open, &self.ai.summary) {
            (None, _) => div().px_4().text_sm().text_color(p.dim).child("Open a conversation to see its summary.").into_any_element(),
            (Some(_), Load::Loading) => v_col().px_4().gap_2()
                .child(Skeleton::new().h(px(10.)).w(relative(0.9)))
                .child(Skeleton::new().h(px(10.)).w(relative(0.75)))
                .child(Skeleton::new().h(px(10.)).w(relative(0.82)))
                .into_any_element(),
            (Some(_), Load::Failed(why)) => h_row().items_start().gap_2().mx_4().p_3().rounded_md().bg(p.working.opacity(0.1))
                .child(icon("lock", 13., p.working))
                .child(div().flex_1().min_w_0().text_sm().text_color(p.muted).child(why.clone()))
                .into_any_element(),
            (Some(o), Load::Ready(text)) => v_col().px_4().gap_2()
                .when(o.summary.triage.as_ref().is_some_and(|t| t.suspicious), |d| d.child(
                    h_row().gap_2().p_2().rounded_md().bg(p.blocked.opacity(0.1))
                        .child(icon("shield-alert", 12., p.blocked))
                        .child(div().flex_1().text_xs().text_color(p.text)
                            .child("This mail tries to instruct an assistant. The summary describes it; nothing in it is followed."))))
                .child(div().text_sm().text_color(p.text).line_height(rems(1.5)).child(text.clone()))
                .into_any_element(),
        };

        let items: AnyElement = match (open, &self.ai.items) {
            (None, _) => div().into_any_element(),
            (Some(_), Load::Loading) => div().px_4().child(Skeleton::new().h(px(10.)).w(relative(0.6))).into_any_element(),
            (Some(_), Load::Failed(e)) => div().px_4().text_sm().text_color(p.dim).child(e.clone()).into_any_element(),
            (Some(_), Load::Ready(x)) if x.items.is_empty() && x.dates.is_empty() => {
                div().px_4().text_sm().text_color(p.dim).child("Nothing asked of you here.").into_any_element()
            }
            // `ai.extract`: what is asked (and of whom), then the dates the thread mentions.
            (Some(_), Load::Ready(x)) => v_col().px_4().gap_1()
                .children(x.items.iter().map(|it| {
                    let who = match it.owner {
                        Owner::Me => None,
                        Owner::Them => Some("They will"),
                        Owner::Unknown => Some("Someone"),
                    };
                    h_row()
                        .items_start()
                        .gap_2()
                        .py_1()
                        .child(div().mt(px(3.)).size(px(12.)).rounded_sm().border_1().border_color(p.strong))
                        .child(div().flex_1().min_w_0().text_sm().text_color(p.text).child(it.text.clone()))
                        .when_some(who, |d, w| d.child(chip(w, p.dim, p.raised)))
                        .when_some(it.due, |d, due| {
                            d.child(chip(crate::time::due(due, self.now), p.working, p.working.opacity(0.14)))
                        })
                }))
                .children(x.dates.iter().map(|m| {
                    h_row()
                        .gap_2()
                        .py_1()
                        .child(icon("calendar", 12., p.dim))
                        .child(div().flex_1().min_w_0().text_sm().text_color(p.muted).child(m.what.clone()))
                        .child(chip(crate::time::due(m.at, self.now), p.dim, p.raised))
                }))
                .into_any_element(),
        };

        let chat = v_col().px_4().gap_2().children(self.ai.chat.iter().enumerate().map(|(i, line)| {
            if line.mine {
                h_row().justify_end().child(
                    div().max_w(relative(0.85)).px_3().py_1p5().rounded_lg().bg(p.accent.opacity(0.18))
                        .text_sm().text_color(p.text).child(line.text.clone()),
                )
            } else {
                h_row().child(
                    v_col()
                        .max_w(relative(0.95))
                        .gap_1p5()
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(p.raised)
                        .border_1()
                        .border_color(if line.failed { p.blocked.opacity(0.5) } else { p.line })
                        .child(div().text_sm().text_color(p.text).line_height(rems(1.45)).child(line.text.clone()))
                        .when(!line.sources.is_empty(), |d| {
                            d.child(div().flex().flex_wrap().gap_1().children(line.sources.iter().enumerate().map(|(j, (r, title))| {
                                let r = r.clone();
                                h_row()
                                    .id(SharedString::from(format!("src-{i}-{j}")))
                                    .gap_1()
                                    .px_1p5()
                                    .h(px(20.))
                                    .max_w(px(220.))
                                    .rounded_sm()
                                    .bg(p.surface)
                                    .border_1()
                                    .border_color(p.line)
                                    .hover(|s| s.border_color(p.accent))
                                    .child(icon("mail", 10., p.dim))
                                    .child(div().truncate().text_xs().text_color(p.muted).child(title.clone()))
                                    .on_click(cx.listener(move |this, _, w, cx| this.open_ref(r.clone(), w, cx)))
                            })))
                        }),
                )
            }
        }));

        v_col()
            .id("ai-panel")
            .w(px(320.))
            .flex_none()
            .h_full()
            .bg(p.chrome)
            .border_l_1()
            .border_color(p.line)
            .child(
                h_row()
                    .gap_2()
                    .px_4()
                    .h(px(52.))
                    .border_b_1()
                    .border_color(p.line)
                    .child(icon("sparkles", 14., p.accent))
                    .child(div().flex_1().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.text).child("Assistant"))
                    .child(chip(place, p.done, p.done.opacity(0.14)))
                    .child(
                        div()
                            .id("ai-close")
                            .size(px(24.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .hover(|s| s.bg(p.hover))
                            .child(icon("x", 12., p.dim))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_ai_panel(cx))),
                    ),
            )
            .child(
                v_col()
                    .id("ai-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .pb_3()
                    .child(v_col().flex_none()
                    .child(self.panel_section("Summary", "scroll-text"))
                    .child(summary)
                    .child(self.panel_section("Action items", "list-checks"))
                    .child(items)
                    .child(self.panel_section("Ask your mail", "message-circle-question-mark"))
                    .child(div().px_4().pb_2().text_xs().text_color(p.dim).child(format!("Searches {scope}. Answers cite their threads.")))
                    .child(chat)
                    .when(self.ai.asking, |d| d.child(h_row().px_4().pt_2().gap_2().child(icon("loader-circle", 12., p.dim))
                        .child(div().text_xs().text_color(p.dim).child("Reading your mail…"))))),
            )
            .child(
                h_row()
                    .gap_2()
                    .p_3()
                    .border_t_1()
                    .border_color(p.line)
                    .child(div().flex_1().child(Input::new(&self.ask_input).small()))
                    .child(Button::new("ask").small().primary().icon(gpui_kit::component::Icon::default().path("icons/arrow-up.svg"))
                        .loading(self.ai.asking)
                        .on_click(cx.listener(|this, _, w, cx| this.ask(w, cx)))),
            )
            .into_any_element()
    }
}
