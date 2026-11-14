//! The harness for driving the real app against the real daemon, headlessly.
//!
//! - [`MailEnv`]: `lumbridge-maild` (the library, exactly what the binary runs) on a temp
//!   `LUMBRIDGE_MAIL_HOME`, with made-up mail (the daemon's `DemoProvider`s, held here so tests can look
//!   at what reached "the server") and a `ScriptedModel`. Each start gets its own tokio runtime, so
//!   [`MailEnv::kill`] can drop it mid-flight like a crash: connections close, the socket file stays.
//! - [`Window`]: the app (`MailApp`) on GPUI's headless test platform, its backend the app's real
//!   `Rpc` client pointed at that socket, driven by keystrokes like `lumbridge-mail-app/tests/behaviour.rs`.
//!
//! Made-up mail only (CONTRIBUTING.md): every address is `.example` or a well-known service's no-reply.

extern crate gpui_kit as gpui;

use gpui::*;
use lumbridge_mail_ai::ScriptedModel;
use lumbridge_mail_app::backend::Backend;
use lumbridge_mail_app::notify::Recorder;
use lumbridge_mail_app::{MailApp, Options};
use lumbridge_mail_core::paths::{HOME_VAR, Paths};
use lumbridge_mail_core::rpc::{AccountStatus, method};
use lumbridge_mail_core::{AccountId, Address, Provider, Query, Result};
use lumbridge_maild::{Client, Daemon, DemoProvider, DemoSet};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The app's clock in these tests: two hours after the demo mail's newest date.
pub const NOW: i64 = lumbridge_maild::demo::BASE_TIME + 2 * 3600;

/// Four made-up accounts: personal, work, anon, and a read-only one (personal persona).
pub const CONFIG: &str = r##"
[daemon]
sync_interval_s = 5
first_retry_s = 1
max_retry_s = 2
max_backoff_s = 2

[[accounts]]
id = "personal"
persona = "personal"
label = "Personal"
email = "sam@example.com"
name = "Sam Example"
color = "#3b82f6"
[accounts.provider]
kind = "demo"

[[accounts]]
id = "work"
persona = "work"
label = "Work"
email = "sam@work.example"
name = "Sam Example"
color = "#10b981"
[accounts.provider]
kind = "demo"

[[accounts]]
id = "anon"
persona = "anon"
label = "Forum"
email = "otter@anon.example"
name = "Otter"
[accounts.provider]
kind = "demo"

[[accounts]]
id = "ro"
persona = "personal"
label = "Archive box"
email = "sam@readonly.example"
name = "Sam Example"
color = "#f59e0b"
readonly = true
[accounts.provider]
kind = "demo"
set = "work"
"##;

/// The scripted model's answers, by task (the prompts are the ai crate's).
pub fn answer(req: &lumbridge_mail_ai::ChatRequest) -> Result<String> {
    let user = req.messages.iter().map(|m| m.content.as_str()).collect::<Vec<_>>().join("\n");
    Ok(match req.task {
        "triage" if user.contains("Dinner") => json!({"category": "important", "priority": 80, "summary": "Maya asks you to book a table for four", "needs_reply": true, "due": null}),
        "triage" if user.contains("Q4 planning") => json!({"category": "important", "priority": 85, "summary": "Priya needs your three Q4 priorities by Thursday", "needs_reply": true, "due": null}),
        "triage" => json!({"category": "personal", "priority": 50, "summary": "A note from someone you know", "needs_reply": false, "due": null}),
        "summary" => json!({"summary": "Maya and Sam settle dinner on Saturday at 7:30; Maya asks for a table for four."}),
        "draft" => json!({"body": "Hi Maya,\n\nDone, I booked a table for four at 7:30.\n\nSam"}),
        "extract" => json!({"items": [{"text": "Book a table for four", "owner": "me", "due": null}], "dates": []}),
        "ask_plan" => json!({"queries": ["dinner"]}),
        "ask_answer" => json!({"answer": "Dinner is on Saturday at 7:30.", "sources": ["t1"]}),
        _ => json!({}),
    }
    .to_string())
}

