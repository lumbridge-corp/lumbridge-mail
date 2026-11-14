//! The contract every Lumbridge Mail crate builds against: the mail model, the provider trait, the
//! offline action queue's vocabulary, AI annotations, and the daemon's wire protocol names.
//!
//! Rules for this crate: no I/O, no async runtime, no SQL, no UI. Only types, traits and pure functions.
//! Changing a public type here touches every crate, so it goes through its own PR.

pub mod ai;
pub mod files;
pub mod model;
pub mod paths;
pub mod provider;
pub mod remote;
pub mod rpc;
pub mod safety;
pub mod threading;

pub use ai::{ActionItem, Category, DateMention, Extracted, Owner, Triage};
pub use model::*;
pub use provider::{
    Action, ApplyOutcome, BodyBundle, Capabilities, Delta, Provider, ProviderLoad, Support, SyncCursor,
};

/// One error type across the workspace, so the daemon can put any failure on the wire.
#[derive(Debug, Clone, thiserror::Error, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum Error {
    /// No network, the server is down, a timeout. The action queue retries these.
    #[error("offline: {0}")]
    Offline(String),
    /// Bad or expired credentials. Never retried; the user has to act.
    #[error("sign-in needed: {0}")]
    Auth(String),
    #[error("not found: {0}")]
    NotFound(String),
    /// The provider cannot do this at all (e.g. labels on a POP3 account).
    #[error("not supported: {0}")]
    Unsupported(String),
    /// A safety rule said no (see `safety`). Never retried, never worked around.
    #[error("refused: {0}")]
    Refused(String),
    #[error("bad input: {0}")]
    Invalid(String),
    /// The provider cannot use the cursor it was handed (unreadable, from another provider, or the
    /// server no longer has history that old and the provider cannot catch up by itself). The engine
    /// drops the stored cursor and calls `sync(None)`, which starts a full sync (`Delta::reset`).
    /// Never retried as is.
    #[error("full resync needed: {0}")]
    ResyncNeeded(String),
    /// The model answered, but with nothing usable (empty, unparseable, or a draft/answer that failed
    /// validation). Not the model being unreachable (that is `Offline`). Retrying may help; nothing
    /// was stored or changed.
    #[error("no usable answer from the model: {0}")]
    NoAnswer(String),
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Whether the offline queue should try this action again later.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Offline(_))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
