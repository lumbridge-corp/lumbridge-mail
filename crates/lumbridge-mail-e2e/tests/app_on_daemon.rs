//! The app, through its real `Rpc` client, against the real daemon (in-process, made-up mail, a scripted
//! model), driven by keystrokes like `lumbridge-mail-app/tests/behaviour.rs` drives it on the demo.
//!
//!     cargo test -p lumbridge-mail-e2e --test app_on_daemon

extern crate gpui_kit as gpui;

use lumbridge_mail_app::app::{Load, Offer, Verb};
use lumbridge_mail_app::backend::rpc::Rpc;
use lumbridge_mail_app::backend::LinkState;
use lumbridge_mail_app::ui::setup::Step;
use lumbridge_mail_core::*;
use lumbridge_mail_e2e::{MailEnv, Window};
use lumbridge_maild::DemoMail;
use std::sync::Arc;

/// Send in the composer is `secondary-enter`: Cmd on macOS, Ctrl elsewhere.
const SEND_KEY: &str = if cfg!(target_os = "macos") { "cmd-enter" } else { "ctrl-enter" };

fn has_inbox(m: &MessageMeta) -> bool {
    m.mailboxes.iter().any(|b| b.as_str() == lumbridge_maild::demo::INBOX)
}

fn thread_messages(env: &MailEnv, account: &str, subject: &str) -> Vec<MessageMeta> {
    let d = env.demo(account);
    let first = d.find(subject).unwrap_or_else(|| panic!("{account} has no {subject:?}"));
    d.messages().into_iter().filter(|m| m.thread == first.thread).collect()
}

fn the_window_loads_from_the_daemon(env: &MailEnv, rpc: &Arc<Rpc>, w: &mut Window) {
    w.wait("accounts and the unified inbox", |a, _| a.accounts.len() == 4 && a.threads.len() >= 15);
    assert!(rpc.is_person(), "the person token was accepted on connect");
    assert!(w.read(|a, _| matches!(a.hello, Load::Ready(_))));
    let accounts: Vec<String> = w.read(|a, _| a.threads.iter().map(|t| t.account.to_string()).collect());
    assert!(!accounts.iter().any(|a| a == "anon"), "anon stays out of Unified");
    for want in ["personal", "work", "ro"] {
        assert!(accounts.iter().any(|a| a == want), "{want} is in Unified");
    }
    assert!(w.read(|a, _| a.is_readonly(&AccountId::new("ro")) && !a.is_readonly(&AccountId::new("personal"))),
            "the daemon says which accounts are read-only");
    // `accounts.capabilities` from the daemon: the read-only account's changes are offered disabled.
    w.wait("capabilities", |a, _| a.caps.len() == 4);
    assert!(w.read(|a, _| matches!(a.offer(&AccountId::new("ro"), Verb::Archive), Offer::Disabled(_))
                        && a.offer(&AccountId::new("personal"), Verb::Archive) == Offer::Server));
    // Triage from the daemon: category badges (counts), and the injection attempt flagged.
    w.wait("category badges from counts", |a, _| a.category_count(Category::Important).is_some_and(|n| n > 0));
    let favour = w.read(|a, _| a.threads.iter().find(|t| t.subject == "Quick favour").and_then(|t| t.triage.clone()));
    assert!(favour.is_some_and(|t| t.suspicious), "Triage.suspicious reaches the list");
    let _ = env;
    println!("ok  list, accounts, anon wall, read-only flag, counts, suspicious");
}

fn open_read_and_render(env: &MailEnv, w: &mut Window) {
    w.cursor_to("personal", "Dinner Saturday?");
    w.keys("enter");
    w.wait("the thread's three messages", |a, _| a.open.as_ref().and_then(|o| o.messages.ready()).is_some_and(|m| m.len() == 3));
    let last = w.read(|a, _| a.open.as_ref().unwrap().messages.ready().unwrap().last().unwrap().id.clone());
    w.wait("the newest body through the renderer", |a, _| {
        a.open.as_ref().and_then(|o| o.rendered.get(&last)).is_some_and(|r| !r.doc.blocks.is_empty())
    });
    // Opening marked it read, through the daemon's queue, on the "server".
    let d = env.demo("personal");
    lumbridge_mail_e2e::wait_real("seen on the server", std::time::Duration::from_secs(15), || {
        d.message(&last).is_some_and(|m| m.flags.seen)
    });
    println!("ok  open, body through the renderer, read on the server");
}

