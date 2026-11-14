//! The window's state and everything it does. Drawing lives in `ui/*`, one file per area, as further
//! `impl MailApp` blocks; this file loads data, reacts to keys and talks to the [`Backend`].

use crate::backend::{Backend, LinkState};
use crate::backend::Progress;
use crate::contacts::Contacts;
use crate::files::{self, Opener};
use crate::instance::Request;
use crate::keymap::*;
use crate::mailto::Mailto;
use crate::notify::{self, Notifier};
use crate::reading::{self, ImageKey, Rendered};
use crate::remote::{self, Fetcher, Picture};
use crate::settings::{Density, Settings, Theme};
use crate::theme::{self, Palette};
use crate::ui::composer::{Composer, ComposeMode};
use gpui::prelude::*;
use gpui::*;
use gpui_kit::component::command::CommandState;
use gpui_kit::component::input::{InputEvent, InputState};
use lumbridge_mail_core::rpc::{AccountStatus, Act, AttachmentRef, Caller, CategoryCount, CountsRequest, Event, ThreadRef};
use lumbridge_mail_render::Link;
use lumbridge_mail_core::{safety, Account, AccountId, Action, Address, AiPolicy, Attachment, AttachmentId, Body, Category, Draft, Error,
                          Mailbox,
                          Extracted, MailboxId, MailboxRole, MessageId, MessageMeta, Persona, Query, ThreadId, ThreadSummary};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Threads asked for per page.
pub const PAGE: u32 = 200;
/// Load the next page when the list is this close to the end.
const PREFETCH: usize = 40;

#[derive(Clone)]
pub struct Options {
    /// Pin the clock (screenshots, tests). None = the real time.
    pub now: Option<i64>,
    pub dark: bool,
    /// Seconds a sent message waits before it really goes, so it can be undone.
    pub undo_send_secs: u64,
    /// The person's choices (theme, density, notifications, image senders).
    pub settings: Settings,
    /// Where `settings` are saved when they change. None = never saved (tests, screenshots).
    pub config_dir: Option<PathBuf>,
    /// Loads remote images once the person asked for them.
    pub fetcher: Arc<dyn Fetcher>,
    /// Desktop notifications.
    pub notifier: Arc<dyn Notifier>,
    /// Requests from a second instance (and macOS open-URL events).
    pub requests: Option<async_channel::Receiver<Request>>,
    /// Open the composer with this as soon as the accounts are in (`lumbridge-mail mailto:...`).
    pub mailto: Option<Mailto>,
    /// Opens attachments with the system's programs.
    pub opener: Arc<dyn Opener>,
    /// Where "Save" puts attachments. None = `files::downloads_dir()`.
    pub downloads: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            now: None,
            dark: true,
            undo_send_secs: 5,
            settings: Settings::default(),
            config_dir: None,
            fetcher: Arc::new(remote::Http::new()),
            notifier: Arc::new(notify::Recorder::default()),
            requests: None,
            mailto: None,
            opener: Arc::new(files::System),
            downloads: None,
        }
    }
}

/// A change the window offers on a thread (toolbar, selection bar, keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Archive,
    Trash,
    Star,
    Read,
    Reply,
}

impl Verb {
    fn name(self) -> &'static str {
        match self {
            Verb::Archive => "archive",
            Verb::Trash => "trash",
            Verb::Star => "star",
            Verb::Read => "read state",
            Verb::Reply => "reply",
        }
    }
}

/// How the window offers a [`Verb`] on an account, from `accounts.capabilities` and `Account::readonly`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer {
    /// On the server, like any mail account.
    Server,
    /// Only in the local store ("on this device only").
    Local,
    /// Shown but not clickable, with the reason (a read-only account).
    Disabled(String),
    /// Not offered at all (the account cannot do it).
    Hidden,
}

/// One attachment of one message.
pub type FileKey = (AccountId, MessageId, AttachmentId);

/// Where an attachment is on its way to being opened or saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileState {
    /// Being fetched (or copied from a daemon on another machine): bytes so far, bytes in all.
    Copying { done: u64, total: u64 },
    /// On this machine at this path.
    Ready(PathBuf),
    Failed(String),
}

/// What the person asked to do with an attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    Open,
    Save,
}

/// A thread's identity across accounts. Core ids are unique only within an account (the same mail
/// cc'd to two of your addresses, or two accounts on one server, can share a thread id), so the window
/// never matches threads by `ThreadId` alone.
pub type Key = (AccountId, ThreadId);

pub fn key(t: &ThreadSummary) -> Key {
    (t.account.clone(), t.id.clone())
}

/// What the sidebar has selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    /// Every non-anon account, one mailbox role (the unified inbox is `Unified(Inbox)`).
    Unified(MailboxRole),
    /// An AI category across the non-anon accounts.
    Category(Category),
    Mailbox { account: AccountId, mailbox: MailboxId },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Load::Ready(t) => Some(t),
            _ => None,
        }
    }
}

pub struct OpenThread {
    pub summary: ThreadSummary,
    pub messages: Load<Vec<MessageMeta>>,
    pub bodies: HashMap<MessageId, Load<Body>>,
    pub expanded: HashSet<MessageId>,
    pub show_quoted: HashSet<MessageId>,
    /// "Show 3 earlier messages" was pressed.
    pub show_middle: bool,
    /// Messages whose remote images the person loaded (this message only).
    pub remote_ok: HashSet<MessageId>,
    /// An anon account's message waiting for "Load anyway".
    pub confirm_remote: Option<MessageId>,
    /// Bodies through the renderer, by message.
    pub rendered: HashMap<MessageId, Rendered>,
}

impl OpenThread {
    pub fn tref(&self) -> ThreadRef {
        ThreadRef { account: self.summary.account.clone(), thread: self.summary.id.clone() }
    }
}

pub struct ChatLine {
    pub mine: bool,
    pub text: String,
    pub sources: Vec<(ThreadRef, String)>,
    pub failed: bool,
}

pub struct AiState {
    pub thread: Option<Key>,
    pub summary: Load<String>,
    /// `ai.extract`: action items (with who owes them) and dates.
    pub items: Load<Extracted>,
    pub chat: Vec<ChatLine>,
    pub asking: bool,
}

/// A message handed to the daemon's queue, waiting to go out. Kept so a failure can reopen it.
pub struct Sending {
    pub account: AccountId,
    pub local_id: String,
    pub subject: String,
    pub composer: Composer,
}

pub struct PendingSend {
    pub account: AccountId,
    pub draft: Draft,
    pub composer: Composer,
    pub remaining: u64,
    _task: Task<()>,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub error: bool,
    pub id: u64,
}

/// A change that `z` can take back.
#[derive(Debug, Clone)]
struct UndoStep {
    label: String,
    acts: Vec<(AccountId, Action)>,
}

pub struct MailApp {
    pub backend: Arc<dyn Backend>,
    pub opts: Options,
    pub pal: Palette,
    pub now: i64,
    pub focus: FocusHandle,

    pub hello: Load<String>,
    pub accounts: Vec<Account>,
    pub mailboxes: Vec<Mailbox>,
    pub status: Vec<AccountStatus>,
    /// Threads and unread threads per AI category (`counts`, unified inbox scope), for the sidebar.
    pub counts: Vec<CategoryCount>,
    /// Sent messages the daemon has queued but not yet sent.
    pub sending: Vec<Sending>,
    /// `accounts.list` answered (so an empty list means "no accounts", not "not loaded").
    pub accounts_loaded: bool,
    /// The first-run screen (no accounts yet): what the person has filled in.
    pub setup: crate::ui::setup::Setup,
    pub expanded_accounts: HashSet<AccountId>,

    pub view: View,
    pub search: Entity<InputState>,
    pub search_text: Option<String>,
    pub search_focused: bool,

    pub threads: Vec<ThreadSummary>,
    pub next_page: Option<String>,
    pub list: Load<()>,
    pub loading_more: bool,
    list_gen: u64,
    pub list_scroll: UniformListScrollHandle,
    pub cursor: Option<usize>,
    pub selected: BTreeSet<Key>,

    pub open: Option<OpenThread>,
    open_gen: u64,
    pub reader_scroll: ScrollHandle,

    pub ai_panel: bool,
    pub ai: AiState,
    pub ask_input: Entity<InputState>,

    pub composer: Option<Composer>,
    pub pending_send: Option<PendingSend>,
    pub palette: Option<Entity<CommandState>>,
    pub shortcuts: bool,
    pub toast: Option<Toast>,
    toast_counter: u64,
    undo: Option<UndoStep>,
    /// Meta of threads we fetched to act on them, so a second action needs no round trip.
    meta_cache: HashMap<Key, Vec<MessageId>>,

    pub settings: Settings,
    config_dir: Option<PathBuf>,
    pub fetcher: Arc<dyn Fetcher>,
    pub notifier: Arc<dyn Notifier>,
    /// Pictures for the reader (remote, inline parts, data: URIs), shared across threads.
    pub images: HashMap<ImageKey, Load<Picture>>,
    /// The daemon menu (click the sync state in the status bar) is open.
    pub daemon_menu: bool,
    /// What `daemon.info` said last (paused, provider load), for that menu.
    pub daemon: Option<Result<lumbridge_mail_core::rpc::DaemonInfo, String>>,
    /// What each account can do and where (`accounts.capabilities`); missing = everything on the server.
    pub caps: HashMap<AccountId, lumbridge_mail_core::Capabilities>,
    /// Attachments being fetched, or on this machine, for Open and Save.
    pub files: HashMap<FileKey, FileState>,
    /// The last attachment saved to Downloads (tests; "show in folder").
    pub last_saved: Option<PathBuf>,
    pub contacts: Contacts,
    /// The daemon link, for backends that have one.
    pub link: Option<LinkState>,
    /// Lost the daemon (or never had it): reload when it is back.
    link_trouble: bool,
    pub window_active: bool,
    pub mono_font: SharedString,
    /// The link under the mouse in the reader.
    pub hovered_link: Option<Link>,
    /// A deceptive link waiting for "Open anyway".
    pub confirm_link: Option<Link>,
    /// A `mailto:` to open in the composer at the next frame (with a window to build it in).
    pub pending_mailto: Option<Mailto>,
    /// Bring the window to the front at the next frame.
    pub activate_requested: bool,
    requests: Option<async_channel::Receiver<Request>>,
    _tasks: Vec<Task<()>>,
    _subs: Vec<Subscription>,
}

