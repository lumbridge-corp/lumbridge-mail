//! The app driven through its keys, on demo data, in a headless GPUI (no renderer, no display).

extern crate gpui_kit as gpui;

use gpui::*;
use lumbridge_mail_app::app::Load;
use lumbridge_mail_app::backend::demo::{Demo, DEFAULT_THREADS};
use lumbridge_mail_app::backend::Backend;
use lumbridge_mail_app::instance::Request;
use lumbridge_mail_app::notify::Recorder;
use lumbridge_mail_app::reading::ImageKey;
use lumbridge_mail_app::remote::Painted;
use lumbridge_mail_app::settings::Settings;
use lumbridge_mail_app::ui::composer::{ComposeMode, Field};
use lumbridge_mail_app::{MailApp, Options, View};
use lumbridge_mail_core::rpc::ThreadRef;
use lumbridge_mail_core::{AccountId, MailboxId, MailboxRole, Query};
use lumbridge_mail_render::ImageSource;
use std::sync::Arc;
use std::time::Duration;

/// Send in the composer is `secondary-enter`: Cmd on macOS, Ctrl elsewhere.
const SEND_KEY: &str = if cfg!(target_os = "macos") { "cmd-enter" } else { "ctrl-enter" };

const NOW: i64 = 1_794_328_920;

struct Harness {
    cx: HeadlessAppContext,
    window: AnyWindowHandle,
    app: Entity<MailApp>,
    demo: Arc<Demo>,
}

impl Harness {
    fn new() -> Self {
        Self::with(Options::default())
    }

    fn with(opts: Options) -> Self {
        let mut cx = HeadlessAppContext::with_platform(
            gpui_kit::platform::current_platform(true).text_system(),
            Arc::new(lumbridge_mail_app::assets()),
            || Ok(None),
        );
        cx.update(lumbridge_mail_app::init);
        // What `main` binds too (this once panicked in the real binary only).
        cx.update(lumbridge_mail_app::keymap::bind_app_keys);
        let demo = Arc::new(Demo::new(NOW, DEFAULT_THREADS));
        let backend: Arc<dyn Backend> = demo.clone();
        let (window, app) = cx
            .update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds { origin: Default::default(), size: size(px(1440.), px(900.)) })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| MailApp::new(backend, Options { now: Some(NOW), dark: true, undo_send_secs: 5, ..opts }, window, cx)),
                )
            })
            .unwrap();
        let mut h = Harness { cx, window, app, demo };
        h.settle();
        h
    }

    fn settle(&mut self) {
        self.cx.run_until_parked();
        let _ = self.cx.update_window(self.window, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        self.cx.run_until_parked();
    }

    fn keys(&mut self, keys: &'static str) {
        for k in keys.split(' ') {
            let stroke = Keystroke::parse(k).unwrap();
            self.cx.update_window(self.window, |_, window, cx| {
                window.dispatch_keystroke(stroke, cx);
            }).unwrap();
            self.settle();
        }
    }

    fn read<R>(&mut self, f: impl FnOnce(&MailApp, &App) -> R) -> R {
        let app = self.app.clone();
        self.cx.update(|cx| f(app.read(cx), cx))
    }

    fn update<R>(&mut self, f: impl FnOnce(&mut MailApp, &mut Window, &mut Context<MailApp>) -> R) -> R {
        let app = self.app.clone();
        let r = self.cx.update_window(self.window, |_, window, cx| app.update(cx, |a, cx| f(a, window, cx))).unwrap();
        self.settle();
        r
    }
}

fn inbox_has(h: &Harness, subject: &str) -> bool {
    let page = futures_lite_block(h.demo.threads(Query { role: Some(MailboxRole::Inbox), limit: 1000, ..Default::default() })).unwrap();
    page.threads.iter().any(|t| t.subject == subject)
}

