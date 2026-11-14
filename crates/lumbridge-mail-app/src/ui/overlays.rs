//! The command palette (gpui-kit's `Command`) and the keyboard shortcuts sheet.

use super::sidebar::SIDEBAR_CATEGORIES;
use super::widgets::*;
use crate::app::{MailApp, View};
use crate::keymap::{self, GoView, COMMANDS, GROUPS};
use crate::theme;
use gpui::prelude::*;
use gpui::*;
use gpui_kit::component::command::{Command, CommandGroup, CommandItem};
use lumbridge_mail_core::{Category, MailboxRole};

fn row(p: crate::theme::Palette, glyph: &'static str, label: String, detail: Option<String>, keys: Vec<String>)
       -> impl Fn(&mut Window, &mut App) -> Div + 'static {
    move |_, _| {
        h_row()
            .w_full()
            .gap_2()
            .h(px(30.))
            .child(icon(glyph, 14., p.dim))
            .child(div().flex_1().truncate().text_sm().text_color(p.text).child(label.clone()))
            .when_some(detail.clone(), |d, t| d.child(div().text_xs().text_color(p.dim).child(t)))
            .child(keys_hint(&p, &keys))
    }
}

fn command_icon(label: &str) -> &'static str {
    match label {
        l if l.starts_with("Go to") => "arrow-right",
        "Archive" => "archive",
        "Move to Trash" => "trash",
        "Star / unstar" => "star",
        "Mark read / unread" => "mail-open",
        "Reply" => "reply",
        "Reply all" => "reply-all",
        "Forward" => "forward",
        "Compose" => "pen-line",
        "Search" => "search",
        "AI panel" => "sparkles",
        "Light / dark" => "sun-moon",
        "Sync now" => "refresh-cw",
        "Undo" => "undo-2",
        "Keyboard shortcuts" => "keyboard",
        "Work offline (demo)" => "wifi-off",
        "Next conversation" => "arrow-down",
        "Previous conversation" => "arrow-up",
        "Open conversation" => "corner-down-left",
        "Select conversation" | "Select all loaded" => "square-check",
        "Clear selection" => "square",
        "Load remote images" => "image",
        "Expand all messages" => "unfold-vertical",
        "Compact / comfortable list" => "rows-3",
        "Desktop notifications on / off" => "bell",
        "Notifications for anon accounts on / off" => "bell-off",
        "Forget senders allowed to show images" => "image-off",
        _ => "command",
    }
}

