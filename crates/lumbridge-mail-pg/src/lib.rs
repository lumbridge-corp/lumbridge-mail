//! The one-way Postgres mirror of Lumbridge Mail.
//!
//! The daemon keeps SQLite as its working store; this crate writes what it holds into a Postgres
//! schema `mail` so every device and our agent tools can query, search and parse mail in one place.
//! Postgres is never read back by the daemon: it is a record others read.
//!
//! - [`migrate`]: the versioned SQL in `migrations/`, the roles (`mail_writer`, `mail_reader`), grants,
//!   and a printable plan (`lbmail pg migrate --dry-run`).
//! - [`rows`]: the row shapes, built from core types. Sensitive mail becomes metadata and its one line.
//! - [`Writer`]: idempotent upserts and deletes of a batch of [`Change`]s in one transaction.
//! - [`connect`]: reading a DSN from a private file, and TLS through rustls.
//! - [`work`]: the embedding and parsing workers' side (jobs, embeddings, facts, stats) and the
//!   read side (`mail.search` with a query vector, the parsed facts).
//!
//! What is mirrored and what never is (anon accounts, sensitive content) is decided by the daemon's
//! mirror module; the schema refuses both as a second wall (see `migrations/0001_init.sql`).

pub mod connect;
pub mod migrate;
pub mod rows;
#[doc(hidden)]
pub mod testing;
pub mod work;
mod writer;

pub use connect::{ConnectOptions, read_dsn_file};
pub use migrate::{EmbeddingType, MigrateOptions, Plan, Report};
pub use rows::{
    AccountRow, AttachmentRow, BodyChange, BodyRow, Change, MailboxRow, MessageRow, TriageRow,
};
/// The Postgres client, for callers that read the mirror (tests, tools).
pub use tokio_postgres;
pub use writer::{Counts, Writer, counts};

/// The schema version this build writes. The writer refuses an older database ("run `lbmail pg
/// migrate`") and works with a newer one as long as it only added things.
pub const SCHEMA_VERSION: i32 = migrate::LATEST;

use lumbridge_mail_core::Error;

/// Postgres errors on the way out: a server-side error (a refused row, a missing table) is `Other`;
/// anything else (cannot connect, connection lost, timeout) is `Offline`, which the mirror retries.
pub(crate) fn pg_err(e: tokio_postgres::Error) -> Error {
    match e.as_db_error() {
        Some(db) => {
            let detail = db.detail().map(|d| format!(" ({d})")).unwrap_or_default();
            Error::Other(format!(
                "postgres: {} [{}]{detail}",
                db.message(),
                db.code().code()
            ))
        }
        None => Error::Offline(format!("postgres: {e}")),
    }
}

/// True for errors that say Postgres refused something (as opposed to being unreachable).
pub fn is_refusal(e: &Error) -> bool {
    !matches!(e, Error::Offline(_))
}