fn star_archive_undo_round_trip(env: &MailEnv, w: &mut Window) {
    let d = env.demo("personal");
    w.keys("escape");
    w.cursor_to("personal", "Rent split for December");
    w.keys("s");
    lumbridge_mail_e2e::wait_real("the star reached the server", std::time::Duration::from_secs(15), || {
        thread_messages(env, "personal", "Rent split for December").iter().all(|m| m.flags.flagged)
    });
    w.wait("the starred row, as the daemon reports it", |a, _| {
        a.threads.iter().any(|t| t.subject == "Rent split for December" && t.flagged)
    });
    w.keys("e");
    assert!(w.read(|a, _| !a.threads.iter().any(|t| t.subject == "Rent split for December")), "archived rows leave at once");
    lumbridge_mail_e2e::wait_real("archived on the server", std::time::Duration::from_secs(15), || {
        thread_messages(env, "personal", "Rent split for December").iter().all(|m| !has_inbox(m))
    });
    w.keys("z");
    lumbridge_mail_e2e::wait_real("back in the inbox on the server", std::time::Duration::from_secs(15), || {
        thread_messages(env, "personal", "Rent split for December").iter().all(has_inbox)
    });
    // Undo is not optimistic: the row comes back only because the daemon's events reloaded the list.
    w.wait("the row back from the daemon's events", |a, _| a.threads.iter().any(|t| t.subject == "Rent split for December"));
    let moves = d.applied().iter().filter(|a| matches!(a, Action::Move { from: None, .. })).count();
    assert!(moves >= 1, "undo of an archive is Move {{ from: None }}: add the inbox, remove nothing");
    println!("ok  s/e/z through the daemon's queue and back as events");
}

fn search(w: &mut Window) {
    w.keys("/");
    w.update(|a, win, cx| {
        a.search.update(cx, |s, cx| s.set_value("from:maya", win, cx));
        a.run_search(win, cx);
    });
    w.wait("search results", |a, _| matches!(a.list, Load::Ready(())) && a.search_text.is_some() && !a.threads.is_empty());
    let all_maya = w.read(|a, _| a.threads.iter().all(|t| t.participants.iter().any(|p| p.email.contains("maya"))));
    assert!(all_maya, "the daemon's search operators");
    w.keys("escape");
    w.wait("back to the inbox", |a, _| a.search_text.is_none() && a.threads.len() >= 15);
    println!("ok  / search on the daemon");
}

fn ai_panel(w: &mut Window) {
    w.cursor_to("personal", "Dinner Saturday?");
    w.keys("enter i");
    w.wait("summary and action items", |a, _| {
        matches!(&a.ai.summary, Load::Ready(s) if s.contains("7:30")) && matches!(&a.ai.items, Load::Ready(x) if !x.items.is_empty())
    });
    let item = w.read(|a, _| match &a.ai.items {
        Load::Ready(x) => (x.items[0].text.clone(), x.items[0].owner, x.by.clone()),
        _ => unreachable!(),
    });
    assert_eq!((item.0.as_str(), item.1), ("Book a table for four", Owner::Me), "ai.extract, not the triage stand-in");
    assert_ne!(item.2, "rules");
    w.update(|a, win, cx| {
        a.ask_input.update(cx, |s, cx| s.set_value("When is dinner?", win, cx));
        a.ask(win, cx);
    });
    w.wait("an answer", |a, _| a.ai.chat.last().is_some_and(|l| !l.mine));
    let (text, sources) = w.read(|a, _| {
        let l = a.ai.chat.last().unwrap();
        (l.text.clone(), l.sources.len())
    });
    assert!(text.contains("Saturday at 7:30") && sources == 1, "{text} ({sources} sources)");
    // The sensitive thread is never sent to a model (the app does not even ask).
    w.keys("escape");
    w.cursor_to("personal", "Your Acme verification code");
    w.keys("enter");
    w.wait("the sensitive thread", |a, _| a.open.as_ref().is_some_and(|o| o.summary.subject.contains("Acme")));
    assert!(w.read(|a, _| matches!(a.ai.summary, Load::Failed(_))));
    w.keys("i escape");
    println!("ok  AI panel: summary, ai.extract, ask, sensitive kept out");
}

