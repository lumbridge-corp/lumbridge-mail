//! Real offscreen renders of the app on demo data, saved as PNGs (GPUI's headless renderer: Metal on
//! macOS, WGPU on Linux, e.g. Mesa's lavapipe). No window is opened on any display.
//!
//!     cargo test -p lumbridge-mail-app --test screenshots          # writes target/screenshots/*.png
//!     LBMAIL_SHOTS=/some/dir cargo test ... --test screenshots      # somewhere else
//!
//! Each shot also checks a few pixels, so a blank or wrongly themed frame fails the test.

extern crate gpui_kit as gpui;

use gpui::*;
use lumbridge_mail_app::backend::demo::{Demo, DEFAULT_THREADS};
use lumbridge_mail_app::backend::rpc::Rpc;
use lumbridge_mail_app::backend::Backend;
use lumbridge_mail_app::ui::composer::ComposeMode;
use lumbridge_mail_app::{MailApp, Options, View};
use lumbridge_mail_core::{AccountId, MailboxId};
use std::path::PathBuf;
use std::sync::Arc;

/// Tue 10 Nov 2026, 16:42 UTC.
const NOW: i64 = 1_794_328_920;
const W: f32 = 1440.;
const H: f32 = 900.;

fn out_dir() -> PathBuf {
    let dir = std::env::var_os("LBMAIL_SHOTS").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots")
    });
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

type Setup = Box<dyn FnOnce(&mut MailApp, &mut Window, &mut Context<MailApp>)>;

struct Shot {
    name: &'static str,
    dark: bool,
    backend: Option<Arc<dyn Backend>>,
    /// Stand-in remote images (no network in tests).
    painted: bool,
    /// Steps, each followed by letting the app's tasks finish.
    steps: Vec<Setup>,
}

fn step(f: impl FnOnce(&mut MailApp, &mut Window, &mut Context<MailApp>) + 'static) -> Setup {
    Box::new(f)
}

fn open_subject(subject: &'static str) -> Setup {
    step(move |app, window, cx| {
        let ix = app.threads.iter().position(|t| t.subject == subject).unwrap_or_else(|| panic!("no thread {subject:?}"));
        app.open_at(ix, window, cx);
    })
}

fn luma(px: &image::Rgba<u8>) -> f32 {
    0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32
}

