//! The sidebar: unified views, AI categories, accounts by persona (anon behind its own wall).

use super::widgets::*;
use crate::app::{MailApp, View};
use crate::theme;
use gpui::prelude::*;
use gpui::*;
use lumbridge_mail_core::{Account, Category, Mailbox, MailboxRole, Persona};

fn role_icon(role: MailboxRole) -> &'static str {
    match role {
        MailboxRole::Inbox => "inbox",
        MailboxRole::Sent => "send",
        MailboxRole::Drafts => "file-pen",
        MailboxRole::Archive => "archive",
        MailboxRole::Trash => "trash",
        MailboxRole::Spam => "shield-alert",
        MailboxRole::Starred => "star",
        MailboxRole::Important => "flag",
        MailboxRole::All => "mails",
        MailboxRole::Custom => "tag",
    }
}

fn category_icon(c: Category) -> &'static str {
    match c {
        Category::Important => "flag",
        Category::Personal => "users",
        Category::Transactional => "receipt",
        Category::Newsletter => "newspaper",
        Category::Notification => "bell",
        Category::Promotion => "megaphone",
        _ => "tag",
    }
}

pub const SIDEBAR_CATEGORIES: [Category; 6] = [
    Category::Important,
    Category::Personal,
    Category::Transactional,
    Category::Newsletter,
    Category::Notification,
    Category::Promotion,
];