fn futures_lite_block<F: std::future::Future>(f: F) -> F::Output {
    // The demo's futures never wait, so one poll finishes them.
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    let mut f = std::pin::pin!(f);
    match f.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(v) => v,
        std::task::Poll::Pending => panic!("demo future did not finish"),
    }
}

fn j_k_enter_open_and_move() {
    let mut h = Harness::new();
    assert!(h.read(|a, _| a.threads.len()) > 100, "the unified inbox loads");
    assert!(h.read(|a, _| a.threads.iter().all(|t| t.account.as_str() != "anon")), "anon stays out of unified");
    h.keys("j j k");
    assert_eq!(h.read(|a, _| a.cursor), Some(0));
    h.keys("j enter");
    let open = h.read(|a, _| a.open.as_ref().map(|o| o.summary.subject.clone()));
    assert_eq!(open.as_deref(), Some("Dinner Saturday?"));
    // Opening marks it read.
    assert_eq!(h.read(|a, _| a.threads[1].unread_count), 0);
    h.keys("escape");
    assert!(h.read(|a, _| a.open.is_none()));
    println!("ok  j/k/enter/escape");
}

fn archive_star_and_undo() {
    let mut h = Harness::new();
    let first = h.read(|a, _| a.threads[0].subject.clone());
    let flagged = h.read(|a, _| a.threads[0].flagged);
    h.keys("j s");
    assert_eq!(h.read(|a, _| a.threads[0].flagged), !flagged, "s toggles the star");
    h.keys("e");
    assert!(h.read(|a, _| a.threads.iter().all(|t| t.subject != first)), "archived rows leave the list at once");
    assert!(!inbox_has(&h, &first), "and the backend got the archive");
    h.keys("z");
    assert!(inbox_has(&h, &first), "z brings it back");
    let second = h.read(|a, _| a.threads[a.cursor.unwrap()].subject.clone());
    h.keys("#");
    assert!(!inbox_has(&h, &second), "# trashes");
    println!("ok  e/#/s/z");
}

fn multi_select_and_go_keys() {
    let mut h = Harness::new();
    h.keys("j x j x");
    assert_eq!(h.read(|a, _| a.selected.len()), 2);
    h.keys("escape");
    assert_eq!(h.read(|a, _| a.selected.len()), 0);
    h.keys("g s");
    assert_eq!(h.read(|a, _| a.view.clone()), View::Unified(MailboxRole::Starred));
    assert!(h.read(|a, _| a.threads.iter().all(|t| t.flagged)));
    h.keys("g i");
    assert_eq!(h.read(|a, _| a.view.clone()), View::Unified(MailboxRole::Inbox));
    println!("ok  x/escape/g s/g i");
}

fn compose_send_and_undo_send() {
    let mut h = Harness::new();
    h.keys("j j r");
    assert!(h.read(|a, _| a.composer.is_some()), "r opens a reply");
    let to = h.read(|a, _| a.composer.as_ref().unwrap().to.clone());
    assert!(!to.is_empty(), "replies are addressed");
    // Single letters typed in the composer are text, not commands.
    h.keys("e");
    assert!(h.read(|a, _| a.composer.is_some()));
    h.keys(SEND_KEY);
    assert!(h.read(|a, _| a.composer.is_none() && a.pending_send.is_some()), "send waits for the undo delay");
    h.update(|a, w, cx| a.undo_send(w, cx));
    assert!(h.read(|a, _| a.composer.is_some() && a.pending_send.is_none()), "undo send reopens the composer");
    h.keys(SEND_KEY);
    for _ in 0..6 {
        h.cx.advance_clock(Duration::from_secs(1));
        h.settle();
    }
    assert!(h.read(|a, _| a.pending_send.is_none()));
    let toast = h.read(|a, _| a.toast.as_ref().map(|t| t.text.clone()));
    assert!(h.read(|a, _| a.composer.is_none()), "sent, not bounced back: {toast:?}");
    let top = futures_lite_block(h.demo.threads(Query { role: Some(MailboxRole::Inbox), limit: 3, ..Default::default() })).unwrap();
    assert_eq!(top.threads[0].subject, "Dinner Saturday?");
    let sent_thread = h.read(|a, _| a.threads[0].clone());
    let msgs = futures_lite_block(h.demo.thread(ThreadRef { account: sent_thread.account.clone(), thread: sent_thread.id.clone() })).unwrap();
    assert_eq!(sent_thread.subject, "Dinner Saturday?", "the replied thread moves to the top");
    assert_eq!(msgs.len(), 5, "with the reply on the end");
    println!("ok  r/ctrl-enter/undo send");
}