/// A stand-in keyring (no test ever writes to a real one).
#[derive(Default)]
pub struct FakeKeyring(pub std::sync::Mutex<Vec<(String, String)>>);

impl lumbridge_maild::secrets::SecretStore for FakeKeyring {
    fn set(&self, account: &str, secret: &str) -> Result<()> {
        self.0.lock().unwrap().push((account.into(), secret.into()));
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<()> {
        self.0.lock().unwrap().retain(|(a, _)| a != account);
        Ok(())
    }
}

pub struct MailEnv {
    _dir: tempfile::TempDir,
    pub paths: Paths,
    /// The made-up "servers", by account id. They outlive daemon restarts, like a real server.
    pub demos: HashMap<String, Arc<DemoProvider>>,
    pub model: Arc<ScriptedModel>,
    pub keyring: Arc<FakeKeyring>,
    rt: Option<tokio::runtime::Runtime>,
    daemon: Option<Daemon>,
}

fn me(name: &str, email: &str) -> Address {
    Address::new(Some(name), email)
}

impl MailEnv {
    /// The four accounts of [`CONFIG`].
    pub fn new() -> MailEnv {
        let env = Self::bare(Some(CONFIG));
        env.with_demo("personal", DemoSet::Personal, me("Sam Example", "sam@example.com"))
            .with_demo("work", DemoSet::Work, me("Sam Example", "sam@work.example"))
            .with_demo("anon", DemoSet::Anon, me("Otter", "otter@anon.example"))
            .with_demo("ro", DemoSet::Work, me("Sam Example", "sam@readonly.example"))
    }

    /// No config at all (first run). The gog account added as `sam@gmail.example` and the IMAP one added as
    /// `sam@fastmail.com` are served by made-up mail (no gog runs, no server is dialled); "Try with demo
    /// mail" uses the daemon's own demo provider from the config.
    pub fn first_run() -> MailEnv {
        Self::bare(None)
            .with_demo("sam-gmail", DemoSet::Personal, me("Sam Example", "sam@gmail.example"))
            // Never a real IMAP server: the account the IMAP form adds is served by made-up mail too.
            .with_demo("sam-fastmail", DemoSet::Work, me("Sam Example", "sam@fastmail.com"))
    }

    fn bare(config: Option<&str>) -> MailEnv {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = dir.path().to_path_buf();
        let paths = Paths::resolve(|k| (k == HOME_VAR).then(|| home.clone().into_os_string()));
        if let Some(c) = config {
            std::fs::create_dir_all(&paths.config_dir).unwrap();
            std::fs::write(paths.config_file(), c).unwrap();
        }
        MailEnv {
            _dir: dir,
            paths,
            demos: HashMap::new(),
            model: Arc::new(ScriptedModel::with(answer)),
            keyring: Arc::default(),
            rt: None,
            daemon: None,
        }
    }

    fn with_demo(mut self, id: &str, set: DemoSet, me: Address) -> Self {
        self.demos.insert(id.into(), Arc::new(DemoProvider::new(AccountId::new(id), set, me)));
        self
    }

    pub fn demo(&self, id: &str) -> Arc<DemoProvider> {
        self.demos[id].clone()
    }

    pub fn socket(&self) -> PathBuf {
        self.paths.socket()
    }

    /// Start the daemon (again). A new runtime each time, a new person token each time.
    pub fn start(&mut self) {
        assert!(self.daemon.is_none(), "already running");
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let mut o = lumbridge_maild::Options::new(self.paths.clone());
        for (id, p) in &self.demos {
            o.providers.insert(AccountId::new(id), p.clone() as Arc<dyn Provider>);
        }
        o.model = Some(self.model.clone());
        o.secrets = Some(self.keyring.clone());
        let d = rt.block_on(Daemon::start(o)).expect("daemon starts");
        self.daemon = Some(d);
        self.rt = Some(rt);
    }