fn reply_and_send_as_a_person(env: &MailEnv, w: &mut Window) {
    let before = env.demo("personal").sent().len();
    w.cursor_to("personal", "Dinner Saturday?");
    w.keys("enter");
    w.wait("thread open", |a, _| a.open.as_ref().and_then(|o| o.messages.ready()).is_some());
    w.keys("r");
    assert!(w.read(|a, _| a.composer.is_some()), "r opens a reply");
    w.update(|a, win, cx| {
        let c = a.composer.as_ref().unwrap();
        c.body.update(cx, |s, cx| s.set_value("Booked, see you at 7:30.", win, cx));
    });
    w.keys(SEND_KEY);
    assert!(w.read(|a, _| a.pending_send.is_some()), "the undo delay");
    w.advance(4);
    lumbridge_mail_e2e::wait_real("the send reached the demo provider's Sent", std::time::Duration::from_secs(15), || {
        env.demo("personal").sent().len() == before + 1
    });
    let sent = env.demo("personal").sent().last().cloned().unwrap();
    assert_eq!(sent.subject, "Re: Dinner Saturday?");
    assert!(sent.text.contains("Booked, see you at 7:30."), "{}", sent.text);
    assert!(sent.to.iter().any(|a| a.email == "maya@friends.example"));
    w.wait("Sent, said once the queue is empty", |a, _| {
        a.composer.is_none() && a.sending.is_empty() && a.toast.as_ref().is_some_and(|t| t.text.starts_with("Sent"))
    });
    println!("ok  reply + Send as a person lands in Sent");
}

/// The status bar's daemon menu: the daemon's state and load, Pause and Resume through `daemon.pause` /
/// `daemon.resume`, and Quit offered for a daemon on this machine.
fn daemon_menu_pause_and_resume(w: &mut Window) {
    w.update(|a, _, cx| a.toggle_daemon_menu(cx));
    w.wait("daemon.info", |a, _| matches!(&a.daemon, Some(Ok(i)) if !i.paused));
    assert!(w.read(|a, _| a.backend.can_quit_daemon()), "a daemon on this machine may be stopped from the app");
    w.update(|a, _, cx| a.set_daemon_paused(true, cx));
    w.wait("paused", |a, _| matches!(&a.daemon, Some(Ok(i)) if i.paused) && a.status.iter().all(|s| s.paused));
    w.update(|a, _, cx| a.set_daemon_paused(false, cx));
    w.wait("resumed", |a, _| matches!(&a.daemon, Some(Ok(i)) if !i.paused) && a.status.iter().all(|s| !s.paused));
    w.update(|a, _, cx| a.toggle_daemon_menu(cx));
    assert!(w.read(|a, _| !a.daemon_menu));
    println!("ok  daemon menu: info, pause, resume");
}

fn read_only_accounts(env: &MailEnv, w: &mut Window) {
    w.keys("escape");
    let applied = env.demo("ro").applied().len();
    w.cursor_to("ro", "Team lunch Friday");
    let flagged = w.read(|a, _| a.threads[a.cursor.unwrap()].flagged);
    w.keys("s");
    let toast = w.read(|a, _| a.toast.as_ref().map(|t| (t.text.clone(), t.error)));
    assert!(toast.as_ref().is_some_and(|(t, err)| t.contains("read-only") && !err), "explained, not an error: {toast:?}");
    assert_eq!(w.read(|a, _| a.threads[a.cursor.unwrap()].flagged), flagged, "no optimistic change");
    w.keys("e");
    assert!(w.read(|a, _| a.threads.iter().any(|t| t.account.as_str() == "ro" && t.subject == "Team lunch Friday")), "not archived");
    w.keys("enter");
    w.wait("the read-only thread", |a, _| a.open.as_ref().and_then(|o| o.messages.ready()).is_some());
    w.keys("r");
    w.keys(SEND_KEY);
    let err = w.read(|a, _| a.composer.as_ref().and_then(|c| c.error.clone()));
    assert!(err.as_ref().is_some_and(|e| e.contains("read-only")), "Send explains: {err:?}");
    w.keys("escape");
    assert!(w.read(|a, _| a.toast.as_ref().is_some_and(|t| t.text.contains("Draft not saved"))));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(env.demo("ro").applied().len(), applied, "nothing reached the read-only server");
    assert!(env.demo("ro").messages().iter().any(|m| !m.flags.seen), "not even the unread flag");
    w.keys("escape");
    println!("ok  read-only account: actions explained, nothing sent to the server");
}

