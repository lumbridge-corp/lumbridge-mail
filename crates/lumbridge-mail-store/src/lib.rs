//! The local mail store for Lumbridge Mail: one SQLite file (WAL) holding accounts, mailboxes, message
//! metadata, cached bodies, the attachments index, drafts, AI triage, sync cursors, the durable action
//! queue and an FTS5 search index.
//!
//! The API is synchronous (rusqlite). The daemon calls it from blocking tasks
//! (`tokio::task::spawn_blocking`) and owns one [`Store`] per thread; open more than one on the same file
//! for parallel readers (WAL lets readers run while one writer writes).
//!
//! What the store guarantees:
//! - The anon wall (CONTRIBUTING.md rule 4): a [`Query`] with `accounts: None` never returns anon accounts'
//!   threads, whether listing the unified inbox or searching. Anon accounts appear only when named.
//! - Local first: [`Store::enqueue`] writes the action and applies it to the local view in one
//!   transaction, and later syncs keep pending actions applied on top of what the server says until the
//!   action is done.
//! - No secrets: there is no column for a password or token anywhere in the schema.
//!
//! Main entry points: [`Store::open`], [`Store::upsert_account`], [`Store::apply_delta`],
//! [`Store::list_threads`], [`Store::thread`], [`Store::put_body`], [`Store::put_triage`],
//! [`Store::save_draft`], [`Store::enqueue`] / [`Store::next_action`] / [`Store::action_done`] /
//! [`Store::action_failed`].
//!
//! The background body fill's work list (schema V5): [`Store::body_fill_next`], [`Store::body_fill_passed`],
//! [`Store::body_fill_unfetchable`], [`Store::body_fill_rewind`], [`Store::body_fill_progress`] (see `fill.rs`).
//!
//! The Postgres mirror's outbox (schema V4): [`Store::set_mirror_enabled`], [`Store::mirror_take`],
//! [`Store::mirror_done`], [`Store::mirror_backfill`] (see `mirror.rs`).

mod accounts;
mod bodies;
mod drafts;
mod fill;
mod list;
mod mirror;
mod queue;
mod schema;
mod search;
mod sync;
mod threading;

use lumbridge_mail_core::{Error, MailboxRole, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub use drafts::{DraftState, StoredDraft};
pub use fill::{FillItem, FillProgress};
pub use lumbridge_mail_core::rpc::{Caller, ThreadPage};
pub use mirror::{MAX_ATTEMPTS as MIRROR_MAX_ATTEMPTS, MirrorState, OutboxItem, OutboxKind};
pub use queue::{ActionId, ActionState, Enqueued, FailOutcome, QueuedAction, RetryPolicy};
pub use search::{Alt, SearchQuery, Term, parse_search, parse_search_at};
pub use sync::Applied;
pub use threading::SUBJECT_WINDOW_S;

/// The current schema version (`PRAGMA user_version`).
pub const SCHEMA_VERSION: u32 = schema::VERSION;

/// Page size when a [`Query`](lumbridge_mail_core::Query) asks for `limit: 0`.
pub const DEFAULT_PAGE: u32 = 50;
/// Largest page [`Store::list_threads`] returns, whatever the query asks.
pub const MAX_PAGE: u32 = 500;

/// The mail database. Not `Sync`: give each thread its own (see the crate docs).
pub struct Store {
    conn: Connection,
    retry: RetryPolicy,
}

impl Store {
    /// Opens (creating if needed) the database at `path`, switches it to WAL and runs migrations.
    /// The file is made private to the user (0600) on Unix.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| Error::Other(format!("store: {}: {e}", dir.display())))?;
        }
        let conn = Connection::open(path).db()?;
        private(path);
        Self::init(conn, true)
    }

    /// A throwaway database in memory, for tests and demos.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory().db()?, false)
    }

    fn init(mut conn: Connection, wal: bool) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5)).db()?;
        // Every transaction here writes, and the daemon has more than one connection writing (the
        // mirror's own). In WAL mode a deferred transaction that reads first and then writes cannot
        // wait for another writer that committed meanwhile: SQLite answers SQLITE_BUSY at once and
        // the busy timeout never applies ("database is locked" in the middle of a sync page). Taking
        // the write lock at BEGIN (IMMEDIATE) makes it wait its turn instead.
        conn.set_transaction_behavior(rusqlite::TransactionBehavior::Immediate);
        if wal {
            let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).db()?;
        }
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY;")
            .db()?;
        schema::migrate(&conn)?;
        Ok(Self { conn, retry: RetryPolicy::default() })
    }

    /// Where the daemon keeps the database: `$LUMBRIDGE_MAIL_HOME/data/mail.db` when that is set,
    /// else `$XDG_DATA_HOME/lumbridge-mail/mail.db` (default `~/.local/share/lumbridge-mail/mail.db`).
    pub fn default_path() -> PathBuf {
        if let Some(home) = std::env::var_os("LUMBRIDGE_MAIL_HOME") {
            return PathBuf::from(home).join("data").join("mail.db");
        }
        let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local").join("share")
        });
        data.join("lumbridge-mail").join("mail.db")
    }

    /// The schema version of the open database.
    pub fn schema_version(&self) -> Result<u32> {
        self.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).db()
    }

    /// How failed actions are retried (see [`RetryPolicy`]).
    pub fn set_retry_policy(&mut self, policy: RetryPolicy) {
        self.retry = policy;
    }
}

#[cfg(unix)]
fn private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn private(_path: &Path) {}

/// rusqlite and serde errors become `Error::Other` on the way out, so the daemon can put them on the wire.
pub(crate) trait Db<T> {
    fn db(self) -> Result<T>;
}

impl<T> Db<T> for rusqlite::Result<T> {
    fn db(self) -> Result<T> {
        self.map_err(|e| Error::Other(format!("store: {e}")))
    }
}

impl<T> Db<T> for serde_json::Result<T> {
    fn db(self) -> Result<T> {
        self.map_err(|e| Error::Other(format!("store: bad stored json: {e}")))
    }
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub(crate) fn role_str(role: MailboxRole) -> &'static str {
    role.as_str()
}

pub(crate) fn parse_role(s: &str) -> Option<MailboxRole> {
    MailboxRole::parse(s)
}

/// A category's wire name (`newsletter`, ...), as stored in `threads.category`.
pub(crate) fn category_str(c: lumbridge_mail_core::Category) -> String {
    c.as_str().to_owned()
}

pub(crate) fn to_json<T: serde::Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).db()
}

pub(crate) fn from_json<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(s).db()
}