fn ai_draft_fills_the_editor() {
    let mut h = Harness::new();
    h.keys("j j r");
    h.update(|a, w, cx| {
        let c = a.composer.as_ref().unwrap();
        c.ai_input.update(cx, |s, cx| s.set_value("say yes, Saturday works", w, cx));
        a.ai_draft(w, cx);
    });
    let body = h.read(|a, cx| a.composer.as_ref().unwrap().body.read(cx).value().to_string());
    assert!(body.starts_with("Hi Maya,"), "{body}");
    assert!(!body.contains("wrote:"), "the editor holds only what is written");
    let draft = h.read(|a, cx| a.composer.as_ref().unwrap().to_draft(cx));
    assert!(draft.text.starts_with("Hi Maya,"), "{}", draft.text);
    assert!(draft.text.contains("Maya Chen <maya.chen@postbox.example> wrote:"), "compose quotes the original: {}", draft.text);
    let html = draft.html.expect("formatted by default: text + HTML");
    assert!(html.contains("<blockquote"), "{html}");
    assert!(draft.in_reply_to.is_some() && draft.reply_to.is_some(), "threaded");
    println!("ok  ask AI to draft, reply quoted by compose");
}

fn open_subject(h: &mut Harness, subject: &str) {
    let ix = h.read(|a, _| a.threads.iter().position(|t| t.subject == subject)).unwrap_or_else(|| panic!("no {subject}"));
    h.update(|a, w, cx| a.open_at(ix, w, cx));
}

fn last_message(h: &mut Harness) -> lumbridge_mail_core::MessageId {
    h.read(|a, _| a.open.as_ref().unwrap().messages.ready().unwrap().last().unwrap().id.clone())
}

fn the_reader_paints_the_real_renderer() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::with(Options { fetcher: Arc::new(Painted), config_dir: Some(dir.path().to_path_buf()), ..Default::default() });
    open_subject(&mut h, "The Weekly Byte #212: the quiet return of local-first");
    let id = last_message(&mut h);
    let (remote, blocked, trackers, unsub) = h.read(|a, _| {
        let r = &a.open.as_ref().unwrap().rendered[&id];
        (r.remote, r.doc.report.remote_images, r.doc.report.tracking_pixels, r.info.unsubscribe.is_some())
    });
    assert!(!remote && blocked >= 4 && trackers >= 1 && unsub, "blocked until asked: {blocked} images, {trackers} trackers");
    assert!(h.read(|a, _| a.images.keys().all(|k| !matches!(k, ImageKey::Remote(_)))), "nothing fetched before the click");
    h.keys("m");
    let (remote, loaded, not_ready) = h.read(|a, _| {
        let r = &a.open.as_ref().unwrap().rendered[&id];
        let remote: Vec<_> = a.images.iter().filter(|(k, _)| matches!(k, ImageKey::Remote(_))).collect();
        let ready = remote.iter().filter(|(_, v)| matches!(v, Load::Ready(_))).count();
        (r.remote, ready, remote.len() - ready)
    });
    assert!(remote && loaded >= 3 && not_ready == 0, "m loads this message's images ({loaded} ready, {not_ready} not)");
    assert!(!h.read(|a, _| a.settings.sender_allowed("hello@weeklybyte.example")), "m is for this message only");
    // "Always for this sender": saved, and the next visit loads without asking.
    h.update(|a, _, cx| a.allow_remote(&id, true, false, cx));
    assert!(Settings::load(dir.path()).sender_allowed("hello@weeklybyte.example"), "persisted in the config dir");
    h.keys("escape");
    open_subject(&mut h, "The Weekly Byte #212: the quiet return of local-first");
    assert!(h.read(|a, _| a.open.as_ref().unwrap().rendered[&id].remote), "allowed sender loads at once");
    println!("ok  reader on the renderer, load images, always for this sender");
}