fn resync_needed_is_handled(env: &MailEnv, w: &mut Window) {
    let d = env.demo("personal");
    d.expire_history();
    d.deliver(DemoMail::new("jacket", "Maya Chen", "maya@friends.example", "Bring a jacket", "It gets cold after 9."));
    w.wait("new mail after a forced full resync", |a, _| a.threads.iter().any(|t| t.subject == "Bring a jacket"));
    assert!(w.read(|a, _| !a.toast.as_ref().is_some_and(|t| t.error)), "a resync is not an error to show");
    println!("ok  resync_needed: the daemon resyncs, the window just updates");
}

fn the_daemon_restarts(env: &mut MailEnv, rpc: &Arc<Rpc>, w: &mut Window) {
    let old_token = std::fs::read_to_string(env.paths.person_token()).unwrap();
    env.kill();
    w.wait("the window notices", |a, _| matches!(a.link, Some(LinkState::Down { .. })));
    env.start();
    let new_token = std::fs::read_to_string(env.paths.person_token()).unwrap();
    assert_ne!(old_token, new_token, "a new token at every start");
    w.wait("reconnected and reloaded", |a, _| matches!(a.link, Some(LinkState::Connected)) && a.threads.len() >= 15);
    lumbridge_mail_e2e::wait_real("the new token was shown", std::time::Duration::from_secs(10), || rpc.is_person());
    // Still a person: a change goes through.
    w.cursor_to("work", "Team lunch Friday");
    w.keys("s");
    lumbridge_mail_e2e::wait_real("a star after the restart", std::time::Duration::from_secs(15), || {
        thread_messages(env, "work", "Team lunch Friday").iter().all(|m| m.flags.flagged)
    });
    println!("ok  daemon killed and restarted: reconnects, new token, still a person");
}

fn an_agent_connection(env: &MailEnv) {
    let rpc = Arc::new(Rpc::agent(env.socket()));
    let mut w = Window::open(rpc.clone());
    w.wait("the inbox for an agent", |a, _| a.threads.len() >= 15);
    assert!(!rpc.is_person());
    assert!(w.read(|a, _| !a.backend.is_person()), "the window knows it cannot send");
    // Sensitive mail: one line only.
    let code = w.read(|a, _| a.threads.iter().find(|t| t.id.as_str() == "t-acme-code" && t.account.as_str() == "personal").cloned()).unwrap();
    assert_eq!(code.subject, "A security code from Acme");
    assert!(code.snippet.is_empty());
    let ix = w.read(|a, _| a.threads.iter().position(|t| t.id == code.id && t.account == code.account)).unwrap();
    w.update(|a, win, cx| a.open_at(ix, win, cx));
    w.wait("the body refused", |a, _| a.open.as_ref().is_some_and(|o| o.bodies.values().any(|b| matches!(b, Load::Failed(e) if e.contains("only to a person")))));
    // Send without the person token: refused by the daemon, said plainly, nothing sent.
    let before = env.demo("personal").sent().len();
    w.keys("escape");
    w.cursor_to("personal", "Dinner Saturday?");
    w.keys("enter");
    w.wait("thread open", |a, _| a.open.as_ref().and_then(|o| o.messages.ready()).is_some());
    w.keys("r");
    w.update(|a, win, cx| {
        let c = a.composer.as_ref().unwrap();
        c.body.update(cx, |s, cx| s.set_value("This must not go out.", win, cx));
    });
    w.keys(SEND_KEY);
    w.advance(4);
    w.wait("refused", |a, _| a.composer.as_ref().and_then(|c| c.error.as_ref()).is_some_and(|e| e.contains("isn't trusted to send")));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(env.demo("personal").sent().len(), before, "nothing was sent");
    println!("ok  agent connection: sensitive mail one line only, Send refused");
}

