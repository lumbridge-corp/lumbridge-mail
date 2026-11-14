//! Lumbridge Mail's Gmail backend: a core `Provider` that drives the `gog` CLI (steipete/gogcli) and
//! reads its JSON, because gog already holds the OAuth tokens and its `--gmail-no-send` /
//! `--enable-commands-exact` guards.
//!
//! - [`runner`]: how a command line runs (local binary or `ssh <host>`, niced, over one shared ssh
//!   connection), and gog exit codes to `Error`.
//! - [`budget`]: the hard limit on gog processes (per account and per host, start rate, backoff,
//!   circuit breaker) that every call goes through in the daemon.
//! - [`fake`]: a scripted runner for tests (also usable by the daemon's tests).
//! - [`GogProvider`]: the `Provider`. Every call passes `--no-input --json --enable-commands-exact <cmd>`,
//!   and `--gmail-no-send` on everything except the one send.
//!
//! See README.md for the exact gog command each method runs.

pub mod address;
pub mod api;
pub mod budget;
pub mod convert;
pub mod fake;
pub mod mime;
pub mod provider;
pub mod runner;

pub use provider::{ComposeMode, GogConfig, GogProvider, SEND_COMMAND};
pub use budget::{AccountLimits, GatedRunner, Gates, HostGate, HostLimits};
pub use runner::{Arg, GogCall, GogOutput, LocalRunner, Mux, Runner, SshRunner};
