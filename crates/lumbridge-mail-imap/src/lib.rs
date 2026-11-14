//! Lumbridge Mail's IMAP + SMTP provider.
//!
//! `ImapProvider` implements `lumbridge_mail_core::Provider` for one account: IMAP for reading (sync with
//! UIDVALIDITY / UIDNEXT, CONDSTORE and QRESYNC where offered, a bounded flag diff where not, IDLE for
//! `wait_for_change`) and SMTP for sending (lettre). It holds connections, never mail or credentials on
//! disk. See the crate README for the cursor format and the guarantees each action gives.
//!
//! ```no_run
//! use lumbridge_mail_core::{AccountId, Provider};
//! use lumbridge_mail_imap::{Credentials, Identity, ImapAccount, ImapProvider, discover};
//! # async fn run() -> lumbridge_mail_core::Result<()> {
//! let found = discover::builtin("ada@fastmail.com").expect("known provider");
//! let account = ImapAccount::new(
//!     AccountId::new("personal-fastmail"),
//!     Identity { email: "ada@fastmail.com".into(), name: "Ada".into() },
//!     found.imap,
//!     Some(found.smtp),
//!     Credentials::Password { username: found.username, password: "from the keyring".into() },
//! );
//! let provider = ImapProvider::new(account);
//! let first = provider.sync(None).await?;
//! # let _ = first; Ok(()) }
//! ```

mod config;
mod conn;
mod cursor;
pub mod discover;
mod folders;
mod ids;
mod parse;
mod provider;
mod send;
mod session;
mod uidset;

pub use config::{Credentials, Identity, ImapAccount, Options, Security, Server};
pub use folders::{decode_mutf7, display_name, role_for};
pub use ids::{message_id, normalise_subject, outgoing_message_id, thread_id};
pub use provider::ImapProvider;
