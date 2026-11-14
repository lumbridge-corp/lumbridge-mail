//! Drawing. Each file adds `render_*` methods to [`MailApp`]; this one puts the window together and draws
//! the status bar.

pub mod ai;
pub mod composer;
pub mod list;
pub mod overlays;
pub mod reader;
pub mod setup;
pub mod sidebar;
pub mod widgets;

use crate::app::{Load, MailApp};
use crate::backend::LinkState;
use crate::keymap::{self, APP_CONTEXT, NAV_CONTEXT};
use gpui::prelude::*;
use gpui::*;
use widgets::*;

impl MailApp {
    fn render_status(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.pal;
        let offline = self.status.iter().any(|s| !s.online);
        let paused = self.status.iter().any(|s| s.paused);
        let queued: u32 = self.status.iter().map(|s| s.queued).sum();
        let errors: Vec<String> = self
            .status
            .iter()
            .filter_map(|s| {
                let label = self.account(&s.account).map(|a| a.label.clone()).unwrap_or_else(|| s.account.to_string());
                s.error.as_ref().map(|e| crate::app::status_error(&label, e))
            })
            .collect();
        // The daemon did not take us for a person (no token): say why Send will be refused.
        let agent = !self.is_demo() && matches!(self.link, Some(LinkState::Connected)) && matches!(self.hello, Load::Ready(_))
            && !self.backend.is_person();
        let last_sync = self.status.iter().filter_map(|s| s.last_sync).max();
        let left = h_row()
            .gap_3()
            // Click the sync state: the daemon menu (provider load, Pause/Resume, Quit).
            .child(h_row().id("status-sync").gap_1p5().px_1().rounded_sm().cursor_pointer()
                .when(self.daemon_menu, |d| d.bg(p.raised))
                .hover(|s| s.bg(p.raised))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_daemon_menu(cx)))
                .child(dot(match &self.link {
                    Some(LinkState::Down { .. }) => p.blocked,
                    Some(LinkState::Starting { .. } | LinkState::Connecting) => p.working,
                    _ if offline => p.working,
                    _ if matches!(self.hello, Load::Failed(_)) => p.blocked,
                    _ => p.done,
                }, 7.))
                .child(div().text_xs().text_color(p.muted).child(match (&self.link, &self.hello, offline) {
                    (Some(LinkState::Starting { .. }), _, _) => "Starting lumbridge-maild…".to_string(),
                    (Some(LinkState::Down { retry_in_s, .. }), _, _) => {
                        format!("{}: not connected, retrying in {retry_in_s} s", self.backend.name())
                    }
                    (_, Load::Failed(_), _) => format!("{}: not connected", self.backend.name()),
                    _ if paused => "Paused: nothing syncs (lbmail resume)".to_string(),
                    (_, _, true) => "Offline".to_string(),
                    _ if self.needs_setup() => "No accounts yet".to_string(),
                    (_, _, false) => match last_sync {
                        Some(t) => format!("Synced {}", crate::time::ago(t, self.now)),
                        None => "Syncing…".to_string(),
                    },
                }))
                .child(icon(if self.daemon_menu { "chevron-down" } else { "chevron-up" }, 10., p.dim)))
            .children(self.status.iter().map(|s| {
                let color = self.account(&s.account).map(|a| p.account(a.color.as_deref())).unwrap_or(p.dim);
                let label = self.account(&s.account).map(|a| a.label.clone()).unwrap_or_else(|| s.account.to_string());
                h_row().gap_1().child(dot(if s.online { color } else { p.dim }, 6.))
                    .child(div().text_xs().text_color(p.dim).child(label))
            }))
            .when(queued > 0, |d| d.child(chip(format!("{queued} queued"), p.working, p.working.opacity(0.14))))
            .when(agent, |d| d.child(chip("Agent connection: sending is off", p.blocked, p.blocked.opacity(0.14))))
            .when(!errors.is_empty(), |d| d.child(div().id("status-error").text_xs().text_color(p.dim).child(errors[0].clone())));

        let middle: Option<AnyElement> = if let Some(ps) = &self.pending_send {
            Some(
                h_row()
                    .gap_2()
                    .px_3()
                    .h(px(22.))
                    .rounded_full()
                    .bg(p.accent.opacity(0.16))
                    .child(icon("send", 11., p.accent))
                    .child(div().text_xs().text_color(p.text).child(format!("Sending “{}” in {} s", ps.draft.subject, ps.remaining)))
                    .child(
                        div()
                            .id("undo-send")
                            .px_1p5()
                            .rounded_sm()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.accent)
                            .hover(|s| s.bg(p.accent.opacity(0.15)))
                            .child("Undo  Z")
                            .on_click(cx.listener(|this, _, w, cx| this.undo_send(w, cx))),
                    )
                    .into_any_element(),
            )
        } else {
            self.toast.as_ref().map(|t| {
                h_row()
                    .gap_2()
                    .px_3()
                    .h(px(22.))
                    .rounded_full()
                    .bg(if t.error { p.blocked.opacity(0.16) } else { p.raised })
                    .child(icon(if t.error { "circle-alert" } else { "check" }, 11., if t.error { p.blocked } else { p.done }))
                    .child(div().text_xs().text_color(p.text).child(t.text.clone()))
                    .into_any_element()
            })
        };

        h_row()
            .h(px(30.))
            .px_3()
            .gap_3()
            .flex_none()
            .bg(p.chrome)
            .border_t_1()
            .border_color(p.line)
            .child(left)
            .child(div().flex_1().flex().justify_center().children(middle))
            .child(h_row().gap_3()
                .child(h_row().gap_1().child(keys_hint(&p, &keymap::display_keys("secondary-k"))).child(div().text_xs().text_color(p.dim).child("Commands")))
                .child(h_row().gap_1().child(keys_hint(&p, &["?".into()])).child(div().text_xs().text_color(p.dim).child("Keys")))
                .child(div().text_xs().text_color(p.dim).child(match &self.hello {
                    _ if self.is_demo() => "Demo data".to_string(),
                    Load::Ready(v) => format!("{} {v}", self.backend.name()),
                    _ => self.backend.name().to_string(),
                })))
            .into_any_element()
    }
}

