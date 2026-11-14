//! What the app talks to. Everything the window shows comes through [`Backend`], so the same UI runs on
//! made-up mail ([`demo::Demo`], `--demo`) and on the daemon's socket ([`rpc::Rpc`]).
//!
//! The trait mirrors `lumbridge_mail_core::rpc::method` one-to-one: each method is one request on the
//! wire. Futures are `Send` and executor-agnostic (no tokio), so GPUI's foreground or background
//! executor can drive them.

pub mod demo;
pub mod rpc;
pub mod ssh;

use async_trait::async_trait;
use lumbridge_mail_core::rpc::{AccountCapabilities, AccountStatus, Act, Answer, AttachmentRef, CategoryCount, CountsRequest, DaemonInfo,
                               Discovered, DraftRequest, Event, Hello, MessageRef, NewAccount, ThreadPage, ThreadRef};
use lumbridge_mail_core::{Account, AccountId, ActionItem, Address, Body, Draft, Error, Extracted, Mailbox, MessageMeta, Owner,
                          Query, Result, Triage};
use std::path::PathBuf;
use std::sync::Arc;

/// Told (bytes so far, bytes in all) while an attachment is copied.
pub type Progress = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// Why there is no connection to the daemon, in words the window can show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// Nothing listens at the socket and this app does not start a daemon there.
    NoDaemon { socket: PathBuf },
    /// Nothing listens and `lumbridge-maild` is neither next to the app nor on `PATH`.
    NotInstalled { socket: PathBuf },
    /// We started `lumbridge-maild` but it exited or never opened its socket.
    StartFailed { detail: String },
    /// The socket is there but refuses us (permissions, a stale file, ...).
    Unreachable { socket: PathBuf, detail: String },
    /// We were connected and the daemon went away (it is restarting, or it crashed).
    Lost { detail: String },
    /// Nothing listens, and the daemon was stopped or paused on purpose (`lbmail quit`, `lbmail pause`):
    /// this app does not start it (see `core::paths::stopped_marker_for`).
    Stopped { socket: PathBuf, paused: bool },
    /// The daemon is on another machine and ssh to it failed, or it is not running there.
    Remote { host: String, detail: String },
}

impl Problem {
    pub fn message(&self) -> String {
        match self {
            Problem::NoDaemon { socket } => format!("lumbridge-maild is not running (nothing at {})", socket.display()),
            Problem::NotInstalled { socket } => format!(
                "lumbridge-maild is not running and was not found next to the app or on PATH (socket {})",
                socket.display()
            ),
            Problem::StartFailed { detail } => format!("lumbridge-maild did not start: {detail}"),
            Problem::Unreachable { socket, detail } => format!("can't reach lumbridge-maild at {}: {detail}", socket.display()),
            Problem::Lost { detail } => format!("lost the connection to lumbridge-maild: {detail}"),
            Problem::Stopped { paused: false, .. } => "lumbridge-maild was stopped on purpose (lbmail quit), so it is not started again".into(),
            Problem::Stopped { paused: true, .. } => "lumbridge-maild is paused (lbmail pause) and not running, so it is not started again".into(),
            Problem::Remote { host, detail } => format!("can't reach lumbridge-maild on {host}: {detail}"),
        }
    }
}

/// The connection to the daemon, as the status bar shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    Connecting,
    /// We started the daemon and wait for its socket.
    Starting { program: String },
    Connected,
    /// Not connected; the next try is in about `retry_in_s` seconds (or at the next request).
    Down { problem: Problem, retry_in_s: u64 },
}

