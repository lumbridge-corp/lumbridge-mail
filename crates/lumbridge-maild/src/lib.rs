//! `lumbridge-maild`: the daemon that owns the mail, and the library its binaries and tests drive.
//!
//! - [`server::Daemon`]: start (instance lock, store, accounts, socket), run, shut down.
//! - `engine`: per account a sync loop and an action-queue runner; background triage.
//! - `fill`: the background body fill (`[daemon] body_fill`), at the lowest priority.
//! - `api`: one function per `core::rpc::method`, with the safety rules that must hold in the
//!   daemon (no send from agents, sensitive mail redacted for agents, read-only accounts, anon wall).
//! - [`config`], [`secrets`]: `config.toml` and where passwords come from.
//! - [`mirror`]: the one-way Postgres mirror (`[pg]`), drained from the store's outbox.
//! - [`pgwork`]: the embedding and parsing workers on the mirror (`[pg.embed]`, `[pg.parse]`) and
//!   semantic search.
//! - [`demo`]: a made-up-mail provider, so everything runs without a network.
//! - [`client`], [`cli`]: the `lbmail` command line client.
//! - [`remote`]: a daemon on another machine over ssh (`lbmail pipe`, `lbmail person-token`).
//!
//! See the crate README for how to run it and docs/ARCHITECTURE.md ("Protocol") for the wire.

mod api;
pub mod cli;
pub mod client;
pub mod config;
pub mod demo;
mod engine;
mod fill;
pub mod fsutil;
pub mod mirror;
pub mod pgwork;
pub mod redact;
pub mod remote;
pub mod secrets;
pub mod server;
pub mod setup;

pub use client::Client;
pub use config::Config;
pub use demo::{DemoMail, DemoProvider, DemoSet};
pub use engine::{THREADING_VERSION, threading_marker};
pub use server::{Daemon, Options};