fn anon_accounts_confirm_every_image_load() {
    let mut h = Harness::new();
    h.update(|a, w, cx| a.set_view(View::Mailbox { account: AccountId::new("anon"), mailbox: MailboxId::new("anon:inbox") }, w, cx));
    open_subject(&mut h, "Re: Your listing: vintage synth (Juno-style, 1980s)");
    let id = last_message(&mut h);
    h.update(|a, _, cx| a.allow_remote(&id, true, false, cx));
    let (confirm, ok) = h.read(|a, _| {
        let o = a.open.as_ref().unwrap();
        (o.confirm_remote.clone(), o.remote_ok.contains(&id))
    });
    assert_eq!(confirm, Some(id.clone()), "anon asks again");
    assert!(!ok);
    assert!(h.read(|a, _| a.settings.remote_allowed_senders.is_empty()), "anon senders never join the allow list");
    h.update(|a, _, cx| a.allow_remote(&id, false, true, cx));
    assert!(h.read(|a, _| a.open.as_ref().unwrap().remote_ok.contains(&id)), "after Load anyway");
    println!("ok  anon image loads are confirmed");
}

fn hostile_mail_warns_and_links_ask_first() {
    let mut h = Harness::new();
    open_subject(&mut h, "Action required: your account is on hold");
    let id = last_message(&mut h);
    let (warnings, deceptive, removed) = h.read(|a, _| {
        let r = &a.open.as_ref().unwrap().rendered[&id];
        (lumbridge_mail_app::reading::sender_warnings(&r.info), r.doc.report.suspicious_links.len(),
         lumbridge_mail_app::reading::removed_summary(&r.doc.report))
    });
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(deceptive >= 2, "{deceptive}");
    assert!(removed.unwrap().contains("script"));
    let link = h.read(|a, _| {
        let r = &a.open.as_ref().unwrap().rendered[&id];
        r.doc.blocks.iter().find_map(|b| match b {
            lumbridge_mail_render::Block::Paragraph { spans } => spans.iter().find_map(|s| s.link.clone().filter(|l| l.warning.is_some())),
            _ => None,
        })
    }).expect("a warned link");
    h.update(|a, w, cx| a.click_link(link.clone(), w, cx));
    assert_eq!(h.read(|a, _| a.confirm_link.clone()), Some(link), "a deceptive link asks before it opens");
    println!("ok  hostile mail: sender warnings, deceptive links ask first");
}

fn inline_parts_and_forwarded_files() {
    let mut h = Harness::new();
    open_subject(&mut h, "Floor plan for Quay Street");
    let id = last_message(&mut h);
    let part = h.read(|a, _| {
        let r = &a.open.as_ref().unwrap().rendered[&id];
        lumbridge_mail_app::reading::images(&r.doc).into_iter().find_map(|s| match s {
            ImageSource::Cid { attachment: Some(att), .. } => Some(att.clone()),
            _ => None,
        })
    }).expect("the cid image resolves to its part");
    assert!(h.read(|a, _| matches!(a.images.get(&ImageKey::Part(part.clone())), Some(Load::Ready(_)))), "inline images show at once");
    h.keys("f");
    let (atts, fetching, subject) = h.read(|a, cx| {
        let c = a.composer.as_ref().unwrap();
        (c.attachments.clone(), c.fetching, c.subject.read(cx).value().to_string())
    });
    assert_eq!(subject, "Fwd: Floor plan for Quay Street");
    assert_eq!(fetching, 0);
    assert_eq!(atts.len(), 2, "a forward carries the original's files: {atts:?}");
    assert!(atts.iter().all(|a| std::path::Path::new(&a.path).is_file()));
    let draft = h.read(|a, cx| a.composer.as_ref().unwrap().to_draft(cx));
    assert!(draft.text.contains("Forwarded message"), "{}", draft.text);
    println!("ok  cid images, forward carries attachments");
}