/// The first of these fonts that is installed, for code (falls back to whatever GPUI picks).
const MONO_FONTS: &[&str] = &["JetBrains Mono", "SF Mono", "Menlo", "DejaVu Sans Mono", "Liberation Mono", "Noto Sans Mono", "monospace"];

pub fn view_title(view: &View, mailboxes: &[Mailbox], accounts: &[Account]) -> String {
    match view {
        View::Unified(role) => match role {
            MailboxRole::Inbox => "Unified Inbox".into(),
            MailboxRole::Important => "Important".into(),
            MailboxRole::Starred => "Starred".into(),
            MailboxRole::Sent => "Sent".into(),
            MailboxRole::Drafts => "Drafts".into(),
            MailboxRole::Archive => "Archive".into(),
            MailboxRole::Trash => "Trash".into(),
            MailboxRole::Spam => "Spam".into(),
            _ => "All mail".into(),
        },
        View::Category(c) => theme::category_label(*c).into(),
        View::Mailbox { account, mailbox } => {
            let acct = accounts.iter().find(|a| &a.id == account).map(|a| a.label.clone()).unwrap_or_default();
            let mb = mailboxes.iter().find(|m| &m.id == mailbox).map(|m| m.name.clone()).unwrap_or_default();
            format!("{acct} · {mb}")
        }
    }
}