impl MailApp {
    /// The daemon menu, above the status bar's sync state: whether mail syncs, each provider's load in
    /// its own words, Pause / Resume, and Quit for a daemon on this machine only (the app never stops a
    /// daemon on another machine: that one belongs to its own service manager).
    fn render_daemon_menu(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.daemon_menu {
            return None;
        }
        let p = self.pal;
        let paused = match &self.daemon {
            Some(Ok(i)) => i.paused,
            _ => self.status.iter().any(|s| s.paused),
        };
        let now = self.now;
        let (title, state, loads): (String, String, Vec<String>) = match &self.daemon {
            Some(Ok(i)) => (
                format!("{} {}", self.backend.name(), i.version),
                if paused { "Paused: no provider is called until you resume".into() } else { "Running: mail syncs".into() },
                i.load.iter().map(|l| l.describe(now)).collect(),
            ),
            Some(Err(e)) => (self.backend.name().to_string(), e.clone(), Vec::new()),
            None => (self.backend.name().to_string(), "Asking the daemon…".into(), Vec::new()),
        };
        let has_daemon = matches!(self.daemon, Some(Ok(_)));
        let quit = has_daemon && self.backend.can_quit_daemon();
        let remote = has_daemon && !self.backend.can_quit_daemon();
        let item = |id: &'static str, glyph: &'static str, label: &'static str, hint: &'static str, color: Hsla| {
            h_row()
                .id(id)
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .hover(|s| s.bg(p.raised))
                .child(icon(glyph, 13., color))
                .child(v_col()
                    .child(div().text_sm().text_color(p.text).child(label))
                    .child(div().text_xs().text_color(p.dim).child(hint)))
        };
        let menu = v_col()
            .id("daemon-menu")
            .absolute()
            .left(px(8.))
            .bottom(px(34.))
            .w(px(460.))
            .p_2()
            .gap_1()
            .rounded_lg()
            .border_1()
            .border_color(p.line)
            .bg(p.chrome)
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(h_row().gap_2().px_2().pt_1()
                .child(dot(if paused { p.working } else if has_daemon { p.done } else { p.blocked }, 7.))
                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.text).child(title)))
            .child(div().px_2().text_xs().text_color(p.muted).child(state))
            .child(v_col().px_2().py_1().gap_0p5()
                .when(has_daemon && loads.is_empty(), |d| d.child(div().text_xs().text_color(p.dim).child("No provider budget is in use")))
                .children(loads.into_iter().map(|l| div().text_xs().font_family(self.mono_font.clone()).text_color(p.muted).child(l))))
            .child(div().h(px(1.)).mx_2().bg(p.line))
            .when(has_daemon && !paused, |d| d.child(item("daemon-pause", "circle-pause", "Pause",
                "No sync, bodies or actions reach any provider; kept across restarts", p.working)
                .on_click(cx.listener(|this, _, _, cx| this.set_daemon_paused(true, cx)))))
            .when(has_daemon && paused, |d| d.child(item("daemon-resume", "circle-play", "Resume", "Sync again", p.done)
                .on_click(cx.listener(|this, _, _, cx| this.set_daemon_paused(false, cx)))))
            .when(quit, |d| d.child(item("daemon-quit", "power", "Quit lumbridge-maild",
                "Stops the daemon; the app does not start it again until it is started by hand", p.blocked)
                .on_click(cx.listener(|this, _, _, cx| this.quit_daemon(cx)))))
            .when(remote, |d| d.child(div().px_2().pb_1().text_xs().text_color(p.dim)
                .child("This daemon runs on another machine: it is stopped there, not from here")));
        Some(
            div()
                .id("daemon-menu-backdrop")
                .absolute()
                .inset_0()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.daemon_menu = false;
                    cx.notify();
                }))
                .child(menu)
                .into_any_element(),
        )
    }
}