fn attachments_open_and_save_but_programs_only_save() {
    use lumbridge_mail_app::app::{FileAction, FileState};
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let opener = Arc::new(lumbridge_mail_app::files::Recorder::default());
    let downloads = dir.path().join("Downloads");
    let mut h = Harness::with(Options { opener: opener.clone(), downloads: Some(downloads.clone()), ..Default::default() });
    open_subject(&mut h, "Your order #4471-2290 has shipped");
    let id = last_message(&mut h);
    let (account, att) = h.read(|a, _| {
        let o = a.open.as_ref().unwrap();
        let b = o.bodies[&id].ready().unwrap();
        (o.summary.account.clone(), b.attachments[0].clone())
    });
    assert_eq!(att.filename, "invoice-4471-2290.pdf");
    h.update(|a, _, cx| a.file_action(account.clone(), id.clone(), att.clone(), FileAction::Open, cx));
    let opened = opener.opened.lock().clone();
    assert_eq!(opened.len(), 1, "opened with the system's program");
    assert!(opened[0].ends_with("invoice-4471-2290.pdf"), "{opened:?}");
    let key = (account.clone(), id.clone(), att.id.clone());
    assert!(h.read(|a, _| matches!(a.files.get(&key), Some(FileState::Ready(_)))));
    // Save: a private copy in Downloads under the mail's (safe) name; twice = a second name.
    h.update(|a, _, cx| a.file_action(account.clone(), id.clone(), att.clone(), FileAction::Save, cx));
    let saved = downloads.join("invoice-4471-2290.pdf");
    assert!(saved.is_file(), "saved to Downloads");
    assert_eq!(std::fs::metadata(&saved).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(h.read(|a, _| a.toast.as_ref().is_some_and(|t| !t.error && t.text.contains("Saved to"))));
    h.update(|a, _, cx| a.file_action(account.clone(), id.clone(), att.clone(), FileAction::Save, cx));
    assert!(downloads.join("invoice-4471-2290 (2).pdf").is_file(), "never overwrites");
    // A program is never opened, whatever it is called; nothing is even fetched.
    let mut evil = att.clone();
    evil.filename = "invoice.pdf.exe".into();
    evil.id = lumbridge_mail_core::AttachmentId::new("evil");
    h.update(|a, _, cx| a.file_action(account.clone(), id.clone(), evil.clone(), FileAction::Open, cx));
    assert_eq!(opener.opened.lock().len(), 1, "not opened");
    let toast = h.read(|a, _| a.toast.clone().map(|t| (t.text, t.error)));
    assert!(matches!(&toast, Some((t, true)) if t.contains("was not opened") && t.contains(".exe")), "{toast:?}");
    assert!(h.read(|a, _| !a.files.contains_key(&(account.clone(), id.clone(), evil.id.clone()))));
    println!("ok  attachments open and save; programs only save");
}

fn capabilities_hide_label_and_disable_actions() {
    use lumbridge_mail_app::app::{Offer, Verb};
    use lumbridge_mail_core::{Capabilities, Support};
    let mut h = Harness::new();
    let work = AccountId::new("work");
    // A Resend-like account: read state only on this device, no archive, no sending.
    h.update(|a, _, cx| {
        a.caps.insert(work.clone(), Capabilities { seen: Support::Local, archive: Support::Unsupported, send: false, ..Capabilities::default() });
        cx.notify();
    });
    assert_eq!(h.read(|a, _| a.offer(&work, Verb::Archive)), Offer::Hidden);
    assert_eq!(h.read(|a, _| a.offer(&work, Verb::Read)), Offer::Local);
    assert_eq!(h.read(|a, _| a.offer(&work, Verb::Trash)), Offer::Server);
    assert_eq!(h.read(|a, _| a.offer(&work, Verb::Reply)), Offer::Hidden);
    assert_eq!(h.read(|a, _| a.offer(&AccountId::new("personal"), Verb::Archive)), Offer::Server, "no capabilities: the server");
    let view = View::Mailbox { account: work.clone(), mailbox: MailboxId::new("work:inbox") };
    h.update(|a, w, cx| a.set_view(view, w, cx));
    let subject = h.read(|a, _| a.threads[0].subject.clone());
    h.update(|a, w, cx| a.open_at(0, w, cx));
    h.keys("e");
    assert!(h.read(|a, _| a.threads.iter().any(|t| t.subject == subject)), "not archived");
    let toast = h.read(|a, _| a.toast.clone().map(|t| t.text)).unwrap_or_default();
    assert!(toast.contains("cannot change the archive"), "{toast}");
    h.keys("u");
    let toast = h.read(|a, _| a.toast.clone().map(|t| t.text)).unwrap_or_default();
    assert!(toast.ends_with("on this device only"), "{toast}");
    // A selection across accounts offers what any of them can.
    assert_eq!(h.read(|a, _| a.offer_for(&[work.clone(), AccountId::new("personal")], Verb::Archive)), Offer::Server);
    // A read-only account: its changes are shown disabled, with why; replies stay (another address can send).
    h.update(|a, _, cx| {
        a.accounts.iter_mut().find(|x| x.id == work).unwrap().readonly = true;
        cx.notify();
    });
    let star = h.read(|a, _| a.offer(&work, Verb::Star));
    assert!(matches!(&star, Offer::Disabled(why) if why.contains("read-only")), "{star:?}");
    assert_eq!(h.read(|a, _| a.offer(&work, Verb::Reply)), Offer::Server);
    println!("ok  capabilities: unsupported hidden, local labelled, read-only disabled");
}

fn contact_suggestions_respect_the_anon_wall() {
    let mut h = Harness::new();
    h.update(|a, w, cx| a.open_composer(ComposeMode::New, w, cx));
    let type_to = |h: &mut Harness, text: &'static str| {
        h.update(|a, w, cx| {
            let c = a.composer.as_mut().unwrap();
            c.suggest_field = Some(Field::To);
            c.to_input.update(cx, |s, cx| s.set_value(text, w, cx));
        });
        h.read(|a, cx| a.suggestions(Field::To, cx))
    };
    let from_personal = type_to(&mut h, "ma");
    assert!(from_personal.iter().any(|a| a.email == "maya.chen@postbox.example"), "{from_personal:?}");
    assert!(type_to(&mut h, "kes").is_empty(), "anon contacts never in a personal draft");
    let anon = h.read(|a, _| a.accounts.iter().position(|x| x.id.as_str() == "anon").unwrap());
    h.update(|a, _, _| a.composer.as_mut().unwrap().from = anon);
    assert_eq!(type_to(&mut h, "kes").first().map(|a| a.email.clone()).as_deref(), Some("kestrel92@relaymail.example"));
    assert!(type_to(&mut h, "dana").is_empty(), "work contacts never in an anon draft");
    println!("ok  contact suggestions behind the anon wall");
}