#[async_trait]
pub trait Backend: Send + Sync + 'static {
    /// A short name for the status bar ("Demo", "maild").
    fn name(&self) -> &str;

    async fn hello(&self) -> Result<Hello>;
    async fn accounts(&self) -> Result<Vec<Account>>;
    async fn mailboxes(&self, account: Option<AccountId>) -> Result<Vec<Mailbox>>;
    async fn threads(&self, query: Query) -> Result<ThreadPage>;
    /// Oldest first.
    async fn thread(&self, thread: ThreadRef) -> Result<Vec<MessageMeta>>;
    async fn body(&self, message: MessageRef) -> Result<Body>;
    /// Save an attachment (or an inline `cid:` part) to a local file and say where (the daemon's cache).
    async fn attachment(&self, attachment: AttachmentRef) -> Result<PathBuf> {
        Err(Error::Unsupported(format!("attachments ({})", attachment.attachment)))
    }
    /// Like [`Backend::attachment`], telling `progress` how far it got when the bytes have to be copied
    /// here (a daemon on another machine). The default reports nothing.
    async fn attachment_progress(&self, attachment: AttachmentRef, progress: Progress) -> Result<PathBuf> {
        let _ = progress;
        self.attachment(attachment).await
    }
    /// Queues one action (applied locally at once, sent in the background) and returns its queue id.
    /// Whether it went out yet is in `status()` / `Event::Status` (`queued`, `online`), not here.
    async fn act(&self, act: Act) -> Result<u64>;
    async fn sync(&self, account: Option<AccountId>) -> Result<()>;
    async fn status(&self) -> Result<Vec<AccountStatus>>;
    async fn triage(&self, thread: ThreadRef) -> Result<Triage>;
    async fn summarise(&self, thread: ThreadRef) -> Result<String>;
    /// A draft to edit. Never sent by anyone but the person pressing Send.
    async fn draft(&self, request: DraftRequest) -> Result<Draft>;
    async fn ask(&self, question: String, accounts: Option<Vec<AccountId>>) -> Result<Answer>;

    /// Action items and dates in a thread (`ai.extract`). The default, for backends without a model,
    /// derives them from the thread's triage: a reply wanted, a date asked for.
    async fn extract(&self, thread: ThreadRef, triage: Option<Triage>) -> Result<Extracted> {
        let _ = thread;
        Ok(Extracted { items: items_from_triage(triage.as_ref()), dates: Vec::new(), by: "rules".into() })
    }

    /// Threads (and unread threads) per AI category (`counts`), for the sidebar's badges.
    async fn counts(&self, request: CountsRequest) -> Result<Vec<CategoryCount>> {
        let _ = request;
        Ok(Vec::new())
    }

    /// Add an account (`accounts.add`; the first-run screen). Only the daemon can.
    async fn add_account(&self, account: NewAccount) -> Result<Account> {
        Err(Error::Unsupported(format!("adding accounts here ({})", account.email)))
    }

    /// What each account can do and where (`accounts.capabilities`). None listed = everything on the
    /// server (the demo).
    async fn capabilities(&self) -> Result<Vec<AccountCapabilities>> {
        Ok(Vec::new())
    }

    /// The daemon's state and its providers' load (`daemon.info`). The demo has no daemon.
    async fn daemon_info(&self) -> Result<DaemonInfo> {
        Err(Error::Unsupported("no daemon (demo data)".into()))
    }

    /// `daemon.pause` (true) or `daemon.resume` (false): no provider call at all while paused.
    async fn set_paused(&self, paused: bool) -> Result<DaemonInfo> {
        let _ = paused;
        Err(Error::Unsupported("no daemon (demo data)".into()))
    }

    /// Whether this app may stop the daemon (`daemon.quit`): only one on this machine. A daemon on
    /// another machine belongs to that machine's service manager.
    fn can_quit_daemon(&self) -> bool {
        false
    }

    /// `daemon.quit`: the daemon writes its "stopped" marker and exits (nothing starts it again until it
    /// is started by hand).
    async fn quit_daemon(&self) -> Result<()> {
        Err(Error::Unsupported("no daemon to stop here".into()))
    }

    /// Servers for an address (`accounts.discover`).
    async fn discover(&self, email: String) -> Result<Discovered> {
        Err(Error::Unsupported(format!("discovery ({email})")))
    }

    /// The daemon accepted us as a person (we may send). The demo always does.
    fn is_person(&self) -> bool {
        true
    }

    /// People to suggest as recipients when writing from `account`. Core has no method for this yet, so
    /// the default is none and the app learns from the mail it shows (see `contacts.rs`).
    async fn contacts(&self, account: AccountId) -> Result<Vec<Address>> {
        let _ = account;
        Ok(Vec::new())
    }

    /// The daemon link's state changes (None for backends without one, like the demo).
    fn link(&self) -> Option<async_channel::Receiver<LinkState>> {
        None
    }

    fn link_state(&self) -> Option<LinkState> {
        None
    }

    /// The demo's extra controls (go offline, ...), when this is the demo.
    fn as_demo(&self) -> Option<&demo::Demo> {
        None
    }

    /// Daemon notifications. Each call returns a receiver of the same stream (clone of one channel).
    fn events(&self) -> async_channel::Receiver<Event>;
}

pub fn items_from_triage(triage: Option<&Triage>) -> Vec<ActionItem> {
    let Some(t) = triage else { return Vec::new() };
    let mut items = Vec::new();
    if t.needs_reply {
        items.push(ActionItem { text: "Reply".into(), owner: Owner::Me, due: t.due });
    } else if let Some(due) = t.due {
        items.push(ActionItem { text: "Deal with this".into(), owner: Owner::Me, due: Some(due) });
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumbridge_mail_core::Category;

    #[test]
    fn triage_becomes_action_items() {
        let mut t = Triage { category: Category::Personal, priority: 50, summary: "x".into(), needs_reply: true,
                             due: Some(10), by: "rules".into(), at: 0, suspicious: false };
        assert_eq!(items_from_triage(Some(&t)), vec![ActionItem { text: "Reply".into(), owner: Owner::Me, due: Some(10) }]);
        t.needs_reply = false;
        assert_eq!(items_from_triage(Some(&t))[0].due, Some(10));
        t.due = None;
        assert!(items_from_triage(Some(&t)).is_empty());
        assert!(items_from_triage(None).is_empty());
    }
}
