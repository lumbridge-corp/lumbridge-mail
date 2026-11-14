//! What a mail backend has to do. One `Provider` value serves one account.
//!
//! Providers hold no local state beyond their connection: the sync engine owns the store and the
//! cursor, and calls these methods. Every method may return `Error::Offline`; the engine retries.

use crate::model::*;
use crate::Result;
use serde::{Deserialize, Serialize};

/// Opaque to everyone but the provider that issued it (a Gmail historyId, IMAP UIDVALIDITY+MODSEQ per
/// folder, a JMAP state string). The engine stores it and hands it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SyncCursor(pub String);

/// What changed on the server since a cursor.
///
/// **Cursors the provider cannot use.** `sync(Some(cursor))` returns `Error::ResyncNeeded` when the
/// cursor is unreadable or too old to resume from and the provider cannot recover by itself. The engine
/// then drops the stored cursor and calls `sync(None)`. A provider that can recover on its own (Gmail
/// history expired: start a full sync) returns that full sync's first page with `reset: true` instead.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Delta {
    /// The full current mailbox list (small; sent whole), or None when the provider did not look or
    /// nothing changed: the engine keeps what it has. `Some(vec![])` means the account has no mailboxes.
    #[serde(default)]
    pub mailboxes: Option<Vec<Mailbox>>,
    /// New messages and messages whose flags or mailboxes changed.
    pub upserted: Vec<MessageMeta>,
    /// Messages that no longer exist on the server.
    pub removed: Vec<MessageId>,
    /// Hand this back next time. None only if the provider has nothing to resume from.
    pub cursor: Option<SyncCursor>,
    /// True when the provider stopped early (page limit): call `sync` again right away.
    pub more: bool,
    /// This page belongs to a fresh full sync: every message the account still has is reported across
    /// its pages. Set on every page of that sync (the first included). When the last page (`more:
    /// false`) is applied, the engine drops every stored message of the account that no page of this
    /// sync reported. Providers set it on `sync(None)` and on any full resync they start themselves.
    #[serde(default)]
    pub reset: bool,
    /// With `reset`: the full sync lists only mail dated at or after this time (Unix seconds), a window
    /// (the last N days or M messages), not everything. When its last page is applied, the store drops
    /// only unreported messages dated at or after the last page's floor; older stored mail is kept. A
    /// provider must never set it below what the run really covered (higher is always safe: it only
    /// deletes less). None: the run covered all mail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_floor: Option<i64>,
    /// The messages here are older mail being filled in behind a windowed sync (a backfill page):
    /// they are stored like any other, but never announced as new mail.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub backfill: bool,
}

/// A body and whatever else the same server call brought back. Gmail through gog fetches a whole
/// thread in one call, so the other messages' bodies (and their full metadata) come for free; the
/// daemon caches them instead of asking again.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BodyBundle {
    pub body: Body,
    /// Bodies of other messages (usually the rest of the thread).
    pub others: Vec<(MessageId, Body)>,
    /// Full metadata for messages the call returned (may include the one asked for). Stored like a
    /// non-reset sync delta, without touching the sync cursor.
    pub metas: Vec<MessageMeta>,
}

/// How hard a provider is working its backend right now, for `status` and the budget log line.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProviderLoad {
    /// What the budget is shared by: an ssh host (`ssh:mailhost`) or `local`.
    pub scope: String,
    /// Processes (gog) running now for this scope.
    pub in_flight: u32,
    /// Process starts in the last 60 seconds, and since the daemon started.
    pub starts_last_min: u32,
    pub starts_total: u64,
    /// Unix time until which new starts wait after transport failures (backoff), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_until: Option<i64>,
    /// Unix time until which the circuit breaker refuses calls (too many transport failures), if open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breaker_open_until: Option<i64>,
    /// The limits in force: concurrent processes for the scope, and starts per minute.
    pub max_in_flight: u32,
    pub max_starts_per_min: u32,
}