impl MailApp {
    pub fn render_palette(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.pal;
        let state = self.palette.clone()?;
        let mut command = Command::new(&state)
            .placeholder("Type a command, a mailbox or an account…")
            .max_h(px(420.))
            .bordered(false);

        let mut go = CommandGroup::new().label("Go to");
        let mut views: Vec<(String, &'static str, String, Option<String>)> = vec![
            ("u:inbox".into(), "inbox", "Unified Inbox".into(), None),
            ("u:starred".into(), "star", "Starred (all accounts)".into(), None),
            ("u:sent".into(), "send", "Sent (all accounts)".into(), None),
            ("u:drafts".into(), "file-pen", "Drafts (all accounts)".into(), None),
        ];
        for c in SIDEBAR_CATEGORIES {
            views.push((format!("c:{c:?}"), "sparkles", theme::category_label(c).to_string(), Some("AI category".into())));
        }
        for m in &self.mailboxes {
            let Some(a) = self.account(&m.account) else { continue };
            if !matches!(m.role, MailboxRole::Inbox | MailboxRole::Sent | MailboxRole::Drafts | MailboxRole::Archive | MailboxRole::Custom) {
                continue;
            }
            views.push((format!("m:{}|{}", m.account, m.id), "folder", format!("{} · {}", a.label, m.name), Some(a.email.clone())));
        }
        for (key, glyph, label, detail) in views {
            go = go.item(
                CommandItem::new()
                    .label(label.clone())
                    .keywords(detail.clone().into_iter().collect::<Vec<_>>())
                    .action(Box::new(GoView { key }))
                    .child(row(p, glyph, label, detail, vec![])),
            );
        }

        for group in GROUPS {
            let mut g = CommandGroup::new().label(*group);
            let mut any = false;
            for c in COMMANDS.iter().filter(|c| c.group == *group && c.in_palette) {
                if c.label.ends_with("(demo)") && !self.is_demo() {
                    continue;
                }
                any = true;
                g = g.item(
                    CommandItem::new()
                        .label(c.label)
                        .action((c.action)())
                        .child(row(p, command_icon(c.label), c.label.into(), None, keymap::display_keys(c.keys))),
                );
            }
            if any {
                command = command.group(g);
            }
        }
        command = command.group(go);
        let weak = cx.entity().downgrade();
        let weak2 = weak.clone();
        let command = command
            .on_confirm(move |_, window, cx| {
                let _ = weak.update(cx, |this, cx| this.close_palette(window, cx));
            })
            .on_cancel(move |window, cx| {
                let _ = weak2.update(cx, |this, cx| this.close_palette(window, cx));
            })
            .footer(move |_, _, _| {
                h_row()
                    .gap_3()
                    .px_3()
                    .h(px(32.))
                    .border_t_1()
                    .border_color(p.line)
                    .text_xs()
                    .text_color(p.dim)
                    .child(h_row().gap_1().child(keys_hint(&p, &["↑".into(), "↓".into()])).child("move"))
                    .child(h_row().gap_1().child(keys_hint(&p, &["Enter".into()])).child("run"))
                    .child(h_row().gap_1().child(keys_hint(&p, &["Esc".into()])).child("close"))
            });

        Some(
            div()
                .id("palette-backdrop")
                .absolute()
                .inset_0()
                .bg(p.backdrop)
                .flex()
                .flex_col()
                .items_center()
                .pt(px(96.))
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.close_palette(window, cx)))
                .child(
                    div()
                        .id("palette")
                        .w(px(620.))
                        .rounded_xl()
                        .overflow_hidden()
                        .bg(if p.dark { rgb(0x1a2029).into() } else { p.raised })
                        .border_1()
                        .border_color(p.strong)
                        .shadow_2xl()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(command),
                )
                .into_any_element(),
        )
    }

    pub fn go_view_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let view = if let Some(role) = key.strip_prefix("u:") {
            View::Unified(match role {
                "starred" => MailboxRole::Starred,
                "sent" => MailboxRole::Sent,
                "drafts" => MailboxRole::Drafts,
                _ => MailboxRole::Inbox,
            })
        } else if let Some(c) = key.strip_prefix("c:") {
            match SIDEBAR_CATEGORIES.iter().find(|x| format!("{x:?}") == c) {
                Some(c) => View::Category(*c),
                None => View::Category(Category::Important),
            }
        } else if let Some((a, m)) = key.strip_prefix("m:").and_then(|r| r.split_once('|')) {
            View::Mailbox { account: lumbridge_mail_core::AccountId::new(a), mailbox: lumbridge_mail_core::MailboxId::new(m) }
        } else {
            return;
        };
        self.set_view(view, window, cx);
    }

    pub fn render_shortcuts(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.shortcuts {
            return None;
        }
        let p = self.pal;
        let column = |groups: &[&str]| {
            v_col().flex_1().gap_4().children(groups.iter().map(|g| {
                v_col()
                    .gap_1()
                    .child(div().pb_1().text_size(px(10.5)).font_weight(FontWeight::MEDIUM).text_color(p.dim).child(g.to_uppercase()))
                    .children(COMMANDS.iter().filter(|c| c.group == *g && !c.keys.is_empty()).map(|c| {
                        h_row()
                            .h(px(26.))
                            .gap_3()
                            .child(div().flex_1().text_sm().text_color(p.text).child(c.label))
                            .child(h_row().gap_1().children(keymap::display_alternatives(c.keys).into_iter().take(2).enumerate().map(|(i, k)| {
                                h_row()
                                    .gap_1()
                                    .when(i > 0, |d| d.child(div().text_xs().text_color(p.dim).child("or")))
                                    .child(keys_hint(&p, &k))
                            })))
                    }))
            }))
        };
        Some(
            div()
                .id("shortcuts-backdrop")
                .absolute()
                .inset_0()
                .bg(p.backdrop)
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.shortcuts = false;
                    cx.notify();
                }))
                .child(
                    v_col()
                        .id("shortcuts")
                        .w(px(900.))
                        .max_w(relative(0.94))
                        .max_h(relative(0.9))
                        .overflow_y_scroll()
                        .rounded_xl()
                        .bg(p.surface)
                        .border_1()
                        .border_color(p.strong)
                        .shadow_2xl()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            h_row()
                                .px_6()
                                .h(px(52.))
                                .border_b_1()
                                .border_color(p.line)
                                .child(icon("keyboard", 16., p.muted))
                                .child(div().pl_2().flex_1().text_base().font_weight(FontWeight::SEMIBOLD).text_color(p.text).child("Keyboard shortcuts"))
                                .child(div().text_xs().text_color(p.dim).child("Single keys work when the list or reader has focus")),
                        )
                        .child(
                            h_row()
                                .items_start()
                                .gap_8()
                                .px_6()
                                .py_5()
                                .child(column(&GROUPS[0..2]))
                                .child(column(&GROUPS[2..4]))
                                .child(column(&GROUPS[4..6])),
                        ),
                )
                .into_any_element(),
        )
    }
}