fn notifications_mailto_and_second_instance() {
    let recorder = Arc::new(Recorder::default());
    let (tx, rx) = async_channel::unbounded();
    let mut h = Harness::with(Options { notifier: recorder.clone(), requests: Some(rx), ..Default::default() });
    h.update(|a, _, _| a.window_active = false);
    futures_lite_block(h.demo.sync(None)).unwrap();
    h.settle();
    let notes = recorder.0.lock().clone();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].title, "Maya Chen");
    // Turned off: nothing.
    h.update(|a, _, cx| a.toggle_notifications(false, cx));
    futures_lite_block(h.demo.sync(None)).unwrap();
    h.settle();
    assert_eq!(recorder.0.lock().len(), 1);
    // A second `lumbridge-mail mailto:...` hands its URL over: the composer opens, filled in.
    tx.send_blocking(Request::Open("mailto:jonah@parkfamily.example?subject=Hike%20photos&body=Send%20them%21".into())).unwrap();
    h.settle();
    h.settle();
    let (to, subject, body) = h.read(|a, cx| {
        let c = a.composer.as_ref().expect("mailto opens the composer");
        (c.to.clone(), c.subject.read(cx).value().to_string(), c.body.read(cx).value().to_string())
    });
    assert_eq!(to[0].email, "jonah@parkfamily.example");
    assert_eq!(subject, "Hike photos");
    assert_eq!(body, "Send them!");
    println!("ok  notifications (setting), mailto from a second instance");
}

