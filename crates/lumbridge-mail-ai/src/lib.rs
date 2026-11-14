//! Lumbridge Mail's AI layer: it reads mail for the user. Rules first, models second.
//!
//! - [`rules`]: no model. Sensitive mail (core's `safety::sensitive_kind` and a few more patterns) gets
//!   one fixed line and goes no further, ever. Then sender and header rules for newsletters,
//!   promotions, notifications and transactional mail.
//! - [`Model`]: an async chat call with an optional JSON schema. [`OpenAiModel`] speaks to any
//!   OpenAI-compatible endpoint; [`ScriptedModel`] is the fake for tests.
//! - [`MailAi`] / [`AccountAi`]: triage, summaries, reply drafts, ask-your-mail and action items. A
//!   thread reaches a model only through an `AccountAi`, which exists only when
//!   `safety::ai_allowed(account, model place)` says yes.
//!
//! Mail is hostile input. It goes into prompts only fenced as untrusted data under a per-call random
//! marker, the system prompt says so first, and every answer is validated before it leaves: a triage
//! line cannot carry links, addresses, codes or obedience; a draft gets its recipients from code, never
//! from the model, and keeps only links the user gave. Nothing here sends mail, and nothing here logs.
//!
//! The crate takes core types in and gives core types out; it does not know the store or the daemon.

pub mod ai;
pub mod config;
pub mod dates;
pub mod fake;
pub mod http;
pub mod injection;
pub mod model;
pub mod prompt;
pub mod rules;
pub mod testkit;
pub mod text;
pub mod thread;
pub mod validate;

pub use ai::{AccountAi, ActionItem, DateMention, Extracted, MailAi, Owner, ReplyRequest, Search, SearchHit};
pub use config::{AiConfig, EndpointConfig};
pub use fake::ScriptedModel;
pub use http::OpenAiModel;
pub use model::{ChatMessage, ChatRequest, JsonSchema, Model, Role};
pub use rules::{sensitive_line, triage_by_rules};
pub use thread::{MailMessage, MailThread};