impl ProviderLoad {
    /// Which provider's budget this is, from the scope: `zoho:<sign-in>` is Zoho (HTTP calls); `ssh:<host>`
    /// and `local` are gog (process starts).
    pub fn provider(&self) -> &'static str {
        if self.scope.starts_with("zoho:") { "zoho" } else { "gog" }
    }

    /// One line for `lbmail status`, the daemon's budget log and the app's status menu, in the provider's
    /// own words: `zoho: 12 calls in the last minute (limit 60/min), 0 of 2 in flight, 340 since start
    /// (sign-in tokens.json)`, or `gog ssh:mailhost: 3 process starts in the last minute (limit 120/min),
    /// 1 of 2 running, 57 since start`; then any backoff or open breaker. `now` is Unix seconds.
    pub fn describe(&self, now: i64) -> String {
        let mut s = match self.scope.strip_prefix("zoho:") {
            Some(signin) => {
                let mut s = format!(
                    "zoho: {} calls in the last minute (limit {}/min), {} of {} in flight, {} since start",
                    self.starts_last_min, self.max_starts_per_min, self.in_flight, self.max_in_flight, self.starts_total
                );
                if !signin.is_empty() {
                    s.push_str(&format!(" (sign-in {signin})"));
                }
                s
            }
            None => format!(
                "gog {}: {} process starts in the last minute (limit {}/min), {} of {} running, {} since start",
                self.scope, self.starts_last_min, self.max_starts_per_min, self.in_flight, self.max_in_flight, self.starts_total
            ),
        };
        let left = |t: i64| (t - now).max(0);
        if let Some(t) = self.breaker_open_until {
            s.push_str(&format!("; circuit breaker open for {} s more", left(t)));
        } else if let Some(t) = self.backoff_until {
            s.push_str(&format!("; backing off for {} s after connection failures", left(t)));
        }
        s
    }
}

/// What the server said about one applied action.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ApplyOutcome {
    /// `SaveDraft`: the provider's id for the saved draft (`Draft::server_id` from then on).
    /// `Send`: the sent message's id, when the provider learns it. None otherwise or when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
}

impl ApplyOutcome {
    pub fn with_id(id: impl Into<String>) -> Self {
        Self { server_id: Some(id.into()) }
    }
}

/// A change the user (or the AI, with the user's approval) made. Applied to the local store at once,
/// queued durably, then sent to the provider in order. Every action must be safe to send twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Action {
    SetSeen { ids: Vec<MessageId>, seen: bool },
    SetFlagged { ids: Vec<MessageId>, flagged: bool },
    /// Remove from `from` and add to `to`. For Gmail this is a label change. With `from: None` it adds
    /// to `to` and removes nothing (for Gmail: add a label; for IMAP, where a message is in one folder,
    /// the message moves out of whatever folder it is in).
    Move { ids: Vec<MessageId>, from: Option<MailboxId>, to: MailboxId },
    /// Out of the inbox, kept in the archive / All Mail.
    Archive { ids: Vec<MessageId> },
    Trash { ids: Vec<MessageId> },
    /// Create or replace the draft on the server.
    SaveDraft { draft: Draft },
    /// `server_id` is the provider's draft id when known (the store fills it in from the saved draft).
    DeleteDraft {
        local_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server_id: Option<String>,
    },
    /// Send. Only ever queued by an explicit user action (see `safety::SEND_NEEDS_A_PERSON`).
    Send { draft: Draft },
}

/// Where an action takes effect for one account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    /// On the server, like any mail account.
    Server,
    /// Only in the local store: `apply` accepts it and changes nothing on the server, so it is not seen on
    /// other devices and a full resync may undo it. Clients may say "on this device only".
    Local,
    /// Not at all: the daemon refuses it (`Error::Unsupported`) before it is queued. Clients hide it.
    Unsupported,
}

/// What an account can do, so clients can hide or label actions. [`Provider::capabilities`] has a
/// default (everything on the server), which fits IMAP, gog and the demo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// `Action::Send` works (still only from a person).
    pub send: bool,
    /// `SaveDraft` / `DeleteDraft`.
    pub drafts: Support,
    /// `SetSeen`.
    pub seen: Support,
    /// `SetFlagged`.
    pub flagged: Support,
    pub archive: Support,
    pub trash: Support,
    /// `Move` between mailboxes.
    pub moves: Support,
    /// `fetch_raw` can return the message source.
    pub raw: bool,
    /// Something the person should know, in plain words (e.g. "Trash on a scheduled email cancels it").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            send: true,
            drafts: Support::Server,
            seen: Support::Server,
            flagged: Support::Server,
            archive: Support::Server,
            trash: Support::Server,
            moves: Support::Server,
            raw: true,
            note: None,
        }
    }
}

