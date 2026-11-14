//! Lumbridge Mail's index: what makes mail in Postgres searchable by meaning and parsed into
//! structured facts. No I/O of its own except the embedding endpoint ([`embed::OpenAiEmbedder`]);
//! the daemon's workers (`lumbridge-maild`, `pgwork`) read and write Postgres.
//!
//! - [`chunk`]: the text that is embedded: subject, sender name and the new part of the body (no
//!   quoted history), cut into overlapping chunks, at most a few per message, each with a hash so
//!   unchanged mail is never embedded twice.
//! - [`embed`]: an [`embed::Embedder`] over an OpenAI-compatible `/v1/embeddings` endpoint, with the
//!   configured headers, and a [`pace::Pacer`] so a shared GPU box is not hammered.
//! - [`structured`]: schema.org objects (JSON-LD, microdata) out of received HTML, size-capped. The
//!   daemon stores them with the body; they are data for the rules, never shown.
//! - [`rules`]: deterministic extractors (schema.org orders, flights, reservations, parcels, events;
//!   List-Unsubscribe; calendar invites; carrier tracking numbers). No model.
//! - [`model`]: the model extractors (receipts, travel, packages, subscriptions, recruiting, bills,
//!   events), one JSON-schema constrained call for at most one kind per message, chosen from the
//!   triage category and the words of the mail so most mail costs no call at all.
//! - [`fact`]: the fact shapes and their validation (types, date sanity, amounts and identifiers
//!   that must appear in the mail). A fact that fails validation is not stored.
//!
//! Mail is hostile input: it reaches a model only fenced as untrusted data (the AI crate's fence and
//! preamble), mail that addresses an assistant is not sent to a model at all, and every answer is
//! validated before anything is written. Nothing here logs.

pub mod chunk;
pub mod embed;
pub mod fact;
pub mod fake;
pub mod model;
pub mod pace;
pub mod rules;
pub mod structured;

pub use chunk::{Chunk, ChunkOptions, chunks};
pub use embed::{EmbedEndpoint, Embedder, InputKind, OpenAiEmbedder};
pub use fact::{Fact, FactKind, MailInput};
pub use pace::Pacer;

/// Written into `mail.parsed.parser_version`; bump it when an extractor's output changes.
pub const PARSER_VERSION: &str = "1";