fn palette_shortcuts_ai_panel_search() {
    let mut h = Harness::new();
    h.keys("ctrl-k");
    assert!(h.read(|a, _| a.palette.is_some()));
    h.keys("escape");
    assert!(h.read(|a, _| a.palette.is_none()));
    h.keys("?");
    assert!(h.read(|a, _| a.shortcuts));
    h.keys("escape");
    assert!(!h.read(|a, _| a.shortcuts));
    h.keys("j enter i");
    assert!(h.read(|a, _| a.ai_panel));
    assert!(h.read(|a, _| a.ai.summary.ready().is_some()), "summary loads for the open thread");
    h.update(|a, w, cx| {
        a.search.update(cx, |s, cx| s.set_value("is:starred", w, cx));
        a.run_search(w, cx);
    });
    assert!(h.read(|a, _| !a.threads.is_empty() && a.threads.iter().all(|t| t.flagged)));
    println!("ok  ctrl-k/?/i/search");
}

fn sensitive_threads_skip_the_model() {
    let mut h = Harness::new();
    let ix = h.read(|a, _| a.threads.iter().position(|t| t.subject == "Your sign-in code").unwrap());
    h.update(|a, w, cx| {
        a.toggle_ai_panel(cx);
        a.open_at(ix, w, cx);
    });
    assert!(h.read(|a, _| matches!(a.ai.summary, lumbridge_mail_app::app::Load::Failed(_))));
    println!("ok  sensitive mail never summarised");
}

fn fifty_thousand_threads_scroll() {
    let mut h = Harness::new();
    let big: Arc<dyn Backend> = Arc::new(Demo::new(NOW, 50_000));
    h.update(|a, _, cx| {
        a.backend = big;
        a.load_threads(false, cx);
    });
    let n = h.read(|a, _| a.threads.len());
    assert_eq!(n, lumbridge_mail_app::app::PAGE as usize);
    h.update(|a, _, cx| a.maybe_load_more(n - 1, cx));
    assert!(h.read(|a, _| a.threads.len()) > n, "the next page loads near the end");
    println!("ok  50k paging");
}

fn main() {
    lumbridge_mail_app::time::use_fixed_offset(Some(0));
    j_k_enter_open_and_move();
    archive_star_and_undo();
    multi_select_and_go_keys();
    compose_send_and_undo_send();
    ai_draft_fills_the_editor();
    palette_shortcuts_ai_panel_search();
    sensitive_threads_skip_the_model();
    fifty_thousand_threads_scroll();
    the_reader_paints_the_real_renderer();
    anon_accounts_confirm_every_image_load();
    hostile_mail_warns_and_links_ask_first();
    inline_parts_and_forwarded_files();
    attachments_open_and_save_but_programs_only_save();
    capabilities_hide_label_and_disable_actions();
    contact_suggestions_respect_the_anon_wall();
    notifications_mailto_and_second_instance();
    println!("behaviour: ok");
}