fn first_run() {
    let mut env = MailEnv::first_run();
    env.start();
    let rpc = Arc::new(Rpc::new(env.socket()));
    let mut w = Window::open(rpc);
    w.wait("the first-run screen", |a, _| a.needs_setup());
    // A form with nothing in it says what is missing.
    w.update(|a, win, cx| a.setup_step(Step::Gog, win, cx));
    w.update(|a, _, cx| a.setup_submit(cx));
    assert!(w.read(|a, _| a.setup.error.as_ref().is_some_and(|e| e.contains("email"))));
    // "Try with demo mail".
    w.update(|a, win, cx| a.setup_step(Step::Choose, win, cx));
    w.update(|a, _, cx| a.setup_demo(cx));
    w.wait("three demo accounts and their mail", |a, _| a.accounts.len() == 3 && !a.needs_setup() && a.threads.len() >= 10);
    assert!(w.read(|a, _| a.threads.iter().all(|t| t.account.as_str() != "demo-anon")), "the demo anon account stays out of Unified");
    // Add Gmail through gog: read-only by default.
    w.update(|a, win, cx| {
        a.setup_step(Step::Gog, win, cx);
        a.setup.email.update(cx, |s, cx| s.set_value("sam@gmail.example", win, cx));
        a.setup.name.update(cx, |s, cx| s.set_value("Sam Example", win, cx));
        a.setup.gog_where.update(cx, |s, cx| s.set_value("ssh:gog-host", win, cx));
    });
    assert!(w.read(|a, _| a.setup.readonly), "gog starts read-only");
    w.update(|a, _, cx| a.setup_submit(cx));
    w.wait("the gog account", |a, _| a.accounts.iter().any(|x| x.id.as_str() == "sam-gmail" && x.readonly));
    let config = std::fs::read_to_string(env.paths.config_file()).unwrap();
    assert!(config.contains("ssh_host = \"gog-host\"") && config.contains("readonly = true"), "{config}");
    // IMAP: servers from the daemon's discovery, password to the keyring only.
    w.update(|a, win, cx| {
        a.setup_step(Step::Imap, win, cx);
        a.setup.email.update(cx, |s, cx| s.set_value("sam@fastmail.com", win, cx));
        a.setup_discover(win, cx);
    });
    w.wait("servers found", |a, cx| a.setup.imap_host.read(cx).value() == "imap.fastmail.com");
    w.update(|a, win, cx| a.setup.password.update(cx, |s, cx| s.set_value("made-up-password", win, cx)));
    w.update(|a, _, cx| a.setup_submit(cx));
    w.wait("the IMAP account", |a, _| a.accounts.iter().any(|x| x.id.as_str() == "sam-fastmail"));
    assert_eq!(env.keyring.0.lock().unwrap().as_slice(), &[("sam-fastmail".to_string(), "made-up-password".to_string())]);
    let config = std::fs::read_to_string(env.paths.config_file()).unwrap();
    assert!(config.contains("imap.fastmail.com") && !config.contains("made-up-password"), "{config}");
    println!("ok  first run: demo mail, gog (read-only), IMAP (discovery, keyring)");
}

fn main() {
    lumbridge_mail_app::time::use_fixed_offset(Some(0));
    let mut env = MailEnv::new();
    env.start();
    env.wait_synced(4);
    let rpc = Arc::new(Rpc::new(env.socket()));
    let mut w = Window::open(rpc.clone());
    the_window_loads_from_the_daemon(&env, &rpc, &mut w);
    open_read_and_render(&env, &mut w);
    star_archive_undo_round_trip(&env, &mut w);
    search(&mut w);
    ai_panel(&mut w);
    reply_and_send_as_a_person(&env, &mut w);
    read_only_accounts(&env, &mut w);
    daemon_menu_pause_and_resume(&mut w);
    resync_needed_is_handled(&env, &mut w);
    an_agent_connection(&env);
    the_daemon_restarts(&mut env, &rpc, &mut w);
    drop(w);
    first_run();
    println!("app_on_daemon: ok");
}
