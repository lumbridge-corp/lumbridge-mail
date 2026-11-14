//! Real offscreen renders of the app on the real daemon (made-up mail, scripted model). PNGs go to
//! `target/screenshots/` (or `$LBMAIL_SHOTS`), named `daemon-*`. No window opens on any display.
//!
//!     cargo test -p lumbridge-mail-e2e --test shots_on_daemon

extern crate gpui_kit as gpui;

use lumbridge_mail_app::app::Load;
use lumbridge_mail_app::backend::rpc::Rpc;
use lumbridge_mail_e2e::{MailEnv, Window};
use std::sync::Arc;

/// Not blank: many distinct colours across the frame.
fn check(name: &str, img: &image::RgbaImage) {
    let mut colours = std::collections::HashSet::new();
    for y in (0..img.height()).step_by(7) {
        for x in (0..img.width()).step_by(11) {
            let p = img.get_pixel(x, y);
            colours.insert((p[0] / 8, p[1] / 8, p[2] / 8));
        }
    }
    assert!(colours.len() > 40, "{name}: frame looks blank ({} colours)", colours.len());
}

fn open(w: &mut Window, account: &str, subject: &str) {
    w.cursor_to(account, subject);
    w.keys("enter");
    w.wait("thread and bodies", |a, _| {
        a.open.as_ref().is_some_and(|o| o.messages.ready().is_some() && !o.rendered.is_empty())
    });
}

fn main() {
    lumbridge_mail_app::time::use_fixed_offset(Some(0));
    let mut env = MailEnv::new();
    env.start();
    env.wait_synced(4);

    {
        let mut w = Window::open_with(Arc::new(Rpc::new(env.socket())), true, true);
        w.wait("the inbox with badges", |a, _| a.threads.len() >= 15 && !a.counts.is_empty());
        check("daemon-inbox-dark", &w.screenshot("daemon-inbox-dark"));
        open(&mut w, "personal", "Dinner Saturday?");
        check("daemon-reader-dark", &w.screenshot("daemon-reader-dark"));
        w.keys("i");
        w.wait("the AI panel", |a, _| matches!(a.ai.summary, Load::Ready(_)) && matches!(a.ai.items, Load::Ready(_)));
        w.update(|a, win, cx| {
            a.ask_input.update(cx, |s, cx| s.set_value("When is dinner?", win, cx));
            a.ask(win, cx);
        });
        w.wait("an answer", |a, _| a.ai.chat.last().is_some_and(|l| !l.mine));
        check("daemon-ai-panel-dark", &w.screenshot("daemon-ai-panel-dark"));
    }
    {
        let mut w = Window::open_with(Arc::new(Rpc::new(env.socket())), true, false);
        w.wait("the inbox", |a, _| a.threads.len() >= 15);
        open(&mut w, "personal", "Quick favour");
        check("daemon-suspicious-light", &w.screenshot("daemon-suspicious-light"));
        w.keys("escape");
        open(&mut w, "ro", "Team lunch Friday");
        w.keys("s");
        check("daemon-readonly-light", &w.screenshot("daemon-readonly-light"));
        w.keys("escape");
        w.update(|a, _, cx| a.toggle_daemon_menu(cx));
        w.wait("daemon.info", |a, _| matches!(a.daemon, Some(Ok(_))));
        check("daemon-menu-light", &w.screenshot("daemon-menu-light"));
    }
    {
        let mut w = Window::open_with(Arc::new(Rpc::agent(env.socket())), true, true);
        w.wait("the inbox", |a, _| a.threads.len() >= 15);
        let ix = w.read(|a, _| a.threads.iter().position(|t| t.id.as_str() == "t-acme-code" && t.account.as_str() == "personal")).unwrap();
        w.update(|a, win, cx| a.open_at(ix, win, cx));
        w.wait("the refusal", |a, _| a.open.as_ref().is_some_and(|o| o.bodies.values().any(|b| matches!(b, Load::Failed(_)))));
        check("daemon-agent-sensitive-dark", &w.screenshot("daemon-agent-sensitive-dark"));
    }
    drop(env);
    {
        let mut env = MailEnv::first_run();
        env.start();
        let mut w = Window::open_with(Arc::new(Rpc::new(env.socket())), true, false);
        w.wait("the first-run screen", |a, _| a.needs_setup());
        check("daemon-first-run-light", &w.screenshot("daemon-first-run-light"));
        w.update(|a, win, cx| {
            a.setup_step(lumbridge_mail_app::ui::setup::Step::Imap, win, cx);
            a.setup.email.update(cx, |s, cx| s.set_value("sam@fastmail.com", win, cx));
            a.setup_discover(win, cx);
        });
        w.wait("servers found", |a, cx| a.setup.imap_host.read(cx).value() == "imap.fastmail.com");
        check("daemon-first-run-imap-light", &w.screenshot("daemon-first-run-imap-light"));
    }
    println!("shots_on_daemon: ok");
}
