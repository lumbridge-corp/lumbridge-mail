//! Lumbridge Mail's Resend backend: a core `Provider` over Resend's HTTP API (resend.com) for one
//! account (a sending domain, or some addresses on it).
//!
//! - Inbox: mail received on the domain (`GET /emails/receiving`), when receiving is turned on.
//! - Sent: mail the team sent from the account's addresses (`GET /emails`), with Resend's delivery state
//!   (`last_event`) in the list row and in [`lumbridge_mail_core::DELIVERY_HEADER`]; Bounced is a view.
//! - Scheduled: scheduled emails; Trash cancels them (`POST /emails/{id}/cancel`).
//! - Send: `POST /emails` with an idempotency key from the draft, so a retry never sends twice.
//! - Read state, stars, archive and drafts are local only (Resend keeps none of them); see
//!   [`Provider::capabilities`](lumbridge_mail_core::Provider::capabilities).
//!
//! [`fake`] is an in-process fake of the API for tests. See README.md for every endpoint used.

pub mod api;
mod client;
pub mod config;
pub mod convert;
mod cursor;
pub mod fake;
mod provider;
pub mod send;

pub use api::{ApiKeyInfo, DomainInfo};
pub use config::{AddressPattern, ApiKey, DEFAULT_BASE_URL, ResendConfig};
pub use provider::ResendProvider;