fn take(shot: Shot) -> image::RgbaImage {
    let mut cx = HeadlessAppContext::with_platform(
        gpui_kit::platform::current_platform(true).text_system(),
        Arc::new(lumbridge_mail_app::assets()),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(lumbridge_mail_app::init);
    let backend = shot.backend.unwrap_or_else(|| Arc::new(Demo::new(NOW, DEFAULT_THREADS)));
    let mut opts = Options { now: Some(NOW), dark: shot.dark, undo_send_secs: 5, ..Default::default() };
    if shot.painted {
        opts.fetcher = Arc::new(lumbridge_mail_app::remote::Painted);
    }
    let (handle, app) = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds { origin: Default::default(), size: size(px(W), px(H)) })),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| MailApp::new(backend, opts, window, cx)),
            )
        })
        .expect("headless window");
    cx.run_until_parked();
    for s in shot.steps {
        cx.update_window(handle, |_, window, cx| {
            app.update(cx, |app, cx| s(app, window, cx));
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
        cx.run_until_parked();
    }
    // Two frames: some components settle (measure, then paint) on the second.
    for _ in 0..2 {
        cx.update_window(handle, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
        cx.run_until_parked();
    }
    let image = cx.capture_screenshot(handle).expect("offscreen rendering must be available");
    let path = out_dir().join(format!("{}.png", shot.name));
    image.save(&path).unwrap();
    println!("shot {} -> {} ({}x{})", shot.name, path.display(), image.width(), image.height());
    image
}

/// Sanity on every frame: not blank, the right appearance, the paper card where it should be.
fn check(name: &str, img: &image::RgbaImage, dark: bool) {
    let sx = img.width() as f32 / W;
    let at = |x: f32, y: f32| *img.get_pixel((x * sx) as u32, (y * sx) as u32);
    let mut colours = std::collections::HashSet::new();
    for y in (0..img.height()).step_by(7) {
        for x in (0..img.width()).step_by(11) {
            let p = img.get_pixel(x, y);
            colours.insert((p[0] / 8, p[1] / 8, p[2] / 8));
        }
    }
    assert!(colours.len() > 40, "{name}: frame looks blank ({} colours)", colours.len());
    if ["composer", "palette", "shortcuts"].iter().any(|o| name.contains(o)) {
        // Behind a backdrop: the sheet itself must be there, in the middle.
        let mid = luma(&at(W / 2., 300.));
        assert!(if dark { mid < 90. } else { mid > 150. }, "{name}: sheet colour wrong, luma {mid}");
        return;
    }
    // The sidebar's empty strip near its bottom edge is chrome.
    let chrome = luma(&at(120., 820.));
    if dark {
        assert!(chrome < 60., "{name}: sidebar should be dark, luma {chrome}");
    } else {
        assert!(chrome > 180., "{name}: sidebar should be light, luma {chrome}");
    }
}

fn main() {
    lumbridge_mail_app::time::use_fixed_offset(Some(0));
    let work_inbox = || View::Mailbox { account: AccountId::new("work"), mailbox: MailboxId::new("work:inbox") };
    let anon_inbox = || View::Mailbox { account: AccountId::new("anon"), mailbox: MailboxId::new("anon:inbox") };
    let shots = vec![
        Shot { name: "inbox-dark", dark: true, painted: false, backend: None,
               steps: vec![open_subject("Atlas launch: go/no-go on Thursday")] },
        Shot { name: "inbox-light", dark: false, painted: false, backend: None,
               steps: vec![open_subject("Atlas launch: go/no-go on Thursday")] },
        Shot { name: "reader-ai-dark", dark: true, painted: false, backend: None, steps: vec![
            step(|app, _, cx| app.toggle_ai_panel(cx)),
            open_subject("Book club: December pick"),
            step(|app, window, cx| {
                app.expand_all(cx);
                app.ask_input.update(cx, |s, cx| s.set_value("when is the launch and who is a go?", window, cx));
                app.ask(window, cx);
            }),
        ] },
        Shot { name: "reader-newsletter-blocked-light", dark: false, painted: false, backend: None,
               steps: vec![open_subject("The Weekly Byte #212: the quiet return of local-first")] },
        Shot { name: "reader-newsletter-loaded-dark", dark: true, painted: true, backend: None, steps: vec![
            open_subject("The Weekly Byte #212: the quiet return of local-first"),
            step(|app, _, cx| app.load_remote_in_open(cx)),
        ] },
        Shot { name: "reader-receipt-light", dark: false, painted: false, backend: None,
               steps: vec![open_subject("Your order #4471-2290 has shipped")] },
        Shot { name: "reader-attachments-dark", dark: true, painted: false, backend: None, steps: vec![
            open_subject("Your order #4471-2290 has shipped"),
            step(|app, _, cx| {
                // The invoice being copied from a daemon on another machine (45%), and a program that is
                // only offered for saving.
                let o = app.open.as_mut().unwrap();
                let m = o.messages.ready().unwrap().last().unwrap().clone();
                if let Some(lumbridge_mail_app::app::Load::Ready(b)) = o.bodies.get_mut(&m.id) {
                    let pdf = b.attachments[0].id.clone();
                    b.attachments.push(lumbridge_mail_core::Attachment { id: lumbridge_mail_core::AttachmentId::new("x9"),
                        filename: "parcelly-tracker-setup.exe".into(), mime: "application/x-msdownload".into(), size: 1_840_000, content_id: None });
                    app.files.insert((m.account.clone(), m.id.clone(), pdf),
                                     lumbridge_mail_app::app::FileState::Copying { done: 112_000, total: 248_000 });
                }
                cx.notify();
            }),
        ] },
        Shot { name: "reader-capabilities-local-light", dark: false, painted: false, backend: None, steps: vec![
            step(|app, _, cx| {
                // A Resend-like account: read state and moves only on this device, no archive.
                use lumbridge_mail_core::{Capabilities, Support};
                app.caps.insert(AccountId::new("personal"), Capabilities { seen: Support::Local, moves: Support::Local,
                    flagged: Support::Local, archive: Support::Unsupported, ..Capabilities::default() });
                cx.notify();
            }),
            open_subject("Your order #4471-2290 has shipped"),
        ] },
        Shot { name: "reader-capabilities-readonly-dark", dark: true, painted: false, backend: None, steps: vec![
            step(|app, _, cx| {
                if let Some(a) = app.accounts.iter_mut().find(|a| a.id.as_str() == "personal") {
                    a.readonly = true;
                }
                cx.notify();
            }),
            open_subject("Your order #4471-2290 has shipped"),
        ] },
        Shot { name: "daemon-menu-remote-paused-light", dark: false, painted: false, backend: None, steps: vec![step(|app, _, cx| {
            use lumbridge_mail_core::ProviderLoad;
            // The menu against a busy daemon on another machine (the demo cannot stop one either: no Quit),
            // made-up numbers: a Zoho sign-in and a gog host.
            app.daemon_menu = true;
            app.daemon = Some(Ok(lumbridge_mail_core::rpc::DaemonInfo {
                version: "0.1.0".into(),
                paused: true,
                load: vec![
                    ProviderLoad { scope: "zoho:zoho-tokens.json".into(), in_flight: 0, starts_last_min: 12, starts_total: 340,
                                   max_in_flight: 2, max_starts_per_min: 60, ..ProviderLoad::default() },
                    ProviderLoad { scope: "ssh:gog-host".into(), in_flight: 1, starts_last_min: 3, starts_total: 57,
                                   max_in_flight: 2, max_starts_per_min: 120, ..ProviderLoad::default() },
                ],
                kept_accounts: vec![],
            }));
            cx.notify();
        })] },
        Shot { name: "reader-reply-chain-folded-dark", dark: true, painted: false, backend: None,
               steps: vec![open_subject("RE: Q3 numbers for the board deck")] },
        Shot { name: "reader-reply-chain-open-light", dark: false, painted: false, backend: None, steps: vec![
            open_subject("RE: Q3 numbers for the board deck"),
            step(|app, _, cx| {
                let id = app.open.as_ref().unwrap().messages.ready().unwrap().last().unwrap().id.clone();
                app.toggle_quoted(&id, cx);
            }),
        ] },
        Shot { name: "reader-phishing-light", dark: false, painted: false, backend: None, steps: vec![
            open_subject("Action required: your account is on hold"),
            step(|app, _, cx| {
                // Hover the lying sign-in link: the foot of the card shows where it really goes.
                let id = app.open.as_ref().unwrap().messages.ready().unwrap().last().unwrap().id.clone();
                let link = app.open.as_ref().unwrap().rendered[&id].doc.blocks.iter().find_map(|b| match b {
                    lumbridge_mail_render::Block::Paragraph { spans } => spans.iter().find_map(|s| s.link.clone().filter(|l| l.warning.is_some())),
                    _ => None,
                });
                app.hover_link(link, cx);
            }),
        ] },
        Shot { name: "reader-inline-image-light", dark: false, painted: false, backend: None,
               steps: vec![open_subject("Floor plan for Quay Street")] },
        Shot { name: "composer-dark", dark: true, painted: false, backend: None, steps: vec![
            open_subject("Dinner Saturday?"),
            step(|app, window, cx| app.open_composer(ComposeMode::Reply, window, cx)),
            step(|app, window, cx| {
                if let Some(c) = app.composer.as_ref() {
                    c.ai_input.update(cx, |s, cx| s.set_value("say yes, Saturday works and I'll bring dessert", window, cx));
                }
                app.ai_draft(window, cx);
            }),
        ] },
        Shot { name: "composer-light", dark: false, painted: false, backend: None, steps: vec![
            step(|app, window, cx| app.open_composer(ComposeMode::New, window, cx)),
            step(|app, _, _| {
                if let Some(c) = app.composer.as_mut() {
                    c.show_cc_bcc = true;
                    c.to.push(lumbridge_mail_core::Address::new(Some("Maya Chen"), "maya.chen@postbox.example"));
                    c.to.push(lumbridge_mail_core::Address::new(None, "jonah@parkfamily.example"));
                    c.add_attachment(std::path::Path::new("/tmp/hike-route.pdf"));
                    c.from_open = true;
                }
            }),
        ] },
        Shot { name: "composer-reply-quoted-light", dark: false, painted: false, backend: None, steps: vec![
            open_subject("Dinner Saturday?"),
            step(|app, window, cx| app.open_composer(ComposeMode::Reply, window, cx)),
            step(|app, window, cx| {
                if let Some(c) = app.composer.as_mut() {
                    c.body.update(cx, |s, cx| s.set_value("Saturday at 7:30 is **perfect**. I'll bring dessert:\n\n- lemon tart\n- or the _famous_ brownies", window, cx));
                    c.show_original = true;
                }
            }),
        ] },
        Shot { name: "composer-suggestions-dark", dark: true, painted: false, backend: None, steps: vec![
            step(|app, window, cx| app.open_composer(ComposeMode::New, window, cx)),
            step(|app, window, cx| {
                if let Some(c) = app.composer.as_mut() {
                    c.suggest_field = Some(lumbridge_mail_app::ui::composer::Field::To);
                    c.to_input.update(cx, |s, cx| s.set_value("ma", window, cx));
                }
            }),
        ] },
        Shot { name: "palette-dark", dark: true, painted: false, backend: None,
               steps: vec![step(|app, window, cx| app.toggle_palette(window, cx))] },
        Shot { name: "shortcuts-light", dark: false, painted: false, backend: None, steps: vec![step(|app, _, cx| {
            app.shortcuts = true;
            cx.notify();
        })] },
        Shot { name: "work-select-dark", dark: true, painted: false, backend: None, steps: vec![
            step(move |app, window, cx| app.set_view(work_inbox(), window, cx)),
            step(|app, _, cx| {
                for t in app.threads.iter().take(3) {
                    app.selected.insert(lumbridge_mail_app::app::key(t));
                }
                app.cursor = Some(2);
                cx.notify();
            }),
        ] },
        Shot { name: "anon-sensitive-dark", dark: true, painted: false, backend: None, steps: vec![
            step(move |app, window, cx| app.set_view(anon_inbox(), window, cx)),
            step(|app, window, cx| {
                app.set_view(View::Unified(lumbridge_mail_core::MailboxRole::Inbox), window, cx);
            }),
            open_subject("Your sign-in code"),
            step(|app, _, cx| app.toggle_ai_panel(cx)),
        ] },
        Shot { name: "search-light", dark: false, painted: false, backend: None, steps: vec![step(|app, window, cx| {
            app.search.update(cx, |s, cx| s.set_value("from:dana has:attachment", window, cx));
            app.run_search(window, cx);
            app.search.update(cx, |s, cx| s.focus(window, cx));
        })] },
        Shot { name: "empty-dark", dark: true, painted: false, backend: None, steps: vec![step(|app, window, cx| {
            app.search.update(cx, |s, cx| s.set_value("subject:zeppelin", window, cx));
            app.run_search(window, cx);
        })] },
        Shot { name: "no-daemon-light", dark: false, painted: false,
               backend: Some(Arc::new(Rpc::new(std::env::temp_dir().join("lbmail-no-such-dir/maild.sock")))),
               // The link manager answers from its own thread: give it a moment, then settle.
               steps: vec![step(|_, _, _| std::thread::sleep(std::time::Duration::from_millis(400))),
                           step(|_, _, _| std::thread::sleep(std::time::Duration::from_millis(100)))] },
    ];
    let only = std::env::args().nth(1).filter(|a| !a.starts_with('-'));
    for shot in shots {
        if only.as_deref().is_some_and(|o| !shot.name.contains(o)) {
            continue;
        }
        let (name, dark) = (shot.name, shot.dark);
        let img = take(shot);
        check(name, &img, dark);
    }
    println!("screenshots: ok");
}