impl MailApp {
    #[allow(clippy::too_many_arguments)]
    fn nav_item(&self, id: SharedString, glyph: &str, glyph_color: Option<Hsla>, label: String, count: Option<u32>,
                active: bool, indent: f32, view: View, cx: &mut Context<Self>) -> Stateful<Div> {
        let p = self.pal;
        h_row()
            .id(id)
            .gap_2()
            .h(px(28.))
            .pl(px(10. + indent))
            .pr_2()
            .mx_2()
            .rounded_md()
            .when(active, |d| d.bg(p.selected))
            .when(!active, |d| d.hover(|s| s.bg(p.hover)))
            .child(icon(glyph, 14., glyph_color.unwrap_or(if active { p.text } else { p.dim })))
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .text_sm()
                    .text_color(if active { p.text } else { p.muted })
                    .when(active, |d| d.font_weight(FontWeight::MEDIUM))
                    .child(label),
            )
            .when_some(count.filter(|c| *c > 0), |d, c| {
                d.child(div().text_xs().text_color(if active { p.text } else { p.dim }).child(format_count(c)))
            })
            .on_click(cx.listener(move |this, _, window, cx| this.set_view(view.clone(), window, cx)))
    }

    fn section(&self, title: &str) -> Div {
        div()
            .px_4()
            .pt_4()
            .pb_1()
            .text_size(px(10.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(self.pal.dim)
            .child(title.to_uppercase())
    }

    fn account_block(&self, a: &Account, cx: &mut Context<Self>) -> Div {
        let p = self.pal;
        let boxes: Vec<&Mailbox> = self.mailboxes.iter().filter(|m| m.account == a.id).collect();
        let inbox = boxes.iter().find(|m| m.role == MailboxRole::Inbox);
        let expanded = self.expanded_accounts.contains(&a.id)
            || matches!(&self.view, View::Mailbox { account, .. } if *account == a.id);
        let color = p.account(a.color.as_deref());
        let in_account = matches!(&self.view, View::Mailbox { account, .. } if *account == a.id);
        let id = a.id.clone();
        let inbox_view = inbox.map(|m| View::Mailbox { account: a.id.clone(), mailbox: m.id.clone() });
        let header = h_row()
            .id(SharedString::from(format!("acct-{}", a.id)))
            .gap_2()
            .h(px(30.))
            .pl(px(10.))
            .pr_2()
            .mx_2()
            .rounded_md()
            .hover(|s| s.bg(p.hover))
            .child(dot(color, 8.))
            .child(
                v_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().text_sm().text_color(if in_account { p.text } else { p.muted })
                        .font_weight(FontWeight::MEDIUM).child(a.label.clone())),
            )
            .when(a.readonly, |d| d.child(div().id(SharedString::from(format!("ro-{}", a.id))).child(icon("lock", 11., p.dim))))
            .when_some(inbox.map(|m| m.unread).filter(|u| *u > 0), |d, u| {
                d.child(div().text_xs().text_color(p.dim).child(format_count(u)))
            })
            .child(icon(if expanded { "chevron-down" } else { "chevron-right" }, 12., p.dim))
            .on_click(cx.listener(move |this, _, window, cx| {
                if this.expanded_accounts.contains(&id) && !matches!(&this.view, View::Mailbox { account, .. } if *account == id) {
                    this.expanded_accounts.remove(&id);
                    cx.notify();
                } else if let Some(v) = inbox_view.clone() {
                    this.set_view(v, window, cx);
                }
            }));
        let mut block = v_col().child(header);
        if expanded {
            block = block.child(
                div().pl(px(26.)).pb_1().text_xs().text_color(p.dim).truncate()
                    .child(if a.readonly { format!("{} · read-only", a.email) } else { a.email.clone() }),
            );
            for m in boxes {
                let active = matches!(&self.view, View::Mailbox { mailbox, .. } if *mailbox == m.id);
                let count = matches!(m.role, MailboxRole::Inbox | MailboxRole::Custom | MailboxRole::Spam).then_some(m.unread);
                let count = if m.role == MailboxRole::Drafts { Some(m.total) } else { count };
                block = block.child(self.nav_item(
                    SharedString::from(format!("mb-{}", m.id)),
                    role_icon(m.role),
                    None,
                    m.name.clone(),
                    count,
                    active,
                    14.,
                    View::Mailbox { account: a.id.clone(), mailbox: m.id.clone() },
                    cx,
                ));
            }
        }
        block
    }

    pub fn render_sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.pal;
        let unified_unread: u32 = self
            .mailboxes
            .iter()
            .filter(|m| m.role == MailboxRole::Inbox && !self.is_anon(&m.account))
            .map(|m| m.unread)
            .sum();
        let drafts: u32 = self
            .mailboxes
            .iter()
            .filter(|m| m.role == MailboxRole::Drafts && !self.is_anon(&m.account))
            .map(|m| m.total)
            .sum();

        let mut col = v_col()
            .id("sidebar")
            .w(px(240.))
            .flex_none()
            .h_full()
            .bg(p.chrome)
            .border_r_1()
            .border_color(p.line)
            .child(
                h_row()
                    .gap_2()
                    .px_4()
                    .h(px(52.))
                    .child(
                        div()
                            .size(px(22.))
                            .rounded_md()
                            .bg(p.accent)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon("mail", 13., p.accent_fg)),
                    )
                    .child(div().flex_1().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.text).child("Lumbridge Mail"))
                    .when(self.is_demo(), |d| d.child(chip("Demo", p.working, p.working.opacity(0.14)))),
            )
            .child(
                div().px_3().pb_2().child(
                    h_row()
                        .id("compose")
                        .gap_2()
                        .h(px(34.))
                        .px_3()
                        .rounded_lg()
                        .bg(p.accent)
                        .hover(|s| s.opacity(0.92))
                        .child(icon("pen-line", 14., p.accent_fg))
                        .child(div().flex_1().text_sm().font_weight(FontWeight::MEDIUM).text_color(p.accent_fg).child("Compose"))
                        .child(div().text_xs().text_color(p.accent_fg.opacity(0.7)).child("C"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_composer(crate::ui::composer::ComposeMode::New, window, cx)
                        })),
                ),
            );

        let scroll = {
            let mut s = v_col().flex_none().pb_3();
            s = s.child(self.section("Unified"));
            for (role, label, count) in [
                (MailboxRole::Inbox, "Inbox", Some(unified_unread)),
                (MailboxRole::Starred, "Starred", None),
                (MailboxRole::Sent, "Sent", None),
                (MailboxRole::Drafts, "Drafts", Some(drafts)),
            ] {
                let active = self.view == View::Unified(role);
                s = s.child(self.nav_item(SharedString::from(format!("u-{role:?}")), role_icon(role), None,
                                          label.into(), count, active, 0., View::Unified(role), cx));
            }
            s = s.child(
                h_row().gap_1().pr_4().child(self.section("Sorted by AI")).child(div().flex_1())
                    .child(div().pt_3().child(icon("sparkles", 11., p.dim))),
            );
            for c in SIDEBAR_CATEGORIES {
                let active = self.view == View::Category(c);
                let (fg, _) = p.category(c);
                // Badges from the daemon's `counts`: unread threads in the unified inboxes per category.
                s = s.child(self.nav_item(SharedString::from(format!("c-{c:?}")), category_icon(c), Some(fg),
                                          theme::category_label(c).into(), self.category_count(c), active, 0., View::Category(c), cx));
            }

            let groups = [(Persona::Personal, "Personal"), (Persona::Work, "Work")];
            for (persona, title) in groups {
                let accts: Vec<Account> = self.accounts.iter().filter(|a| a.persona == persona).cloned().collect();
                if accts.is_empty() {
                    continue;
                }
                s = s.child(self.section(title));
                for a in &accts {
                    s = s.child(self.account_block(a, cx));
                }
            }
            let others: Vec<Account> = self.accounts.iter()
                .filter(|a| matches!(a.persona, Persona::Other(_))).cloned().collect();
            if !others.is_empty() {
                s = s.child(self.section("Other"));
                for a in &others {
                    s = s.child(self.account_block(a, cx));
                }
            }
            let anon: Vec<Account> = self.accounts.iter().filter(|a| a.persona == Persona::Anon).cloned().collect();
            if !anon.is_empty() {
                let mut wall = v_col()
                    .mx_2()
                    .mt_4()
                    .pb_1()
                    .rounded_lg()
                    .border_1()
                    .border_dashed()
                    .border_color(p.anon.opacity(0.45))
                    .bg(p.anon.opacity(0.05))
                    .child(
                        h_row()
                            .gap_1p5()
                            .px_3()
                            .pt_2()
                            .pb_1()
                            .child(icon("shield-half", 12., p.anon))
                            .child(div().text_size(px(10.5)).font_weight(FontWeight::MEDIUM).text_color(p.anon).child("ANON · KEPT APART")),
                    )
                    .child(
                        div().px_3().pb_1p5().text_size(px(11.)).text_color(p.dim)
                            .child("Never in the unified inbox, cross-account search or hosted AI."),
                    );
                for a in &anon {
                    wall = wall.child(div().mx(px(-8.)).child(self.account_block(a, cx)));
                }
                s = s.child(wall);
            }
            s
        };
        col = col.child(div().id("sidebar-scroll").flex_1().min_h_0().overflow_y_scroll().child(scroll));
        col = col.child(
            h_row()
                .gap_2()
                .px_3()
                .h(px(44.))
                .border_t_1()
                .border_color(p.line)
                .child(segmented(&p, "theme-switch", &["Dark", "Light"], usize::from(!p.dark),
                                 cx.listener(|this, ix: &usize, _, cx| {
                                     if (*ix == 0) != this.opts.dark {
                                         this.toggle_theme(cx);
                                     }
                                 })))
                .child(div().flex_1())
                .child(
                    h_row()
                        .id("shortcuts-link")
                        .gap_1()
                        .text_xs()
                        .text_color(p.dim)
                        .hover(|s| s.text_color(p.text))
                        .child(icon("keyboard", 13., p.dim))
                        .child("?")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.shortcuts = !this.shortcuts;
                            cx.notify();
                        })),
                ),
        );
        col.into_any_element()
    }
}

pub fn format_count(n: u32) -> String {
    if n >= 10_000 {
        format!("{}k", n / 1000)
    } else if n >= 1000 {
        format!("{},{:03}", n / 1000, n % 1000)
    } else {
        n.to_string()
    }
}