    /// Kill the daemon like a crash: its runtime is dropped with every task in it (connections close,
    /// nothing is shut down cleanly; the socket file is left behind for the next start to replace).
    pub fn kill(&mut self) {
        let d = self.daemon.take();
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
        drop(d);
    }

    pub fn block_on<F: std::future::Future>(&self, f: F) -> F::Output {
        self.rt.as_ref().expect("daemon running").block_on(f)
    }

    /// A protocol client (what `lbmail` uses), as a person or as an agent.
    pub fn client(&self, person: bool) -> Client {
        let token = person.then(|| Client::read_person_token(&self.paths.person_token()).expect("token file"));
        self.block_on(Client::connect(&self.socket(), token, "e2e")).expect("connect")
    }

    pub fn status(&self) -> Vec<AccountStatus> {
        let mut c = self.client(true);
        self.block_on(c.call(method::STATUS, json!({}))).unwrap()
    }

    /// Every configured account has synced and every thread has its triage (rules or the model).
    pub fn wait_synced(&self, accounts: usize) {
        let mut c = self.client(true);
        wait_real("every account synced and triaged", Duration::from_secs(40), || {
            let st: Vec<AccountStatus> = self.block_on(c.call(method::STATUS, json!({}))).unwrap();
            if st.len() != accounts || !st.iter().all(|s| s.last_sync.is_some()) {
                return false;
            }
            st.iter().all(|s| {
                let q = Query { accounts: Some(vec![s.account.clone()]), limit: 500, ..Query::default() };
                let page: lumbridge_mail_core::rpc::ThreadPage = self.block_on(c.call(method::THREADS, &q)).unwrap();
                !page.threads.is_empty() && page.threads.iter().all(|t| t.triage.is_some())
            })
        });
    }
}

impl Default for MailEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MailEnv {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Poll `cond` in real time until it holds.
pub fn wait_real(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < limit, "timed out after {limit:?} waiting for: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The app window on GPUI's headless test platform. With `shots`, a real offscreen renderer too.
pub struct Window {
    pub cx: HeadlessAppContext,
    pub window: AnyWindowHandle,
    pub app: Entity<MailApp>,
    pub notes: Arc<Recorder>,
}

pub const W: f32 = 1440.;
pub const H: f32 = 900.;

impl Window {
    pub fn open(backend: Arc<dyn Backend>) -> Window {
        Self::open_with(backend, false, true)
    }

    pub fn open_with(backend: Arc<dyn Backend>, shots: bool, dark: bool) -> Window {
        let text = gpui_kit::platform::current_platform(true).text_system();
        let assets = Arc::new(lumbridge_mail_app::assets());
        let mut cx = if shots {
            HeadlessAppContext::with_platform(text, assets, gpui_kit::platform::current_headless_renderer)
        } else {
            HeadlessAppContext::with_platform(text, assets, || Ok(None))
        };
        cx.update(lumbridge_mail_app::init);
        cx.update(lumbridge_mail_app::keymap::bind_app_keys);
        let notes = Arc::new(Recorder::default());
        let opts = Options { now: Some(NOW), dark, undo_send_secs: 3, notifier: notes.clone(),
                             fetcher: Arc::new(lumbridge_mail_app::remote::Painted), ..Options::default() };
        let (window, app) = cx
            .update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds { origin: Default::default(), size: size(px(W), px(H)) })),
                        focus: !shots,
                        show: !shots,
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| MailApp::new(backend, opts, window, cx)),
                )
            })
            .expect("headless window");
        let mut w = Window { cx, window, app, notes };
        w.settle();
        w
    }

    /// Let the app's tasks run and draw a frame.
    pub fn settle(&mut self) {
        self.cx.run_until_parked();
        let _ = self.cx.update_window(self.window, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        });
        self.cx.run_until_parked();
    }

    pub fn keys(&mut self, keys: &'static str) {
        for k in keys.split(' ') {
            let stroke = Keystroke::parse(k).unwrap();
            self.cx.update_window(self.window, |_, window, cx| {
                window.dispatch_keystroke(stroke, cx);
            })
            .unwrap();
            self.settle();
        }
    }

    pub fn read<R>(&mut self, f: impl FnOnce(&MailApp, &App) -> R) -> R {
        let app = self.app.clone();
        self.cx.update(|cx| f(app.read(cx), cx))
    }

    pub fn update<R>(&mut self, f: impl FnOnce(&mut MailApp, &mut gpui::Window, &mut Context<MailApp>) -> R) -> R {
        let app = self.app.clone();
        let r = self.cx.update_window(self.window, |_, window, cx| app.update(cx, |a, cx| f(a, window, cx))).unwrap();
        self.settle();
        r
    }

    /// The daemon answers on other threads: settle, look, sleep a little, until `cond` holds (20 s).
    pub fn wait(&mut self, what: &str, cond: impl Fn(&MailApp, &App) -> bool) {
        let start = Instant::now();
        loop {
            self.settle();
            if self.read(|a, cx| cond(a, cx)) {
                return;
            }
            if start.elapsed() > Duration::from_secs(20) {
                let state = self.read(|a, _| format!("link {:?}, list {:?}, {} threads, toast {:?}, status {:?}",
                                                     a.link, a.list, a.threads.len(), a.toast.as_ref().map(|t| t.text.clone()), a.status));
                panic!("timed out waiting for: {what}\n  app: {state}");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Move the cursor to the thread with this subject in this account, by keys (`j`/`k`).
    pub fn cursor_to(&mut self, account: &str, subject: &str) -> usize {
        let ix = self.read(|a, _| a.threads.iter().position(|t| t.account.as_str() == account && t.subject == subject))
            .unwrap_or_else(|| panic!("no thread {account}: {subject:?} in the list"));
        let cur = self.read(|a, _| a.cursor);
        let (from, key) = match cur {
            None => {
                self.keys("j");
                (0, "j")
            }
            Some(c) if c <= ix => (c, "j"),
            Some(c) => (c, "k"),
        };
        let steps = from.abs_diff(ix);
        for _ in 0..steps {
            self.keys(key);
        }
        assert_eq!(self.read(|a, _| a.cursor), Some(ix));
        ix
    }

    /// Advance the app's (test) clock, a second at a time.
    pub fn advance(&mut self, secs: u64) {
        for _ in 0..secs {
            self.cx.advance_clock(Duration::from_secs(1));
            self.settle();
        }
    }

    /// A real offscreen frame (only for windows opened with `shots`).
    pub fn screenshot(&mut self, name: &str) -> image::RgbaImage {
        for _ in 0..2 {
            self.settle();
        }
        let img = self.cx.capture_screenshot(self.window).expect("offscreen rendering must be available");
        let dir = std::env::var_os("LBMAIL_SHOTS")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.png"));
        img.save(&path).unwrap();
        println!("shot {name} -> {} ({}x{})", path.display(), img.width(), img.height());
        img
    }
}

/// The pid of the process serving a Unix socket (Linux: `SO_PEERCRED`).
#[cfg(target_os = "linux")]
pub fn socket_peer_pid(path: &std::path::Path) -> Option<i32> {
    use std::os::fd::AsRawFd;
    let s = std::os::unix::net::UnixStream::connect(path).ok()?;
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: valid out-pointers of the sizes given; the fd is an open socket.
    let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &mut len) };
    (r == 0).then_some(cred.pid)
}

/// The pid of the process serving a Unix socket (macOS: `LOCAL_PEERPID`).
#[cfg(target_os = "macos")]
pub fn socket_peer_pid(path: &std::path::Path) -> Option<i32> {
    use std::os::fd::AsRawFd;
    let s = std::os::unix::net::UnixStream::connect(path).ok()?;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: valid out-pointers of the sizes given; the fd is an open socket.
    let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_LOCAL, libc::LOCAL_PEERPID, (&raw mut pid).cast(), &mut len) };
    (r == 0).then_some(pid)
}
