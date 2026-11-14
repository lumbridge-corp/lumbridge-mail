//! Received mail to things that are safe to show, and drafts to MIME-ready text + HTML.
//!
//! Everything here is pure: no I/O, no network, no UI. Mail is hostile input, so every entry point
//! is total (never panics, never fails) and bounded by [`Limits`].
//!
//! * [`sanitize`] / [`sanitize_body`]: received HTML to [`SafeHtml`] (no scripts, forms, frames,
//!   handlers, stylesheets or remote loads unless the policy allows them), with a [`Report`] of what
//!   was blocked: remote images, tracking pixels, suspicious links.
//! * [`blocks`]: a [`Body`] to a native block tree ([`Document`]) the GPUI reader paints without a
//!   browser. Layout tables are linearised; no colours are carried.
//! * [`plain_text`] / [`snippet`]: clean text for list rows and for the AI layer.
//! * [`fold_text`] and the `quoted` / `signature` parts of [`Document`] and [`SafeHtml`]: new content
//!   vs. signature vs. quoted history.
//! * [`header_info`]: `List-Unsubscribe`, `List-Id`, `Reply-To`, and a sender-authentication summary.
//! * [`compose`]: Markdown (or blocks) to an HTML part + a plain-text part, with a quoted original.
//! * [`charset`]: decoding bytes with a declared charset, and repairing mojibake.
//!
//! See the crate README for the block tree as the app paints it.

pub mod blocks;
pub mod charset;
pub mod compose;
mod clean;
mod dom;
pub mod fold;
pub mod headers;
pub mod links;
mod sanitize;
mod style;
pub mod text;

pub use blocks::{Block, Cell, Document, Image, ImageSource, Link, Span, blocks, blocks_from_html, blocks_from_text};
pub use clean::{Removed, Report};
pub use compose::{Original, Outgoing, QuoteKind, compose};
pub use fold::{FoldedText, fold_text};
pub use headers::{AuthSummary, HeaderInfo, Trust, Unsubscribe, Verdict, header_info};
pub use links::{LinkWarning, WarningKind};
pub use lumbridge_mail_core::{Attachment, Body};
pub use sanitize::{SafeHtml, sanitize, sanitize_body};
pub use text::{TextOptions, plain_text, snippet};

use serde::{Deserialize, Serialize};

/// Whether remote resources (images, CSS backgrounds) may be loaded. Blocked by default
/// (`lumbridge_mail_core::safety::REMOTE_CONTENT_BLOCKED_BY_DEFAULT`); the app flips it per message or
/// per sender when the user asks. Tracking pixels are dropped either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteContent {
    #[default]
    Blocked,
    Allowed,
}

/// How one rendering is done. `Policy::default()` is the safe reader default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub remote: RemoteContent,
    /// Keep sender colours (`color`, `background*`, `bgcolor`, `<font color>`) in sanitised HTML.
    /// Off by default so mail reads in dark mode. The block tree never carries colours.
    pub keep_colors: bool,
    /// Split quoted history and the signature out of the new content.
    pub fold: bool,
    /// When set, `cid:<content-id>` image sources resolve to `<cid_base><attachment id>` in sanitised
    /// HTML (e.g. `lbm-attachment://`), so a renderer can load the part. When unset, resolved images keep
    /// `cid:<content-id>`. Images whose content id matches no attachment are dropped either way.
    pub cid_base: Option<String>,
    pub limits: Limits,
}

impl Default for Policy {
    fn default() -> Self {
        Self { remote: RemoteContent::Blocked, keep_colors: false, fold: true, cid_base: None, limits: Limits::default() }
    }
}

impl Policy {
    /// The reader default with remote content allowed (the user pressed "Load images").
    pub fn remote_allowed() -> Self {
        Self { remote: RemoteContent::Allowed, ..Self::default() }
    }
    pub(crate) fn remote_allowed_now(&self) -> bool {
        self.remote == RemoteContent::Allowed
    }
}

/// Hard bounds so a hostile or huge message stays fast. Exceeding one truncates and sets
/// `Report::truncated`; it never fails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Input HTML or text beyond this many bytes is cut (at a char boundary) before parsing.
    pub max_input_bytes: usize,
    /// Elements and text nodes kept from the parsed DOM.
    pub max_nodes: usize,
    /// Elements nested deeper than this are flattened to their text.
    pub max_depth: usize,
    /// Blocks in one `Document` (counting nested blocks).
    pub max_blocks: usize,
    /// Characters of text in one `Document`.
    pub max_text_chars: usize,
    /// Inline `data:` images larger than this are dropped.
    pub max_data_uri_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input_bytes: 4 << 20,
            max_nodes: 150_000,
            max_depth: 64,
            max_blocks: 4_000,
            max_text_chars: 400_000,
            max_data_uri_bytes: 512 << 10,
        }
    }
}

/// Cut `s` to at most `max` bytes at a char boundary.
pub(crate) fn cut(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}