impl MailApp {
    pub fn new(backend: Arc<dyn Backend>, opts: Options, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let now = opts.now.unwrap_or_else(crate::time::now);
        theme::apply(opts.dark, cx);
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search mail"));
        let ask_input = cx.new(|cx| InputState::new(window, cx).placeholder("Ask your mail…"));
        let mut subs = Vec::new();
        subs.push(cx.subscribe_in(&search, window, |this: &mut Self, _, ev: &InputEvent, window, cx| match ev {
            InputEvent::PressEnter { .. } => this.run_search(window, cx),
            InputEvent::Focus => {
                this.search_focused = true;
                cx.notify();
            }
            InputEvent::Blur => {
                this.search_focused = false;
                cx.notify();
            }
            InputEvent::Change => {
                // Clearing the field (the clear button, or deleting it all) goes back to the mailbox.
                if this.search.read(cx).value().trim().is_empty() && this.search_text.is_some() {
                    this.search_text = None;
                    this.load_threads(false, cx);
                }
            }
        }));
        subs.push(cx.subscribe_in(&ask_input, window, |this: &mut Self, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.ask(window, cx);
            }
        }));
        let focus = cx.focus_handle();
        subs.push(cx.observe_window_activation(window, |this: &mut Self, window, cx| {
            this.window_active = window.is_window_active();
            cx.notify();
        }));
        let mono_font: SharedString = {
            let installed = cx.text_system().all_font_names();
            MONO_FONTS.iter().find(|f| installed.iter().any(|n| n == *f)).copied().unwrap_or("monospace").into()
        };
        let settings = opts.settings.clone();
        let config_dir = opts.config_dir.clone();
        let fetcher = opts.fetcher.clone();
        let notifier = opts.notifier.clone();
        let pending_mailto = opts.mailto.clone();
        let requests = opts.requests.clone();
        let mut this = Self {
            backend,
            pal: Palette::of(opts.dark),
            opts,
            now,
            focus,
            hello: Load::Loading,
            accounts: Vec::new(),
            mailboxes: Vec::new(),
            status: Vec::new(),
            counts: Vec::new(),
            sending: Vec::new(),
            accounts_loaded: false,
            setup: crate::ui::setup::Setup::new(window, cx),
            expanded_accounts: HashSet::new(),
            view: View::Unified(MailboxRole::Inbox),
            search,
            search_text: None,
            search_focused: false,
            threads: Vec::new(),
            next_page: None,
            list: Load::Loading,
            loading_more: false,
            list_gen: 0,
            list_scroll: UniformListScrollHandle::new(),
            cursor: None,
            selected: BTreeSet::new(),
            open: None,
            open_gen: 0,
            reader_scroll: ScrollHandle::new(),
            ai_panel: false,
            ai: AiState { thread: None, summary: Load::Loading, items: Load::Loading, chat: Vec::new(), asking: false },
            ask_input,
            composer: None,
            pending_send: None,
            palette: None,
            shortcuts: false,
            toast: None,
            toast_counter: 0,
            undo: None,
            meta_cache: HashMap::new(),
            settings,
            config_dir,
            fetcher,
            notifier,
            images: HashMap::new(),
            files: HashMap::new(),
            caps: HashMap::new(),
            daemon_menu: false,
            daemon: None,
            last_saved: None,
            contacts: Contacts::default(),
            link: None,
            link_trouble: false,
            window_active: window.is_window_active(),
            mono_font,
            hovered_link: None,
            confirm_link: None,
            pending_mailto,
            activate_requested: false,
            requests,
            _tasks: Vec::new(),
            _subs: subs,
        };
        this.start(cx);
        this.focus.focus(window, cx);
        this
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let hello = b.hello().await;
            let accounts = b.accounts().await;
            let caps = b.capabilities().await;
            let mailboxes = b.mailboxes(None).await;
            let status = b.status().await;
            let _ = this.update(cx, |this, cx| {
                this.hello = match hello {
                    Ok(h) => Load::Ready(h.version),
                    Err(e) => Load::Failed(e.to_string()),
                };
                match accounts {
                    Ok(a) => {
                        this.accounts = a;
                        this.accounts_loaded = true;
                        this.load_contacts(cx);
                    }
                    Err(e) => this.list = Load::Failed(plain_error(&e)),
                }
                if let Ok(m) = mailboxes {
                    this.mailboxes = m;
                }
                if let Ok(c) = caps {
                    this.caps = c.into_iter().map(|c| (c.account, c.capabilities)).collect();
                }
                if let Ok(s) = status {
                    this.status = s;
                }
                this.load_counts(cx);
                cx.notify();
            });
        })
        .detach();
        self.load_threads(false, cx);

        if let Some(link) = self.backend.link() {
            self.link = self.backend.link_state();
            self._tasks.push(cx.spawn(async move |this, cx| {
                while let Ok(state) = link.recv().await {
                    if this.update(cx, |this, cx| this.on_link(state, cx)).is_err() {
                        break;
                    }
                }
            }));
        }
        if let Some(requests) = self.requests.clone() {
            self._tasks.push(cx.spawn(async move |this, cx| {
                while let Ok(req) = requests.recv().await {
                    if this.update(cx, |this, cx| this.on_request(req, cx)).is_err() {
                        break;
                    }
                }
            }));
        }

        let events = self.backend.events();
        self._tasks.push(cx.spawn(async move |this, cx| {
            while let Ok(ev) = events.recv().await {
                if this.update(cx, |this, cx| this.on_event(ev, cx)).is_err() {
                    break;
                }
            }
        }));
        if self.opts.now.is_none() {
            self._tasks.push(cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(Duration::from_secs(30)).await;
                if this.update(cx, |this, cx| { this.now = crate::time::now(); cx.notify(); }).is_err() {
                    break;
                }
            }));
        }
    }

    pub fn retry(&mut self, cx: &mut Context<Self>) {
        self.list = Load::Loading;
        self._tasks.clear();
        self.start(cx);
        cx.notify();
    }

    fn on_event(&mut self, ev: Event, cx: &mut Context<Self>) {
        match ev {
            Event::Changed { threads, account } => {
                for t in &threads {
                    self.meta_cache.remove(&(account.clone(), t.clone()));
                }
                self.load_threads(true, cx);
                self.reload_mailboxes(cx);
                self.load_counts(cx);
                if self.open.as_ref().is_some_and(|o| o.summary.account == account && threads.contains(&o.summary.id)) {
                    self.refresh_open(cx);
                }
            }
            Event::NewMail { from, subject, account, .. } => {
                let anon = self.is_anon(&account);
                // Never quote sensitive mail: one fixed line ("A security code from Brightdesk").
                let line = match safety::sensitive_kind(&from, &subject, "") {
                    Some(kind) => format!("{kind} from {from}"),
                    None => format!("New mail from {from}: {subject}"),
                };
                if !anon || matches!(&self.view, View::Mailbox { account: a, .. } if *a == account) {
                    self.show_toast(line, false, cx);
                }
                // A desktop notification when the window is in the background (and the settings allow).
                if !self.window_active
                    && let Some(note) = notify::new_mail_note(&from, &subject, anon, &self.settings.notifications)
                {
                    self.notifier.notify(&note);
                }
            }
            Event::Status { status } => self.on_status(status, cx),
            Event::ActionFailed { account, action, error } => self.on_action_failed(account, action, error, cx),
        }
        cx.notify();
    }

    /// A status from the daemon: queued changes while offline are said once; a queue that ran empty
    /// means the mail we were sending went out.
    fn on_status(&mut self, status: AccountStatus, cx: &mut Context<Self>) {
        let before = self.status.iter().find(|s| s.account == status.account).map(|s| s.queued).unwrap_or(0);
        if !status.online && status.queued > before {
            let n = status.queued;
            self.show_toast(format!("Offline: {n} change{} queued", if n == 1 { "" } else { "s" }), false, cx);
        }
        let done = status.online && status.queued == 0;
        let account = status.account.clone();
        if let Some(s) = self.status.iter_mut().find(|s| s.account == status.account) {
            *s = status;
        } else {
            self.status.push(status);
        }
        if done && self.sending.iter().any(|s| s.account == account) {
            let sent: Vec<Sending> = self.sending.extract_if(.., |s| s.account == account).collect();
            let text = match sent.as_slice() {
                [one] => format!("Sent “{}”", one.subject),
                many => format!("Sent {} messages", many.len()),
            };
            self.show_toast(text, false, cx);
        }
    }

    fn on_action_failed(&mut self, account: AccountId, action: Action, error: Error, cx: &mut Context<Self>) {
        if let Action::Send { draft } = &action
            && let Some(i) = self.sending.iter().position(|s| s.account == account && s.local_id == draft.local_id)
        {
            // Give the message back so nothing the person wrote is lost.
            let mut s = self.sending.remove(i);
            s.composer.error = Some(format!("Not sent: {}", plain_error(&error)));
            if self.composer.is_none() {
                self.composer = Some(s.composer);
            }
            self.show_toast(format!("Not sent: {}", plain_error(&error)), true, cx);
            return;
        }
        self.show_toast(format!("A change didn't reach the server: {}", plain_error(&error)), true, cx);
        self.load_threads(true, cx);
    }

    /// Sidebar badges: unread threads per AI category across the unified accounts' inboxes.
    pub fn load_counts(&mut self, cx: &mut Context<Self>) {
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let counts = b.counts(CountsRequest { accounts: None, role: Some(MailboxRole::Inbox) }).await;
            let _ = this.update(cx, |this, cx| {
                if let Ok(c) = counts {
                    this.counts = c;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn category_count(&self, c: Category) -> Option<u32> {
        self.counts.iter().find(|x| x.category == c).map(|x| x.unread)
    }

    pub fn is_readonly(&self, id: &AccountId) -> bool {
        self.account(id).is_some_and(|a| a.readonly)
    }

    /// Why nothing will happen on these accounts, if one of them is read-only (shown instead of an error).
    pub fn readonly_note(&self, ids: &[AccountId]) -> Option<String> {
        let ro: Vec<&str> = ids.iter().filter_map(|id| self.account(id).filter(|a| a.readonly).map(|a| a.label.as_str())).collect();
        match ro.as_slice() {
            [] => None,
            [one] => Some(format!("{one} is read-only: nothing is changed on the server")),
            _ => Some("Those accounts are read-only: nothing is changed on the server".into()),
        }
    }

    /// How `verb` is offered on `account`: disabled with the reason on a read-only account (replies stay:
    /// another address can send them), hidden when the account cannot do it, labelled when it changes only
    /// this device's copy.
    pub fn offer(&self, account: &AccountId, verb: Verb) -> Offer {
        let readonly = self.is_readonly(account);
        if readonly && verb != Verb::Reply {
            return Offer::Disabled(self.readonly_note(std::slice::from_ref(account)).unwrap_or_default());
        }
        let Some(c) = self.caps.get(account).filter(|_| !readonly) else { return Offer::Server };
        let support = match verb {
            Verb::Archive => c.archive,
            Verb::Trash => c.trash,
            Verb::Star => c.flagged,
            Verb::Read => c.seen,
            Verb::Reply if c.send => lumbridge_mail_core::Support::Server,
            Verb::Reply => lumbridge_mail_core::Support::Unsupported,
        };
        match support {
            lumbridge_mail_core::Support::Server => Offer::Server,
            lumbridge_mail_core::Support::Local => Offer::Local,
            lumbridge_mail_core::Support::Unsupported => Offer::Hidden,
        }
    }

    /// The offer for several accounts at once (a selection): any on the server wins, then this device;
    /// disabled when none can and one is read-only; hidden when none can at all.
    pub fn offer_for(&self, accounts: &[AccountId], verb: Verb) -> Offer {
        let offers: Vec<Offer> = accounts.iter().map(|a| self.offer(a, verb)).collect();
        if offers.is_empty() || offers.contains(&Offer::Server) {
            Offer::Server
        } else if offers.contains(&Offer::Local) {
            Offer::Local
        } else {
            offers.into_iter().find(|o| matches!(o, Offer::Disabled(_))).unwrap_or(Offer::Hidden)
        }
    }

    /// Drop the targets whose account cannot do `verb` (read-only, or not supported), saying so; None
    /// when nothing is left to act on. The flag says every target left changes only this device's copy.
    fn writable(&mut self, targets: Vec<ThreadSummary>, verb: Verb, cx: &mut Context<Self>) -> Option<(Vec<ThreadSummary>, bool)> {
        let ro: Vec<AccountId> = targets.iter().filter(|t| self.is_readonly(&t.account)).map(|t| t.account.clone()).collect();
        if let Some(note) = self.readonly_note(&ro) {
            self.show_toast(note, false, cx);
        }
        let cannot: Vec<String> = targets
            .iter()
            .filter(|t| !self.is_readonly(&t.account) && self.offer(&t.account, verb) == Offer::Hidden)
            .filter_map(|t| self.account(&t.account).map(|a| a.label.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if !cannot.is_empty() {
            self.show_toast(format!("{} cannot change the {} of mail", cannot.join(", "), verb.name()), false, cx);
        }
        let left: Vec<ThreadSummary> =
            targets.into_iter().filter(|t| matches!(self.offer(&t.account, verb), Offer::Server | Offer::Local)).collect();
        let local = left.iter().all(|t| self.offer(&t.account, verb) == Offer::Local);
        (!left.is_empty()).then_some((left, local))
    }

    pub fn account(&self, id: &AccountId) -> Option<&Account> {
        self.accounts.iter().find(|a| &a.id == id)
    }

    pub fn is_anon(&self, id: &AccountId) -> bool {
        self.account(id).is_some_and(|a| a.persona == Persona::Anon)
    }

    pub fn is_demo(&self) -> bool {
        self.backend.as_demo().is_some()
    }

    pub fn show_toast(&mut self, text: impl Into<String>, error: bool, cx: &mut Context<Self>) {
        self.toast_counter += 1;
        let id = self.toast_counter;
        self.toast = Some(Toast { text: text.into(), error, id });
        cx.notify();
        self._tasks.push(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(if error { 8 } else { 4 })).await;
            let _ = this.update(cx, |this, cx| {
                if this.toast.as_ref().is_some_and(|t| t.id == id) {
                    this.toast = None;
                    cx.notify();
                }
            });
        }));
    }

    // ---- the list

    /// The accounts a view covers. None = every non-anon account.
    pub fn view_accounts(&self) -> Option<Vec<AccountId>> {
        match &self.view {
            View::Mailbox { account, .. } => Some(vec![account.clone()]),
            _ => None,
        }
    }

    pub fn query(&self) -> Query {
        let mut q = Query { accounts: self.view_accounts(), limit: PAGE, ..Default::default() };
        if let Some(text) = &self.search_text {
            q.text = Some(text.clone());
            return q;
        }
        match &self.view {
            View::Unified(role) => q.role = Some(*role),
            View::Category(c) => q.category = Some(*c),
            View::Mailbox { mailbox, .. } => q.mailbox = Some(mailbox.clone()),
        }
        q
    }

    /// Run the current query again. `keep` keeps the rows, the cursor and the scroll (a refresh).
    pub fn load_threads(&mut self, keep: bool, cx: &mut Context<Self>) {
        self.list_gen += 1;
        let generation = self.list_gen;
        let mut q = self.query();
        if keep {
            q.limit = (self.threads.len() as u32).max(PAGE);
        } else {
            self.list = Load::Loading;
            self.threads.clear();
            self.next_page = None;
            self.cursor = None;
            self.selected.clear();
        }
        self.loading_more = false;
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let page = b.threads(q).await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.list_gen {
                    return;
                }
                match page {
                    Ok(page) => {
                        for t in &page.threads {
                            for a in &t.participants {
                                this.contacts.learn(&t.account, a, t.date);
                            }
                        }
                        let keep_id = this.cursor.and_then(|i| this.threads.get(i)).map(key);
                        this.threads = page.threads;
                        this.next_page = page.next;
                        this.list = Load::Ready(());
                        this.cursor = keep_id
                            .and_then(|id| this.threads.iter().position(|t| key(t) == id))
                            .or(if keep { this.cursor.map(|c| c.min(this.threads.len().saturating_sub(1))) } else { None })
                            .filter(|_| !this.threads.is_empty());
                        let ids: HashSet<Key> = this.threads.iter().map(key).collect();
                        this.selected.retain(|t| ids.contains(t));
                    }
                    Err(e) => this.list = Load::Failed(plain_error(&e)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub fn maybe_load_more(&mut self, visible_end: usize, cx: &mut Context<Self>) {
        if self.loading_more || self.next_page.is_none() || visible_end + PREFETCH < self.threads.len() {
            return;
        }
        self.loading_more = true;
        let generation = self.list_gen;
        let mut q = self.query();
        q.page = self.next_page.clone();
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let page = b.threads(q).await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.list_gen {
                    return;
                }
                this.loading_more = false;
                if let Ok(page) = page {
                    let have: HashSet<Key> = this.threads.iter().map(key).collect();
                    this.threads.extend(page.threads.into_iter().filter(|t| !have.contains(&key(t))));
                    this.next_page = page.next;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn reload_mailboxes(&mut self, cx: &mut Context<Self>) {
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(m) = b.mailboxes(None).await {
                let _ = this.update(cx, |this, cx| {
                    this.mailboxes = m;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub fn set_view(&mut self, view: View, window: &mut Window, cx: &mut Context<Self>) {
        if let View::Mailbox { account, .. } = &view {
            self.expanded_accounts.insert(account.clone());
        }
        self.view = view;
        self.search_text = None;
        self.search.update(cx, |s, cx| s.set_value("", window, cx));
        self.open = None;
        self.list_scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.load_threads(false, cx);
        self.focus.focus(window, cx);
    }

    pub fn go_role(&mut self, role: MailboxRole, window: &mut Window, cx: &mut Context<Self>) {
        // In an account, `g` keys stay in that account; elsewhere they go to the unified view.
        let view = match &self.view {
            View::Mailbox { account, .. } => self
                .mailboxes
                .iter()
                .find(|m| &m.account == account && m.role == role)
                .map(|m| View::Mailbox { account: account.clone(), mailbox: m.id.clone() })
                .unwrap_or(View::Unified(role)),
            _ => View::Unified(role),
        };
        self.set_view(view, window, cx);
    }

    pub fn run_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.search.read(cx).value().trim().to_string();
        self.search_text = (!text.is_empty()).then_some(text);
        self.open = None;
        self.load_threads(false, cx);
        self.focus.focus(window, cx);
    }

    pub fn insert_search_operator(&mut self, op: &str, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.search.read(cx).value().to_string();
        let sep = if current.is_empty() || current.ends_with(' ') { "" } else { " " };
        let value = format!("{current}{sep}{op}");
        self.search.update(cx, |s, cx| {
            s.set_value(value, window, cx);
            s.focus(window, cx);
            let end = s.value().len();
            s.set_selected_range(end..end, cx);
        });
    }

    pub fn move_cursor(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.threads.is_empty() {
            return;
        }
        let last = self.threads.len() as isize - 1;
        let next = match self.cursor {
            None => 0,
            Some(c) => (c as isize + delta).clamp(0, last) as usize,
        };
        self.cursor = Some(next);
        self.list_scroll.scroll_to_item(next, ScrollStrategy::Nearest);
        // With the reader open, j/k move through conversations like Gmail does.
        if self.open.is_some() {
            self.open_at(next, window, cx);
        }
        self.maybe_load_more(next, cx);
        cx.notify();
    }

    /// The threads an action applies to: the selection, else the one under the cursor.
    pub fn targets(&self) -> Vec<ThreadSummary> {
        if !self.selected.is_empty() {
            return self.threads.iter().filter(|t| self.selected.contains(&key(t))).cloned().collect();
        }
        if let Some(open) = &self.open {
            return vec![open.summary.clone()];
        }
        self.cursor.and_then(|c| self.threads.get(c)).cloned().into_iter().collect()
    }

    pub fn click_row(&mut self, ix: usize, modifiers: Modifiers, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = self.threads.get(ix) else { return };
        if modifiers.platform || modifiers.control {
            let id = key(t);
            if !self.selected.remove(&id) {
                self.selected.insert(id);
            }
            self.cursor = Some(ix);
        } else if modifiers.shift {
            let anchor = self.cursor.unwrap_or(ix);
            let (a, b) = if anchor <= ix { (anchor, ix) } else { (ix, anchor) };
            for t in &self.threads[a..=b] {
                self.selected.insert(key(t));
            }
            self.cursor = Some(ix);
        } else {
            self.selected.clear();
            self.open_at(ix, window, cx);
        }
        self.focus.focus(window, cx);
        cx.notify();
    }

    // ---- the reader

    pub fn open_at(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(summary) = self.threads.get(ix).cloned() else { return };
        self.cursor = Some(ix);
        self.open_summary(summary, None, window, cx);
    }

    /// Open a thread; `then` runs once its messages are in (used by reply when nothing was open).
    pub fn open_summary(&mut self, summary: ThreadSummary, then: Option<ComposeMode>, window: &mut Window,
                        cx: &mut Context<Self>) {
        self.open_gen += 1;
        let generation = self.open_gen;
        let tref = ThreadRef { account: summary.account.clone(), thread: summary.id.clone() };
        let unread = summary.unread_count > 0;
        self.open = Some(OpenThread {
            summary,
            messages: Load::Loading,
            bodies: HashMap::new(),
            expanded: HashSet::new(),
            show_quoted: HashSet::new(),
            show_middle: false,
            remote_ok: HashSet::new(),
            confirm_remote: None,
            rendered: HashMap::new(),
        });
        self.hovered_link = None;
        self.reader_scroll.set_offset(point(px(0.), px(0.)));
        if self.ai_panel {
            self.load_ai(cx);
        }
        let b = self.backend.clone();
        cx.spawn_in(window, async move |this, cx| {
            let msgs = b.thread(tref.clone()).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if generation != this.open_gen {
                    return;
                }
                let Some(open) = this.open.as_mut() else { return };
                match msgs {
                    Ok(msgs) => {
                        // Expand the newest message and every unread one; the rest stay one line each.
                        let n = msgs.len();
                        for (i, m) in msgs.iter().enumerate() {
                            if i + 1 == n || !m.flags.seen {
                                open.expanded.insert(m.id.clone());
                            }
                        }
                        this.meta_cache.insert((tref.account.clone(), tref.thread.clone()), msgs.iter().map(|m| m.id.clone()).collect());
                        for m in &msgs {
                            for a in m.from.iter().chain(&m.to).chain(&m.cc) {
                                this.contacts.learn(&m.account, a, m.date);
                            }
                        }
                        let ids: Vec<MessageId> = open.expanded.iter().cloned().collect();
                        open.messages = Load::Ready(msgs);
                        for id in ids {
                            this.ensure_body(&id, cx);
                        }
                        if unread {
                            this.mark_open_read(cx);
                        }
                        if let Some(mode) = then {
                            this.open_composer(mode, window, cx);
                        }
                    }
                    Err(e) => open.messages = Load::Failed(e.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn refresh_open(&mut self, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let tref = open.tref();
        let generation = self.open_gen;
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let msgs = b.thread(tref).await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.open_gen {
                    return;
                }
                if let (Some(open), Ok(msgs)) = (this.open.as_mut(), msgs) {
                    if let Some(last) = msgs.last() {
                        open.expanded.insert(last.id.clone());
                    }
                    let ids: Vec<MessageId> = open.expanded.iter().cloned().collect();
                    open.messages = Load::Ready(msgs);
                    if let Some(t) = this.threads.iter().find(|t| key(t) == key(&open.summary)) {
                        open.summary = t.clone();
                    }
                    for id in ids {
                        this.ensure_body(&id, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn ensure_body(&mut self, id: &MessageId, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else { return };
        if open.bodies.contains_key(id) {
            return;
        }
        open.bodies.insert(id.clone(), Load::Loading);
        let mref = lumbridge_mail_core::rpc::MessageRef { account: open.summary.account.clone(), message: id.clone() };
        let generation = self.open_gen;
        let id = id.clone();
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let body = b.body(mref).await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.open_gen {
                    return;
                }
                let ok = body.is_ok();
                if let Some(open) = this.open.as_mut() {
                    open.bodies.insert(id.clone(), body.map(Load::Ready).unwrap_or_else(|e| Load::Failed(e.to_string())));
                }
                if ok {
                    this.render_body(&id, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The sender of a message in the open thread.
    fn sender_of(&self, id: &MessageId) -> Option<Address> {
        let open = self.open.as_ref()?;
        open.messages.ready()?.iter().find(|m| &m.id == id)?.from.first().cloned()
    }

    /// May this message's remote images load without asking? Only for a non-anon account and a
    /// sender the person allowed, or after "Load images" on this message.
    pub fn remote_allowed(&self, id: &MessageId) -> bool {
        let Some(open) = &self.open else { return false };
        if open.remote_ok.contains(id) {
            return true;
        }
        !self.is_anon(&open.summary.account) && self.sender_of(id).is_some_and(|a| self.settings.sender_allowed(&a.email))
    }

    /// Run a body through the renderer (off the UI thread) and then load its pictures.
    pub fn render_body(&mut self, id: &MessageId, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let Some(Load::Ready(body)) = open.bodies.get(id) else { return };
        let body = body.clone();
        let from = self.sender_of(id);
        let remote = self.remote_allowed(id);
        let generation = self.open_gen;
        let id = id.clone();
        let task = cx.background_spawn(async move { reading::render(&body, from.as_ref(), remote) });
        cx.spawn(async move |this, cx| {
            let rendered = task.await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.open_gen {
                    return;
                }
                if let Some(open) = this.open.as_mut() {
                    open.rendered.insert(id.clone(), rendered);
                }
                this.load_images(&id, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Fetch or decode the pictures a rendered message shows: its inline parts and data: images always
    /// (they are in the message already), remote ones only when they are in the tree as `Remote`, which
    /// the renderer does only after the person allowed it.
    fn load_images(&mut self, id: &MessageId, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let Some(r) = open.rendered.get(id) else { return };
        let account = open.summary.account.clone();
        let keys: Vec<ImageKey> = reading::images(&r.doc).into_iter().filter_map(ImageKey::of).collect();
        let mut remote_count = 0;
        for key in keys {
            if self.images.contains_key(&key) {
                continue;
            }
            if matches!(key, ImageKey::Remote(_)) {
                remote_count += 1;
                if remote_count > remote::MAX_IMAGES_PER_MESSAGE {
                    continue;
                }
            }
            self.images.insert(key.clone(), Load::Loading);
            let fetcher = self.fetcher.clone();
            let b = self.backend.clone();
            let aref = match &key {
                ImageKey::Part(att) => Some(AttachmentRef { account: account.clone(), message: id.clone(), attachment: att.clone() }),
                _ => None,
            };
            let k = key.clone();
            let task = cx.background_spawn(async move {
                let bytes = match &k {
                    ImageKey::Remote(url) => fetcher.fetch(url),
                    ImageKey::Data(uri) => remote::data_uri_bytes(uri).ok_or_else(|| "bad data: image".to_string()),
                    ImageKey::Part(_) => match aref {
                        Some(r) => match b.attachment(r).await {
                            Ok(path) => read_capped(&path),
                            Err(e) => Err(e.to_string()),
                        },
                        None => Err("no part".into()),
                    },
                };
                bytes.and_then(remote::decode)
            });
            cx.spawn(async move |this, cx| {
                let result = task.await;
                let _ = this.update(cx, |this, cx| {
                    this.images.insert(key, match result {
                        Ok(p) => Load::Ready(p),
                        Err(e) => Load::Failed(e),
                    });
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// "Load images" (this message), "Always for this sender" (`always`), or "Load anyway" after the
    /// anon warning (`confirmed`). Anon accounts always ask again and never join the sender list.
    pub fn allow_remote(&mut self, id: &MessageId, always: bool, confirmed: bool, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let anon = self.is_anon(&open.summary.account);
        if anon && !confirmed {
            if let Some(o) = self.open.as_mut() {
                o.confirm_remote = Some(id.clone());
            }
            cx.notify();
            return;
        }
        if always && !anon
            && let Some(sender) = self.sender_of(id)
        {
            self.settings.allow_sender(&sender.email);
            self.save_settings(cx);
            self.show_toast(format!("Images from {} will load from now on", sender.email), false, cx);
        }
        if let Some(o) = self.open.as_mut() {
            o.remote_ok.insert(id.clone());
            o.confirm_remote = None;
        }
        self.render_body(id, cx);
        cx.notify();
    }

    /// `m`: load images in the expanded messages of the open thread that have some blocked (on an anon
    /// account this asks first, on the newest one).
    pub fn load_remote_in_open(&mut self, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let anon = self.is_anon(&open.summary.account);
        let ids: Vec<MessageId> = open
            .messages
            .ready()
            .map(|m| {
                m.iter()
                    .filter(|m| open.expanded.contains(&m.id))
                    .filter(|m| open.rendered.get(&m.id).is_some_and(|r| r.doc.report.remote_blocked && r.doc.report.remote_images > 0))
                    .map(|m| m.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        if anon {
            if let Some(last) = ids.last() {
                self.allow_remote(last, false, false, cx);
            }
            return;
        }
        for id in ids {
            self.allow_remote(&id, false, false, cx);
        }
    }

    pub fn forget_remote_sender(&mut self, email: &str, cx: &mut Context<Self>) {
        self.settings.forget_sender(email);
        self.save_settings(cx);
        let ids: Vec<MessageId> = self.open.as_ref().map(|o| o.rendered.keys().cloned().collect()).unwrap_or_default();
        for id in ids {
            self.render_body(&id, cx);
        }
        self.show_toast(format!("Images from {email} will ask first again"), false, cx);
    }

    pub fn toggle_quoted(&mut self, id: &MessageId, cx: &mut Context<Self>) {
        if let Some(o) = self.open.as_mut()
            && !o.show_quoted.remove(id)
        {
            o.show_quoted.insert(id.clone());
        }
        cx.notify();
    }

    /// A link in a message was clicked. `mailto:` opens the composer; a deceptive link asks first;
    /// anything else goes to the browser (the app itself never fetches it).
    pub fn click_link(&mut self, link: Link, window: &mut Window, cx: &mut Context<Self>) {
        if link.href.to_ascii_lowercase().starts_with("mailto:") {
            if let Some(m) = crate::mailto::parse(&link.href) {
                self.open_mailto(m, window, cx);
            }
            return;
        }
        if link.warning.is_some() {
            self.confirm_link = Some(link);
            cx.notify();
            return;
        }
        cx.open_url(&link.href);
    }

    pub fn hover_link(&mut self, link: Option<Link>, cx: &mut Context<Self>) {
        if self.hovered_link != link {
            self.hovered_link = link;
            cx.notify();
        }
    }

    /// The Unsubscribe button: a mailto unsubscribe becomes a draft the person sends (it is mail like
    /// any other); otherwise the sender's page opens in the browser. The app makes no request itself.
    pub fn unsubscribe(&mut self, id: &MessageId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let Some(unsub) = open.rendered.get(id).and_then(|r| r.info.unsubscribe.clone()) else { return };
        if let Some(m) = unsub.mailto {
            let to = Address::new(None, &m.to);
            let mailto = Mailto { to: vec![to], subject: m.subject.unwrap_or_else(|| "unsubscribe".into()),
                                  body: m.body.unwrap_or_default(), ..Default::default() };
            self.open_mailto(mailto, window, cx);
        } else if let Some(web) = unsub.web {
            cx.open_url(&web);
            self.show_toast("Opened the sender's unsubscribe page in your browser", false, cx);
        }
    }

    /// Open the composer filled from a `mailto:` (nothing is sent until the person presses Send).
    pub fn open_mailto(&mut self, m: Mailto, window: &mut Window, cx: &mut Context<Self>) {
        if self.accounts.is_empty() {
            self.pending_mailto = Some(m);
            return;
        }
        // From a message (a link, Unsubscribe): write from the account it came to, not the default one.
        let from = self.open.as_ref().and_then(|o| self.accounts.iter().position(|a| a.id == o.summary.account));
        self.open_composer(ComposeMode::New, window, cx);
        if let Some(c) = self.composer.as_mut() {
            if let Some(f) = from {
                c.from = f;
            }
            c.fill_from_mailto(m, window, cx);
        }
        cx.notify();
    }

    fn on_request(&mut self, req: Request, cx: &mut Context<Self>) {
        match req {
            Request::Activate => self.activate_requested = true,
            Request::Open(url) => {
                self.activate_requested = true;
                match crate::mailto::parse(&url) {
                    Some(m) => self.pending_mailto = Some(m),
                    None => self.show_toast("Only mailto: links can be opened here", true, cx),
                }
            }
        }
        cx.notify();
    }

    fn on_link(&mut self, state: LinkState, cx: &mut Context<Self>) {
        let back = state == LinkState::Connected;
        if !back {
            self.link_trouble = true;
        }
        self.link = Some(state);
        // The daemon is back (started, or restarted): load everything again.
        if back && (self.link_trouble || matches!(self.list, Load::Failed(_)) || matches!(self.hello, Load::Failed(_))) {
            self.link_trouble = false;
            self.retry(cx);
        }
        cx.notify();
    }

    fn load_contacts(&mut self, cx: &mut Context<Self>) {
        let b = self.backend.clone();
        let ids: Vec<AccountId> = self.accounts.iter().map(|a| a.id.clone()).collect();
        cx.spawn(async move |this, cx| {
            for id in ids {
                if let Ok(people) = b.contacts(id.clone()).await {
                    let _ = this.update(cx, |this, _| {
                        for p in &people {
                            this.contacts.learn(&id, p, 0);
                        }
                    });
                }
            }
        })
        .detach();
    }

    pub fn save_settings(&mut self, cx: &mut Context<Self>) {
        if let Some(dir) = &self.config_dir
            && let Err(e) = self.settings.save(dir)
        {
            self.show_toast(format!("Couldn't save settings: {e}"), true, cx);
        }
    }

    pub fn row_height(&self) -> f32 {
        match self.settings.density {
            Density::Comfortable => crate::ui::list::ROW_HEIGHT,
            Density::Compact => crate::ui::list::COMPACT_ROW_HEIGHT,
        }
    }

    pub fn toggle_density(&mut self, cx: &mut Context<Self>) {
        self.settings.density = match self.settings.density {
            Density::Comfortable => Density::Compact,
            Density::Compact => Density::Comfortable,
        };
        self.save_settings(cx);
        cx.notify();
    }

    pub fn toggle_notifications(&mut self, anon: bool, cx: &mut Context<Self>) {
        let n = &mut self.settings.notifications;
        let text = if anon {
            n.anon = !n.anon;
            if n.anon { "Notifications for anon accounts: on" } else { "Notifications for anon accounts: off" }
        } else {
            n.enabled = !n.enabled;
            if n.enabled { "Desktop notifications: on" } else { "Desktop notifications: off" }
        };
        self.save_settings(cx);
        self.show_toast(text, false, cx);
    }

    pub fn toggle_message(&mut self, id: &MessageId, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else { return };
        if !open.expanded.remove(id) {
            open.expanded.insert(id.clone());
            self.ensure_body(id, cx);
        }
        cx.notify();
    }

    pub fn expand_all(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else { return };
        let ids: Vec<MessageId> = open.messages.ready().map(|m| m.iter().map(|m| m.id.clone()).collect()).unwrap_or_default();
        open.show_middle = true;
        for id in &ids {
            open.expanded.insert(id.clone());
        }
        for id in ids {
            self.ensure_body(&id, cx);
        }
        cx.notify();
    }

    fn mark_open_read(&mut self, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        let Some(msgs) = open.messages.ready() else { return };
        let ids: Vec<MessageId> = msgs.iter().filter(|m| !m.flags.seen).map(|m| m.id.clone()).collect();
        if ids.is_empty() {
            return;
        }
        let account = open.summary.account.clone();
        if self.is_readonly(&account) {
            // Reading changes nothing on a read-only account, not even the unread flag.
            return;
        }
        let tid = key(&open.summary);
        if let Some(t) = self.threads.iter_mut().find(|t| key(t) == tid) {
            t.unread_count = 0;
        }
        if let Some(open) = self.open.as_mut() {
            open.summary.unread_count = 0;
        }
        self.send_acts(vec![(account, Action::SetSeen { ids, seen: true })], None, cx);
    }

    pub fn close_reader(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = None;
        self.focus.focus(window, cx);
        cx.notify();
    }

    // ---- actions on threads

    fn send_acts(&mut self, acts: Vec<(AccountId, Action)>, undo: Option<UndoStep>, cx: &mut Context<Self>) {
        if let Some(u) = undo {
            self.undo = Some(u);
        }
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let mut failure = None;
            for (account, action) in acts {
                // Offline is not a failure: the change is queued and the status says so (`on_status`).
                if let Err(e) = b.act(Act { account, action, caller: Caller::Person }).await {
                    failure = Some(e);
                }
            }
            let _ = this.update(cx, |this, cx| {
                if let Some(e) = failure {
                    this.show_toast(format!("Couldn't do that: {}", plain_error(&e)), true, cx);
                    this.load_threads(true, cx);
                }
            });
        })
        .detach();
    }

    /// Fetch message ids for threads (cached), then run `make` per thread.
    fn act_on(&mut self, targets: Vec<ThreadSummary>, make: impl Fn(&ThreadSummary, Vec<MessageId>) -> Option<Action> + 'static,
              undo: impl Fn(&ThreadSummary, Vec<MessageId>) -> Option<Action> + 'static, label: String, cx: &mut Context<Self>) {
        let b = self.backend.clone();
        let cached: Vec<(ThreadSummary, Option<Vec<MessageId>>)> =
            targets.into_iter().map(|t| { let c = self.meta_cache.get(&key(&t)).cloned(); (t, c) }).collect();
        cx.spawn(async move |this, cx| {
            let mut acts = Vec::new();
            let mut back = Vec::new();
            for (t, ids) in cached {
                let ids = match ids {
                    Some(ids) => ids,
                    None => match b.thread(ThreadRef { account: t.account.clone(), thread: t.id.clone() }).await {
                        Ok(m) => m.into_iter().map(|m| m.id).collect(),
                        Err(_) => continue,
                    },
                };
                if let Some(a) = make(&t, ids.clone()) {
                    acts.push((t.account.clone(), a));
                }
                if let Some(a) = undo(&t, ids) {
                    back.push((t.account.clone(), a));
                }
            }
            let _ = this.update(cx, |this, cx| {
                let step = (!back.is_empty()).then_some(UndoStep { label, acts: back });
                this.send_acts(acts, step, cx);
            });
        })
        .detach();
    }

    fn mailbox_with_role(&self, account: &AccountId, role: MailboxRole) -> Option<MailboxId> {
        self.mailboxes.iter().find(|m| &m.account == account && m.role == role).map(|m| m.id.clone())
    }

    fn inbox_of(&self, account: &AccountId) -> MailboxId {
        self.mailboxes
            .iter()
            .find(|m| &m.account == account && m.role == MailboxRole::Inbox)
            .map(|m| m.id.clone())
            .unwrap_or_else(|| MailboxId::new(format!("{account}:inbox")))
    }

    /// Archive or trash: the rows leave the list at once; `z` brings them back.
    pub fn remove_targets(&mut self, trash: bool, window: &mut Window, cx: &mut Context<Self>) {
        let targets = self.targets();
        if targets.is_empty() {
            return;
        }
        let Some((targets, local)) = self.writable(targets, if trash { Verb::Trash } else { Verb::Archive }, cx) else { return };
        let in_place = matches!(self.view, View::Unified(MailboxRole::Trash))
            || (!trash && matches!(self.view, View::Unified(MailboxRole::Archive)));
        let ids: HashSet<Key> = targets.iter().map(key).collect();
        let was_open = self.open.as_ref().is_some_and(|o| ids.contains(&key(&o.summary)));
        let first = self.threads.iter().position(|t| ids.contains(&key(t)));
        if !in_place {
            self.threads.retain(|t| !ids.contains(&key(t)));
        }
        self.selected.clear();
        self.cursor = first.map(|f| f.min(self.threads.len().saturating_sub(1))).filter(|_| !self.threads.is_empty());
        if was_open {
            match self.cursor {
                Some(c) => self.open_at(c, window, cx),
                None => self.open = None,
            }
        }
        let n = targets.len();
        let label = format!("{} {n} conversation{}{}", if trash { "Moved to Trash:" } else { "Archived" }, if n == 1 { "" } else { "s" },
                            if local { " on this device only" } else { "" });
        self.show_toast(format!("{label}  ·  Z to undo"), false, cx);
        let inboxes: HashMap<AccountId, MailboxId> =
            targets.iter().map(|t| (t.account.clone(), self.inbox_of(&t.account))).collect();
        // Undo puts them back in the inbox. Out of the trash it is a real move (Gmail would otherwise keep
        // the Trash label); out of the archive it only adds the inbox (`from: None`).
        let trashes: HashMap<AccountId, MailboxId> = targets
            .iter()
            .filter_map(|t| self.mailbox_with_role(&t.account, MailboxRole::Trash).map(|m| (t.account.clone(), m)))
            .collect();
        self.act_on(
            targets,
            move |_, ids| Some(if trash { Action::Trash { ids } } else { Action::Archive { ids } }),
            move |t, ids| {
                let from = if trash { trashes.get(&t.account).cloned() } else { None };
                inboxes.get(&t.account).map(|to| Action::Move { ids, from, to: to.clone() })
            },
            label,
            cx,
        );
        cx.notify();
    }

    pub fn toggle_star(&mut self, cx: &mut Context<Self>) {
        let targets = self.targets();
        if targets.is_empty() {
            return;
        }
        let Some((targets, local)) = self.writable(targets, Verb::Star, cx) else { return };
        let flag = !targets.iter().all(|t| t.flagged);
        let ids: HashSet<Key> = targets.iter().map(key).collect();
        for t in self.threads.iter_mut().filter(|t| ids.contains(&key(t))) {
            t.flagged = flag;
        }
        if let Some(o) = self.open.as_mut().filter(|o| ids.contains(&key(&o.summary))) {
            o.summary.flagged = flag;
        }
        let label = format!("{}{}", if flag { "Starred" } else { "Unstarred" }, if local { " on this device only" } else { "" });
        if local {
            self.show_toast(label.clone(), false, cx);
        }
        self.act_on(targets, move |_, ids| Some(Action::SetFlagged { ids, flagged: flag }),
                    move |_, ids| Some(Action::SetFlagged { ids, flagged: !flag }), label, cx);
        cx.notify();
    }

    pub fn toggle_read(&mut self, cx: &mut Context<Self>) {
        let targets = self.targets();
        if targets.is_empty() {
            return;
        }
        let Some((targets, local)) = self.writable(targets, Verb::Read, cx) else { return };
        let seen = targets.iter().any(|t| t.unread_count > 0);
        let ids: HashSet<Key> = targets.iter().map(key).collect();
        for t in self.threads.iter_mut().filter(|t| ids.contains(&key(t))) {
            t.unread_count = if seen { 0 } else { 1 };
        }
        let label = format!("{}{}", if seen { "Marked read" } else { "Marked unread" }, if local { " on this device only" } else { "" });
        if local {
            self.show_toast(label.clone(), false, cx);
        }
        self.act_on(targets, move |_, ids| Some(Action::SetSeen { ids, seen }),
                    move |_, ids| Some(Action::SetSeen { ids, seen: !seen }), label, cx);
        cx.notify();
    }

    pub fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_send.is_some() {
            self.undo_send(window, cx);
            return;
        }
        let Some(step) = self.undo.take() else {
            self.show_toast("Nothing to undo", false, cx);
            return;
        };
        self.show_toast(format!("Undone: {}", step.label), false, cx);
        self.send_acts(step.acts, None, cx);
    }

    // ---- writing

    /// The account new mail is written from: the one in view, else the first non-anon account.
    pub fn default_from(&self) -> usize {
        let wanted = match &self.view {
            View::Mailbox { account, .. } => Some(account.clone()),
            _ => None,
        };
        wanted
            .and_then(|w| self.accounts.iter().position(|a| a.id == w))
            .or_else(|| self.accounts.iter().position(|a| a.persona != Persona::Anon))
            .unwrap_or(0)
    }

    pub fn open_composer(&mut self, mode: ComposeMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.accounts.is_empty() {
            self.show_toast("No accounts yet", true, cx);
            return;
        }
        if mode != ComposeMode::New {
            let ready = self.open.as_ref().is_some_and(|o| o.messages.ready().is_some());
            if !ready {
                // Open the conversation under the cursor first, then write.
                match self.cursor.and_then(|c| self.threads.get(c)).cloned() {
                    Some(t) if self.open.is_none() => self.open_summary(t, Some(mode), window, cx),
                    _ => {}
                }
                return;
            }
        }
        let mut composer = Composer::new(self, mode, window, cx);
        composer.focus_first(window, cx);
        let parts = std::mem::take(&mut composer.forward_parts);
        composer.fetching = parts.len();
        let local_id = composer.local_id.clone();
        self.composer = Some(composer);
        // A forward carries the original's files: the backend saves each to a local path.
        for (aref, att) in parts {
            let b = self.backend.clone();
            let local_id = local_id.clone();
            cx.spawn(async move |this, cx| {
                let saved = b.attachment(aref).await;
                let _ = this.update(cx, |this, cx| {
                    let Some(c) = this.composer.as_mut().filter(|c| c.local_id == local_id) else { return };
                    c.fetching = c.fetching.saturating_sub(1);
                    match saved {
                        Ok(path) => c.add_forwarded(&path, &att),
                        Err(e) => c.error = Some(format!("Couldn't attach {}: {e}", att.filename)),
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    pub fn close_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.composer.take() else { return };
        if !c.is_empty(cx) {
            let account = c.from_account(self).id.clone();
            if let Some(note) = self.readonly_note(std::slice::from_ref(&account)) {
                self.show_toast(format!("Draft not saved. {note}"), false, cx);
            } else {
                let draft = c.to_draft(cx);
                self.send_acts(vec![(account, Action::SaveDraft { draft })], None, cx);
                self.show_toast("Draft saved", false, cx);
            }
        }
        self.focus.focus(window, cx);
        cx.notify();
    }

    pub fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut c) = self.composer.take() else { return };
        c.commit_recipients(window, cx);
        if let Err(problem) = c.check(cx) {
            c.error = Some(problem);
            self.composer = Some(c);
            cx.notify();
            return;
        }
        let account = c.from_account(self).id.clone();
        if let Some(note) = self.readonly_note(std::slice::from_ref(&account)) {
            c.error = Some(format!("{note}, so it can't send. Pick another account in From."));
            self.composer = Some(c);
            cx.notify();
            return;
        }
        let draft = c.to_draft(cx);
        let secs = self.opts.undo_send_secs;
        let task = cx.spawn(async move |this, cx| {
            let mut left = secs;
            while left > 0 {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                left -= 1;
                if this.update(cx, |this, cx| {
                    if let Some(p) = this.pending_send.as_mut() {
                        p.remaining = left;
                    }
                    cx.notify();
                }).is_err() {
                    return;
                }
            }
            let _ = this.update(cx, |this, cx| this.dispatch_send(cx));
        });
        self.pending_send = Some(PendingSend { account, draft, composer: c, remaining: secs, _task: task });
        self.focus.focus(window, cx);
        cx.notify();
    }

    pub fn undo_send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.pending_send.take() {
            p.composer.focus_body(window, cx);
            self.composer = Some(p.composer);
            self.show_toast("Sending undone", false, cx);
        }
    }

    fn dispatch_send(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.pending_send.take() else { return };
        let b = self.backend.clone();
        let PendingSend { account, draft, composer, .. } = p;
        let (local_id, subject) = (draft.local_id.clone(), draft.subject.clone());
        cx.spawn(async move |this, cx| {
            // A person pressed Send: this is the only place the app asks for a send.
            let result = b.act(Act { account: account.clone(), action: Action::Send { draft }, caller: Caller::Person }).await;
            // Queued; ask where the queue is now (the daemon may already have sent it).
            let status = match &result {
                Ok(_) => b.status().await.ok(),
                Err(_) => None,
            };
            let _ = this.update(cx, |this, cx| match result {
                Ok(_) => {
                    let mine = status.as_ref().and_then(|st| st.iter().find(|s| s.account == account).cloned());
                    if let Some(st) = status {
                        this.status = st;
                    }
                    match mine {
                        Some(s) if s.online && s.queued == 0 => this.show_toast(format!("Sent “{subject}”"), false, cx),
                        Some(s) if !s.online => {
                            this.sending.push(Sending { account, local_id, subject, composer });
                            this.show_toast("Offline: queued, it goes when you're back online", false, cx);
                        }
                        _ => {
                            this.sending.push(Sending { account, local_id, subject, composer });
                            this.show_toast("Sending…", false, cx);
                        }
                    }
                }
                Err(e) => {
                    let why = match &e {
                        Error::Refused(r) if r == safety::SEND_NEEDS_A_PERSON => {
                            "this connection isn't trusted to send (the daemon didn't accept the app's person token)".to_string()
                        }
                        e => plain_error(e),
                    };
                    let mut composer = composer;
                    composer.error = Some(format!("Not sent: {why}"));
                    this.composer = Some(composer);
                    this.show_toast(format!("Not sent: {why}"), true, cx);
                }
            });
        })
        .detach();
    }

    pub fn ai_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.composer.as_mut() else { return };
        let instruction = c.ai_input.read(cx).value().trim().to_string();
        if instruction.is_empty() {
            c.ai_input.update(cx, |s, cx| s.focus(window, cx));
            return;
        }
        let account = self.accounts.get(c.from).map(|a| a.id.clone());
        let Some(account) = account else { return };
        if self.accounts.get(c.from).is_some_and(|a| a.ai == AiPolicy::Off) {
            c.error = Some("AI is off for this account.".into());
            cx.notify();
            return;
        }
        c.ai_busy = true;
        c.error = None;
        // Reply all asks the model to answer everyone; people the person added to Cc themselves are
        // the only new recipients a draft may get (the model never invents any).
        let reply_all = c.mode == ComposeMode::ReplyAll;
        let known: HashSet<String> = self
            .open
            .as_ref()
            .and_then(|o| o.messages.ready())
            .map(|m| m.iter().flat_map(|m| m.from.iter().chain(&m.to).chain(&m.cc)).map(|a| a.email.clone()).collect())
            .unwrap_or_default();
        let extra_cc: Vec<Address> = c.cc.iter().filter(|a| !known.contains(&a.email)).cloned().collect();
        let req = lumbridge_mail_core::rpc::DraftRequest { account, thread: c.thread.clone(), instruction, reply_all, extra_cc };
        let b = self.backend.clone();
        cx.spawn_in(window, async move |this, cx| {
            let draft = b.draft(req).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(c) = this.composer.as_mut() {
                    c.ai_busy = false;
                    match draft {
                        Ok(d) => c.fill_from_ai(d, window, cx),
                        Err(e) => c.error = Some(format!("The assistant couldn't draft this: {e}")),
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ---- the AI panel

    /// Why the AI may not look at this thread, if it may not.
    pub fn ai_refusal(&self, t: &ThreadSummary) -> Option<&'static str> {
        let category = t.triage.as_ref().map(|t| t.category);
        let from = t.participants.first().map(|p| p.email.as_str()).unwrap_or("");
        if category == Some(Category::Sensitive) || safety::sensitive_kind(from, &t.subject, "").is_some() {
            return Some("Security codes, password resets and bank mail are never given to a model.");
        }
        if self.account(&t.account).is_some_and(|a| a.ai == AiPolicy::Off) {
            return Some("AI is off for this account.");
        }
        None
    }

    pub fn load_ai(&mut self, cx: &mut Context<Self>) {
        let Some(open) = &self.open else { return };
        if self.ai.thread.as_ref() == Some(&key(&open.summary)) {
            return;
        }
        self.ai.thread = Some(key(&open.summary));
        if let Some(why) = self.ai_refusal(&open.summary) {
            self.ai.summary = Load::Failed(why.into());
            self.ai.items = Load::Ready(Extracted::default());
            cx.notify();
            return;
        }
        self.ai.summary = Load::Loading;
        self.ai.items = Load::Loading;
        let tref = open.tref();
        let triage = open.summary.triage.clone();
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let summary = b.summarise(tref.clone()).await;
            let items = b.extract(tref.clone(), triage).await;
            let _ = this.update(cx, |this, cx| {
                if this.ai.thread.as_ref() != Some(&(tref.account.clone(), tref.thread.clone())) {
                    return;
                }
                this.ai.summary = summary.map(Load::Ready).unwrap_or_else(|e| Load::Failed(match e {
                    Error::Refused(r) => r,
                    e => e.to_string(),
                }));
                this.ai.items = items.map(Load::Ready).unwrap_or_else(|e| Load::Failed(match e {
                    Error::Refused(r) => r,
                    e => plain_error(&e),
                }));
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub fn ask(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let question = self.ask_input.read(cx).value().trim().to_string();
        if question.is_empty() || self.ai.asking {
            return;
        }
        self.ask_input.update(cx, |s, cx| s.set_value("", window, cx));
        self.ai.chat.push(ChatLine { mine: true, text: question.clone(), sources: vec![], failed: false });
        self.ai.asking = true;
        let accounts = self.view_accounts();
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let answer = b.ask(question, accounts).await;
            let _ = this.update(cx, |this, cx| {
                this.ai.asking = false;
                let line = match answer {
                    Ok(a) => {
                        let sources = a
                            .sources
                            .into_iter()
                            .map(|r| {
                                let title = this.threads.iter().find(|t| t.id == r.thread && t.account == r.account).map(|t| t.subject.clone())
                                    .unwrap_or_else(|| "Open conversation".into());
                                (r, title)
                            })
                            .collect();
                        ChatLine { mine: false, text: a.text, sources, failed: false }
                    }
                    Err(e) => ChatLine { mine: false, text: format!("I couldn't answer that: {e}"), sources: vec![], failed: true },
                };
                this.ai.chat.push(line);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Open a thread from an answer's sources (it may not be in the list).
    pub fn open_ref(&mut self, tref: ThreadRef, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.threads.iter().position(|t| t.id == tref.thread && t.account == tref.account) {
            self.open_at(ix, window, cx);
            return;
        }
        let b = self.backend.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(msgs) = b.thread(tref.clone()).await else { return };
            let Some(last) = msgs.last() else { return };
            let mut participants = Vec::new();
            for m in &msgs {
                for a in &m.from {
                    if !participants.contains(a) {
                        participants.push(a.clone());
                    }
                }
            }
            let summary = ThreadSummary {
                id: tref.thread.clone(),
                account: tref.account.clone(),
                subject: msgs[0].subject.clone(),
                snippet: last.snippet.clone(),
                participants,
                message_count: msgs.len() as u32,
                unread_count: msgs.iter().filter(|m| !m.flags.seen).count() as u32,
                date: last.date,
                flagged: msgs.iter().any(|m| m.flags.flagged),
                has_attachments: msgs.iter().any(|m| m.has_attachments),
                mailboxes: last.mailboxes.clone(),
                triage: None,
            };
            let _ = this.update_in(cx, |this, window, cx| this.open_summary(summary, None, window, cx));
        })
        .detach();
    }

    pub fn toggle_ai_panel(&mut self, cx: &mut Context<Self>) {
        self.ai_panel = !self.ai_panel;
        if self.ai_panel {
            self.load_ai(cx);
        }
        cx.notify();
    }

    // ---- overlays

    pub fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.take().is_some() {
            self.focus.focus(window, cx);
        } else {
            self.shortcuts = false;
            let state = cx.new(|cx| CommandState::new(window, cx));
            self.palette = Some(state);
            // The palette's model is installed on its first render; focus it after that.
            cx.on_next_frame(window, |this, window, cx| {
                if let Some(p) = this.palette.clone() {
                    p.update(cx, |s, cx| s.focus(window, cx));
                }
            });
        }
        cx.notify();
    }

    pub fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.palette.take() {
            // Only take focus back if the palette still has it (a command may have moved it).
            if p.focus_handle(cx).contains_focused(window, cx) || window.focused(cx).is_none() {
                self.focus.focus(window, cx);
            }
        }
        cx.notify();
    }

    pub fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        self.opts.dark = !self.opts.dark;
        self.pal = Palette::of(self.opts.dark);
        theme::apply(self.opts.dark, cx);
        self.settings.theme = if self.opts.dark { Theme::Dark } else { Theme::Light };
        self.save_settings(cx);
        cx.notify();
    }

    pub fn toggle_offline(&mut self, cx: &mut Context<Self>) {
        match self.backend.as_demo() {
            Some(d) => {
                let off = !d.is_offline();
                d.set_offline(off);
                self.show_toast(if off { "Working offline (demo): changes will queue" } else { "Back online" }, false, cx);
                if !off {
                    let b = self.backend.clone();
                    cx.spawn(async move |_, _| { let _ = b.sync(None).await; }).detach();
                }
            }
            None => self.show_toast("Offline mode is only simulated in the demo", false, cx),
        }
    }

    pub fn sync_now(&mut self, cx: &mut Context<Self>) {
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let r = b.sync(None).await;
            let status = b.status().await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = r {
                    this.show_toast(format!("Sync failed: {e}"), true, cx);
                }
                if let Ok(s) = status {
                    this.status = s;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Escape: close the innermost thing.
    pub fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm_link.is_some() {
            self.confirm_link = None;
        } else if self.open.as_ref().is_some_and(|o| o.confirm_remote.is_some()) {
            if let Some(o) = self.open.as_mut() {
                o.confirm_remote = None;
            }
        } else if self.palette.is_some() {
            self.close_palette(window, cx);
        } else if self.shortcuts {
            self.shortcuts = false;
        } else if self.search_focused {
            self.focus.focus(window, cx);
        } else if !self.selected.is_empty() {
            self.selected.clear();
        } else if self.search_text.is_some() {
            self.search_text = None;
            self.search.update(cx, |s, cx| s.set_value("", window, cx));
            self.load_threads(false, cx);
        } else if self.open.is_some() {
            self.close_reader(window, cx);
        }
        cx.notify();
    }

    /// Wire every action to the root element (they work from anywhere; keys decide where they fire).
    pub fn actions(&self, root: Div, cx: &mut Context<Self>) -> Div {
        root.on_action(cx.listener(|this, _: &Next, w, cx| this.move_cursor(1, w, cx)))
            .on_action(cx.listener(|this, _: &Prev, w, cx| this.move_cursor(-1, w, cx)))
            .on_action(cx.listener(|this, _: &Open, w, cx| {
                if let Some(c) = this.cursor {
                    this.open_at(c, w, cx);
                } else if !this.threads.is_empty() {
                    this.open_at(0, w, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Back, w, cx| this.back(w, cx)))
            .on_action(cx.listener(|this, _: &Archive, w, cx| this.remove_targets(false, w, cx)))
            .on_action(cx.listener(|this, _: &Trash, w, cx| this.remove_targets(true, w, cx)))
            .on_action(cx.listener(|this, _: &ToggleStar, _, cx| this.toggle_star(cx)))
            .on_action(cx.listener(|this, _: &ToggleRead, _, cx| this.toggle_read(cx)))
            .on_action(cx.listener(|this, _: &Undo, w, cx| this.undo(w, cx)))
            .on_action(cx.listener(|this, _: &Reply, w, cx| this.open_composer(ComposeMode::Reply, w, cx)))
            .on_action(cx.listener(|this, _: &ReplyAll, w, cx| this.open_composer(ComposeMode::ReplyAll, w, cx)))
            .on_action(cx.listener(|this, _: &Forward, w, cx| this.open_composer(ComposeMode::Forward, w, cx)))
            .on_action(cx.listener(|this, _: &Compose, w, cx| this.open_composer(ComposeMode::New, w, cx)))
            .on_action(cx.listener(|this, _: &FocusSearch, w, cx| {
                this.search.update(cx, |s, cx| s.focus(w, cx));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &TogglePalette, w, cx| this.toggle_palette(w, cx)))
            .on_action(cx.listener(|this, _: &ToggleShortcuts, _, cx| {
                this.shortcuts = !this.shortcuts;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &GoInbox, w, cx| this.set_view(View::Unified(MailboxRole::Inbox), w, cx)))
            .on_action(cx.listener(|this, _: &GoImportant, w, cx| this.set_view(View::Category(Category::Important), w, cx)))
            .on_action(cx.listener(|this, a: &GoView, w, cx| this.go_view_key(&a.key, w, cx)))
            .on_action(cx.listener(|this, _: &GoStarred, w, cx| this.go_role(MailboxRole::Starred, w, cx)))
            .on_action(cx.listener(|this, _: &GoSent, w, cx| this.go_role(MailboxRole::Sent, w, cx)))
            .on_action(cx.listener(|this, _: &GoDrafts, w, cx| this.go_role(MailboxRole::Drafts, w, cx)))
            .on_action(cx.listener(|this, _: &GoArchive, w, cx| this.go_role(MailboxRole::Archive, w, cx)))
            .on_action(cx.listener(|this, _: &GoTrash, w, cx| this.go_role(MailboxRole::Trash, w, cx)))
            .on_action(cx.listener(|this, _: &ToggleSelect, _, cx| {
                if let Some(t) = this.cursor.and_then(|c| this.threads.get(c)) {
                    let id = key(t);
                    if !this.selected.remove(&id) {
                        this.selected.insert(id);
                    }
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                this.selected = this.threads.iter().map(key).collect();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ClearSelection, _, cx| {
                this.selected.clear();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleAiPanel, _, cx| this.toggle_ai_panel(cx)))
            .on_action(cx.listener(|this, _: &ToggleTheme, _, cx| this.toggle_theme(cx)))
            .on_action(cx.listener(|this, _: &SyncNow, _, cx| this.sync_now(cx)))
            .on_action(cx.listener(|this, _: &ToggleOffline, _, cx| this.toggle_offline(cx)))
            .on_action(cx.listener(|this, _: &LoadRemote, _, cx| this.load_remote_in_open(cx)))
            .on_action(cx.listener(|this, _: &ToggleDensity, _, cx| this.toggle_density(cx)))
            .on_action(cx.listener(|this, _: &ToggleNotifications, _, cx| this.toggle_notifications(false, cx)))
            .on_action(cx.listener(|this, _: &ToggleAnonNotifications, _, cx| this.toggle_notifications(true, cx)))
            .on_action(cx.listener(|this, _: &ForgetImageSenders, _, cx| {
                let n = this.settings.remote_allowed_senders.len();
                this.settings.remote_allowed_senders.clear();
                this.save_settings(cx);
                this.show_toast(format!("Forgot {n} sender{} allowed to show images", if n == 1 { "" } else { "s" }), false, cx);
            }))
            .on_action(cx.listener(|this, _: &ExpandAll, _, cx| this.expand_all(cx)))
            .on_action(cx.listener(|this, _: &Send, w, cx| this.send(w, cx)))
            .on_action(cx.listener(|this, _: &CloseComposer, w, cx| this.close_composer(w, cx)))
            .on_action(cx.listener(|this, _: &AttachFile, _, cx| this.attach_files(cx)))
            .on_action(cx.listener(|this, _: &ToggleCcBcc, _, cx| {
                if let Some(c) = this.composer.as_mut() {
                    c.show_cc_bcc = !c.show_cc_bcc;
                    cx.notify();
                }
            }))
    }

    /// Open or close the daemon menu (sync state, provider load, Pause/Resume, Quit); opening asks the
    /// daemon for its state again.
    pub fn toggle_daemon_menu(&mut self, cx: &mut Context<Self>) {
        self.daemon_menu = !self.daemon_menu;
        if self.daemon_menu {
            self.refresh_daemon_info(cx);
        }
        cx.notify();
    }

    pub fn refresh_daemon_info(&mut self, cx: &mut Context<Self>) {
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let info = b.daemon_info().await.map_err(|e| plain_error(&e));
            let _ = this.update(cx, |this, cx| {
                this.daemon = Some(info);
                cx.notify();
            });
        })
        .detach();
    }

    /// Pause (no provider call at all) or resume the daemon, from the menu.
    pub fn set_daemon_paused(&mut self, paused: bool, cx: &mut Context<Self>) {
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let r = b.set_paused(paused).await;
            let status = b.status().await;
            let _ = this.update(cx, |this, cx| {
                match r {
                    Ok(info) => {
                        this.daemon = Some(Ok(info));
                        this.show_toast(if paused { "Paused: nothing syncs until you resume" } else { "Resumed" }, false, cx);
                    }
                    Err(e) => this.show_toast(format!("Couldn't {}: {}", if paused { "pause" } else { "resume" }, plain_error(&e)), true, cx),
                }
                if let Ok(s) = status {
                    this.status = s;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Stop the daemon on this machine (never one on another machine: the menu has no Quit there).
    pub fn quit_daemon(&mut self, cx: &mut Context<Self>) {
        if !self.backend.can_quit_daemon() {
            return;
        }
        self.daemon_menu = false;
        let b = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let r = b.quit_daemon().await;
            let _ = this.update(cx, |this, cx| match r {
                Ok(()) => this.show_toast("lumbridge-maild stopped; start it again with lumbridge-maild or lbmail resume", false, cx),
                Err(e) => this.show_toast(format!("Couldn't stop lumbridge-maild: {}", plain_error(&e)), true, cx),
            });
        })
        .detach();
        cx.notify();
    }

    /// Open or save an attachment: fetched through the backend (copied here in pieces when the daemon
    /// is on another machine, with progress on its chip), then opened with the system's program or
    /// copied to Downloads. A file that could run code (`core::files::dangerous_to_open`) is never
    /// opened: only saved, so the person can look before running anything.
    pub fn file_action(&mut self, account: AccountId, message: MessageId, att: Attachment, what: FileAction, cx: &mut Context<Self>) {
        let name = lumbridge_mail_core::files::safe_file_name(&att.filename);
        if what == FileAction::Open
            && let Some(why) = lumbridge_mail_core::files::dangerous_to_open(&att.filename, &att.mime)
        {
            self.show_toast(format!("{name} was not opened: {why}. Save it and check it first."), true, cx);
            return;
        }
        let key: FileKey = (account.clone(), message.clone(), att.id.clone());
        match self.files.get(&key) {
            Some(FileState::Copying { .. }) => return,
            Some(FileState::Ready(path)) if path.is_file() => {
                let path = path.clone();
                self.file_ready(key, name, what, Ok(path), cx);
                return;
            }
            _ => {}
        }
        self.files.insert(key.clone(), FileState::Copying { done: 0, total: att.size });
        cx.notify();
        let (tx, rx) = async_channel::unbounded::<(u64, u64)>();
        let progress: Progress = Arc::new(move |done, total| {
            let _ = tx.try_send((done, total));
        });
        let b = self.backend.clone();
        let aref = AttachmentRef { account, message, attachment: att.id.clone() };
        let task = cx.background_spawn(async move { b.attachment_progress(aref, progress).await });
        let k = key.clone();
        cx.spawn(async move |this, cx| {
            while let Ok((done, total)) = rx.recv().await {
                let alive = this.update(cx, |this, cx| {
                    if let Some(FileState::Copying { .. }) = this.files.get(&k) {
                        this.files.insert(k.clone(), FileState::Copying { done, total });
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| this.file_ready(key, name, what, result, cx));
        })
        .detach();
    }

    fn file_ready(&mut self, key: FileKey, name: String, what: FileAction, result: Result<PathBuf, Error>, cx: &mut Context<Self>) {
        let path = match result {
            Ok(p) => p,
            Err(e) => {
                let why = plain_error(&e);
                self.files.insert(key, FileState::Failed(why.clone()));
                self.show_toast(format!("Couldn't {} {name}: {why}", if what == FileAction::Open { "open" } else { "save" }), true, cx);
                return;
            }
        };
        self.files.insert(key, FileState::Ready(path.clone()));
        cx.notify();
        match what {
            FileAction::Open => {
                if let Err(e) = self.opts.opener.open(&path) {
                    self.show_toast(format!("Couldn't open {name}: {e}"), true, cx);
                }
            }
            FileAction::Save => {
                let Some(dir) = self.opts.downloads.clone().or_else(files::downloads_dir) else {
                    self.show_toast(format!("Couldn't save {name}: no Downloads folder"), true, cx);
                    return;
                };
                let task = cx.background_spawn(async move { files::save_copy(&path, &dir, &name).map(|p| (p, name)) });
                cx.spawn(async move |this, cx| {
                    let saved = task.await;
                    let _ = this.update(cx, |this, cx| match saved {
                        Ok((p, _)) => {
                            let shown = p.display().to_string();
                            this.last_saved = Some(p);
                            this.show_toast(format!("Saved to {shown}"), false, cx);
                        }
                        Err(e) => this.show_toast(format!("Couldn't save: {e}"), true, cx),
                    });
                })
                .detach();
            }
        }
    }

    pub fn attach_files(&mut self, cx: &mut Context<Self>) {
        if self.composer.is_none() {
            return;
        }
        let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true,
                                                            prompt: Some("Attach".into()) });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                let _ = this.update(cx, |this, cx| {
                    if let Some(c) = this.composer.as_mut() {
                        for p in paths {
                            c.add_attachment(&p);
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

impl Focusable for MailApp {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Read a saved part, refusing anything over the image cap.
fn read_capped(path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    f.take(remote::MAX_IMAGE_BYTES + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > remote::MAX_IMAGE_BYTES {
        return Err("image too large".into());
    }
    Ok(bytes)
}

/// An account's sync problem as the status bar says it. `resync_needed` is not a failure the person
/// must fix: the daemon drops its cursor and syncs everything again.
pub fn status_error(label: &str, e: &Error) -> String {
    match e {
        Error::ResyncNeeded(_) => format!("{label}: syncing everything again (the server forgot where we were)"),
        Error::Auth(m) => format!("{label}: sign in again ({m})"),
        Error::Offline(m) => format!("{label}: offline, retrying ({m})"),
        Error::Refused(m) | Error::Other(m) | Error::Invalid(m) | Error::NotFound(m) | Error::Unsupported(m) | Error::NoAnswer(m) => {
            format!("{label}: {m}")
        }
    }
}

/// An error as a sentence for the window (no "offline:" prefix when the reason says it all).
pub fn plain_error(e: &Error) -> String {
    match e {
        Error::Offline(m) | Error::Other(m) => m.clone(),
        e => e.to_string(),
    }
}
