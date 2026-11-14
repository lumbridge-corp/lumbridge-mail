//! Lumbridge Mail's Zoho backend: a core `Provider` for one Zoho Mail address, through Zoho's hosted MCP
//! server (JSON-RPC 2.0 `tools/call` over HTTPS with a Bearer token; JSON or SSE answers).
//!
//! - Folders, mail listings, bodies (HTML), headers, the raw message and attachments (cut out of the raw
//!   message) through Zoho's mail tools; read state, flags, moves, archive and trash on the server; send
//!   and reply. Drafts are local only (no draft tool).
//! - Only an allowlist of mail tools is ever called ([`mcp::ALLOWED_TOOLS`]); the server's admin tools
//!   are refused before anything is sent.
//! - One sign-in (one token file) can serve several addresses: each is its own account, and they share
//!   one connection: one token, one refresh at a time, one budget ([`mcp::ZohoConn::shared`]).
//! - The token is refreshed only when it has expired or a call came back 401, after reading the file
//!   again (another program may share the refresh token).
//!
//! [`fake`] is an in-process fake of the server for tests. See README.md for every tool call.

pub mod budget;
pub mod config;
mod cursor;
pub mod fake;
pub mod mcp;
pub mod parse;
mod provider;
pub mod schema;
mod token;

pub use config::{Connection, DEFAULT_LIST_FIELDS, FlagKind, Limits, ZohoConfig};
pub use cursor::ARCHIVE;
pub use mcp::{ALLOWED_TOOLS, Retry, ZohoConn, loads};
pub use provider::{ProbeLine, ZohoProvider, cursor_page_ids};