impl Capabilities {
    /// Where `action` takes effect (`Send` is `Server` when `send`, else `Unsupported`).
    pub fn support_for(&self, action: &Action) -> Support {
        match action {
            Action::SetSeen { .. } => self.seen,
            Action::SetFlagged { .. } => self.flagged,
            Action::Move { .. } => self.moves,
            Action::Archive { .. } => self.archive,
            Action::Trash { .. } => self.trash,
            Action::SaveDraft { .. } | Action::DeleteDraft { .. } => self.drafts,
            Action::Send { .. } => {
                if self.send {
                    Support::Server
                } else {
                    Support::Unsupported
                }
            }
        }
    }
}

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn account(&self) -> &AccountId;

    /// What this account can do and where it takes effect. The daemon refuses actions that are
    /// `Support::Unsupported` before they are queued and tells clients (`accounts.capabilities`).
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    /// Changes since `cursor`; with None, the first page of a full sync (newest mail first, `reset: true`).
    /// `Error::ResyncNeeded` if the cursor cannot be used (see [`Delta`]).
    async fn sync(&self, cursor: Option<&SyncCursor>) -> Result<Delta>;

    async fn fetch_body(&self, id: &MessageId) -> Result<Body>;

    /// The whole message as RFC 5322 bytes (for "view source", "save as .eml", and re-parsing).
    async fn fetch_raw(&self, id: &MessageId) -> Result<Vec<u8>>;

    async fn fetch_attachment(&self, message: &MessageId, attachment: &AttachmentId) -> Result<Vec<u8>>;

    /// Apply one queued action on the server. Must be idempotent. Drafts carry `server_id` when the
    /// store knows it; the outcome reports the id of a saved draft or sent message.
    async fn apply(&self, action: &Action) -> Result<ApplyOutcome>;

    /// Block until the server says something changed (IMAP IDLE, JMAP push) or `timeout_s` passes.
    /// Providers that cannot wait return `Ok(())` after sleeping; the engine then calls `sync`.
    async fn wait_for_change(&self, timeout_s: u64) -> Result<()>;

    /// The body of `id` and whatever else the same call returns ([`BodyBundle`]). `thread` is the
    /// message's thread when the caller knows it (lets a provider fetch the thread in one call). The
    /// default fetches just the body.
    async fn fetch_body_bundle(&self, id: &MessageId, thread: Option<&ThreadId>) -> Result<BodyBundle> {
        let _ = thread;
        Ok(BodyBundle { body: self.fetch_body(id).await?, ..Default::default() })
    }

    /// Start (or restart) filling in mail older than what a windowed sync fetched, slowly, from the
    /// next `sync` calls on, back to `since` (Unix seconds; None = everything). Pages come back as
    /// `Delta { backfill: true }`. Providers that always sync everything return `Unsupported`.
    async fn request_backfill(&self, since: Option<i64>) -> Result<()> {
        let _ = since;
        Err(crate::Error::Unsupported("this provider has no backfill".into()))
    }

    /// The provider's current load on its backend, if it keeps such numbers (gog does).
    fn load(&self) -> Option<ProviderLoad> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_provider_names_its_own_load() {
        let zoho = ProviderLoad {
            scope: "zoho:tokens.json".into(),
            in_flight: 0,
            starts_last_min: 12,
            starts_total: 340,
            max_in_flight: 2,
            max_starts_per_min: 60,
            ..ProviderLoad::default()
        };
        assert_eq!(zoho.provider(), "zoho");
        assert_eq!(
            zoho.describe(0),
            "zoho: 12 calls in the last minute (limit 60/min), 0 of 2 in flight, 340 since start (sign-in tokens.json)"
        );
        let gog = ProviderLoad { scope: "ssh:gog-host".into(), in_flight: 1, starts_last_min: 3, starts_total: 57, max_in_flight: 2,
                                 max_starts_per_min: 120, breaker_open_until: Some(130), ..ProviderLoad::default() };
        assert_eq!(gog.provider(), "gog");
        assert_eq!(
            gog.describe(100),
            "gog ssh:gog-host: 3 process starts in the last minute (limit 120/min), 1 of 2 running, 57 since start; circuit breaker open for 30 s more"
        );
        assert!(!zoho.describe(0).contains("process"));
    }

    #[test]
    fn capabilities_default_to_the_server_and_map_actions() {
        let c = Capabilities::default();
        assert_eq!(c.support_for(&Action::Trash { ids: vec![] }), Support::Server);
        let local = Capabilities { send: false, seen: Support::Local, ..Capabilities::default() };
        assert_eq!(local.support_for(&Action::SetSeen { ids: vec![], seen: true }), Support::Local);
        assert_eq!(local.support_for(&Action::Send { draft: Draft::default() }), Support::Unsupported);
        let j = serde_json::to_value(&local).unwrap();
        assert_eq!(j["seen"], "local");
        assert!(j.get("note").is_none());
        assert_eq!(serde_json::from_value::<Capabilities>(j).unwrap(), local);
    }
}
