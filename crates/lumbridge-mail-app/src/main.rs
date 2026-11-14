//! `lumbridge-mail`: the desktop app. See `cli::USAGE` for the arguments.
//!
//!     lumbridge-mail --demo                 made-up mail, no daemon
//!     lumbridge-mail                        the daemon (started if it is not running)
//!     lumbridge-mail mailto:a@b.example     open the composer (hands over to a running window)
//!     lumbridge-mail --remote homeserver    the daemon on homeserver, over ssh (never started from here)

use gpui_kit::AppContext;
use lumbridge_mail_app::backend::demo::{Demo, DEFAULT_THREADS};
use lumbridge_mail_app::backend::rpc::{Autostart, Rpc};
use lumbridge_mail_app::backend::ssh::{self, Target};
use lumbridge_mail_app::backend::Backend;
use lumbridge_mail_app::cli::{self, Parsed};
use lumbridge_mail_app::instance::{self, Acquired, Request};
use lumbridge_mail_app::settings::{Settings, Theme};
use lumbridge_mail_app::{mailto, notify, paths, remote, MailApp, Options};
use std::sync::Arc;

fn main() {
    let args = match cli::parse(std::env::args().skip(1), DEFAULT_THREADS) {
        Ok(Parsed::Run(a)) => a,
        Ok(Parsed::Help) => {
            println!("{}", cli::USAGE);
            return;
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    // Which daemon, before anything else: a bad --remote or config should not open a window.
    let target = match ssh::target(&args) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("lumbridge-mail: {e}");
            std::process::exit(2);
        }
    };

    // One window per user: a second launch hands its mailto (or "come to the front") to the first.
    let request = match &args.mailto {
        Some(url) => Request::Open(url.clone()),
        None => Request::Activate,
    };
    let mut primary = None;
    if !args.new_instance {
        match instance::acquire(&paths::instance_socket(), &request) {
            Ok(Acquired::Handed) => return,
            Ok(Acquired::Primary(p)) => primary = Some(p),
            Err(e) => eprintln!("lumbridge-mail: no single-instance guard ({e}); carrying on"),
        }
    }
    let (req_tx, req_rx) = async_channel::unbounded::<Request>();
    if let Some(p) = primary.as_mut() {
        let from_others = p.serve();
        let tx = req_tx.clone();
        std::thread::spawn(move || {
            while let Ok(r) = from_others.recv_blocking() {
                if tx.send_blocking(r).is_err() {
                    break;
                }
            }
        });
    }

    let config_dir = paths::config_dir();
    let settings = Settings::load(&config_dir);
    let backend: Arc<dyn Backend> = match target {
        Target::Demo => Arc::new(Demo::new(lumbridge_mail_app::time::now(), args.threads)),
        // A socket named on the command line belongs to someone else's daemon: never start one there.
        Target::Local { socket, autostart: false } => Arc::new(Rpc::new(socket)),
        Target::Local { socket, autostart: true } => Arc::new(Rpc::with_autostart(socket, Autostart::Find)),
        // The server's systemd unit owns that daemon: nothing is started or stopped from here.
        Target::Remote(remote) => Arc::new(Rpc::remote(remote)),
    };
    let dark = args.dark.unwrap_or(settings.theme == Theme::Dark);
    let opts = Options {
        now: None,
        dark,
        undo_send_secs: args.undo_send,
        settings,
        // The demo does not write the real settings file.
        config_dir: (!args.demo).then_some(config_dir),
        fetcher: Arc::new(remote::Http::new()),
        notifier: Arc::new(notify::System),
        requests: Some(req_rx),
        mailto: args.mailto.as_deref().and_then(mailto::parse),
        opener: Arc::new(lumbridge_mail_app::files::System),
        downloads: None,
    };

    let app = gpui_kit::application().with_assets(lumbridge_mail_app::assets());
    // macOS hands `mailto:` links to a running app as an open-URL event, not as an argument.
    let url_tx = req_tx.clone();
    app.on_open_urls(move |urls| {
        for url in urls {
            let _ = url_tx.send_blocking(Request::Open(url));
        }
    });
    // The guard lives as long as the process (a socket file left by a crash is cleaned up by the next
    // launch, see `instance::acquire`).
    std::mem::forget(primary);
    app.run(move |cx| {
        lumbridge_mail_app::init(cx);
        lumbridge_mail_app::keymap::bind_app_keys(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let options = lumbridge_mail_app::window_options(cx);
        if let Err(e) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| MailApp::new(backend, opts, window, cx))
        }) {
            eprintln!("lumbridge-mail: could not open a window: {e}");
            cx.quit();
        }
        // On macOS, Launch Services already brings a normally launched app forward (Dock, Finder, a
        // mailto link), and `open -g` asks for a background launch: forcing activation here would
        // steal focus from whatever the person is doing. Elsewhere nobody else raises the window.
        if !cfg!(target_os = "macos") {
            cx.activate(true);
        }
    });
}