impl Render for MailApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.pal;
        let sidebar = self.render_sidebar(cx);
        let setup = self.needs_setup();
        let list = if setup { self.render_setup(cx) } else { self.render_list(cx) };
        let reader = if setup { div().into_any_element() } else { self.render_reader(cx) };
        let ai = self.ai_panel.then(|| self.render_ai_panel(cx));
        let status = self.render_status(cx);
        let composer = self.composer.is_some().then(|| self.render_composer(_window, cx));
        let palette = self.render_palette(cx);
        let shortcuts = self.render_shortcuts(cx);
        let link_confirm = self.render_link_confirm(cx);
        let daemon_menu = self.render_daemon_menu(cx);
        if self.activate_requested {
            self.activate_requested = false;
            _window.activate_window();
        }
        if self.pending_mailto.is_some() && !self.accounts.is_empty() {
            cx.defer_in(_window, |this, window, cx| {
                if let Some(m) = this.pending_mailto.take() {
                    this.open_mailto(m, window, cx);
                }
            });
        }

        let nav = div()
            .id("nav")
            .key_context(NAV_CONTEXT)
            .track_focus(&self.focus)
            .flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(list)
            .child(reader);

        let root = div()
            .key_context(APP_CONTEXT)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(p.canvas)
            .text_color(p.text)
            .font_family(cx.global::<gpui_kit::component::Theme>().font_family.clone())
            .child(div().flex().flex_1().min_h_0().child(sidebar).child(nav).children(ai))
            .child(status)
            .children(composer)
            .children(palette)
            .children(shortcuts)
            .children(link_confirm)
            .children(daemon_menu);
        self.actions(root, cx)
    }
}
