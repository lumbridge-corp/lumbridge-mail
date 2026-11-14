//! The mail model. Ids are opaque strings; times are Unix seconds (UTC).

use serde::{Deserialize, Serialize};

macro_rules! id {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);
        impl $name {
            pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
            pub fn as_str(&self) -> &str { &self.0 }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
        }
    };
}

id!(/// Ours, stable, chosen when the account is added (e.g. `personal-gmail`).
    AccountId);
id!(/// Unique within an account; the provider's own folder or label id.
    MailboxId);
id!(/// Unique within an account; the provider's own message id (Gmail id, `mailbox:uidvalidity:uid`, JMAP id).
    MessageId);
id!(/// Unique within an account. Providers without threads derive one from References / subject.
    ThreadId);
id!(/// Unique within a message.
    AttachmentId);

/// Which part of life an account belongs to. Drives colour, the unified inbox's grouping and the AI policy default.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Persona {
    Personal,
    Work,
    /// Accounts that must not be linked to the others: never merged into cross-account threads or contact
    /// suggestions, and never sent to a model outside our own machines.
    Anon,
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Gmail through the `gog` CLI (JSON), which already holds the OAuth tokens.
    Gog,
    /// IMAP for reading, SMTP for sending.
    Imap,
    Jmap,
    /// Made-up mail for tests, screenshots and UI work. No network.
    Demo,
    /// Resend's HTTP API (resend.com): sent mail with its delivery state, scheduled mail, and mail
    /// received on a domain with receiving turned on.
    Resend,
    /// Zoho Mail through Zoho's hosted MCP server (one address per account; a sign-in may serve several).
    Zoho,
}

/// Where this account's mail may be sent for AI work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AiPolicy {
    Off,
    /// Only models on our own machines.
    #[default]
    LocalOnly,
    /// Local first, a hosted model when asked for.
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub persona: Persona,
    /// Shown in the sidebar, e.g. "Personal".
    pub label: String,
    pub email: String,
    /// The name mail is sent as.
    pub name: String,
    pub provider: ProviderKind,
    /// `#rrggbb`, used for the account's dot in the unified inbox.
    pub color: Option<String>,
    #[serde(default)]
    pub ai: AiPolicy,
    /// The daemon refuses every action that would change the server (config `readonly = true`, the
    /// way a first live account is added). Clients show the account as read-only and offer no actions.
    #[serde(default)]
    pub readonly: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Address {
    pub name: Option<String>,
    /// Lower-cased on the way in.
    pub email: String,
}

impl Address {
    pub fn new(name: Option<&str>, email: &str) -> Self {
        let name = name.map(str::trim).filter(|n| !n.is_empty()).map(str::to_owned);
        Self { name, email: email.trim().to_ascii_lowercase() }
    }
    /// What to show in a list row: the name if there is one, else the address.
    pub fn display(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.email)
    }
    pub fn domain(&self) -> &str {
        self.email.rsplit_once('@').map(|(_, d)| d).unwrap_or("")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailboxRole {
    Inbox,
    Sent,
    Drafts,
    Archive,
    Trash,
    Spam,
    Starred,
    Important,
    /// Gmail's "All Mail".
    All,
    /// A user folder or label.
    Custom,
}

impl MailboxRole {
    pub const ALL: [MailboxRole; 10] = [
        MailboxRole::Inbox,
        MailboxRole::Sent,
        MailboxRole::Drafts,
        MailboxRole::Archive,
        MailboxRole::Trash,
        MailboxRole::Spam,
        MailboxRole::Starred,
        MailboxRole::Important,
        MailboxRole::All,
        MailboxRole::Custom,
    ];

    /// The wire name (`inbox`, `sent`, ...), the same string serde uses.
    pub fn as_str(self) -> &'static str {
        match self {
            MailboxRole::Inbox => "inbox",
            MailboxRole::Sent => "sent",
            MailboxRole::Drafts => "drafts",
            MailboxRole::Archive => "archive",
            MailboxRole::Trash => "trash",
            MailboxRole::Spam => "spam",
            MailboxRole::Starred => "starred",
            MailboxRole::Important => "important",
            MailboxRole::All => "all",
            MailboxRole::Custom => "custom",
        }
    }

    /// The reverse of [`MailboxRole::as_str`] (case-insensitive; `junk` is accepted for `spam`).
    pub fn parse(s: &str) -> Option<MailboxRole> {
        let s = s.trim().to_ascii_lowercase();
        if s == "junk" {
            return Some(MailboxRole::Spam);
        }
        MailboxRole::ALL.into_iter().find(|r| r.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mailbox {
    pub id: MailboxId,
    pub account: AccountId,
    /// Display name; `/` separates levels of nested folders.
    pub name: String,
    pub role: MailboxRole,
    pub unread: u32,
    pub total: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Flags {
    pub seen: bool,
    /// Starred.
    pub flagged: bool,
    pub answered: bool,
    pub draft: bool,
}

/// Everything about a message except its body. Cheap to list, always in the local store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageMeta {
    pub id: MessageId,
    pub account: AccountId,
    pub thread: ThreadId,
    /// Folders or labels this message is in. One for plain IMAP, several for Gmail.
    pub mailboxes: Vec<MailboxId>,
    pub from: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    /// Up to ~200 characters of plain text for the list row.
    pub snippet: String,
    pub date: i64,
    pub flags: Flags,
    pub has_attachments: bool,
    pub size: u64,
    /// The `Message-ID` header, without angle brackets.
    pub rfc_message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// This record came from a listing, not from the message itself, so some fields may be missing
    /// (a Gmail search through an older gog has no To, Cc or Message-ID). The store keeps what it already
    /// has for every empty field of a partial record (addresses, snippet, ids, references, size, date
    /// 0, `has_attachments: false`) and takes the rest (mailboxes, flags, subject, from) as new.
    /// A message's addresses and ids never change, so this never hides a real change.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: AttachmentId,
    pub filename: String,
    pub mime: String,
    pub size: u64,
    /// Set for inline images (`cid:` references in the HTML).
    pub content_id: Option<String>,
}

/// Headers every provider must copy into [`Body::headers`] when the message has them. The AI rules
/// (newsletter / notification / reply-to checks) and the reader (unsubscribe, sender authentication)
/// depend on them. Providers may keep more.
pub const BODY_HEADERS: &[&str] = &[
    "List-Id",
    "List-Unsubscribe",
    "List-Unsubscribe-Post",
    "Precedence",
    "Auto-Submitted",
    "Reply-To",
    "Authentication-Results",
    "X-GitHub-Reason",
];

/// Header a provider adds to [`Body::headers`] for mail it sent through an API that reports delivery
/// (Resend): the last delivery event as a [`DeliveryState`] wire name, e.g. `delivered`. It describes the
/// state when the body was fetched; providers that know it also put the live state into the list row.
pub const DELIVERY_HEADER: &str = "X-Lumbridge-Delivery";

/// What happened to a message this account sent, as far as the provider knows (Resend's `last_event`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Scheduled,
    Queued,
    Sent,
    Delivered,
    DeliveryDelayed,
    Opened,
    Clicked,
    Bounced,
    Complained,
    Failed,
    Suppressed,
    Canceled,
}

impl DeliveryState {
    pub const ALL: [DeliveryState; 12] = [
        DeliveryState::Scheduled,
        DeliveryState::Queued,
        DeliveryState::Sent,
        DeliveryState::Delivered,
        DeliveryState::DeliveryDelayed,
        DeliveryState::Opened,
        DeliveryState::Clicked,
        DeliveryState::Bounced,
        DeliveryState::Complained,
        DeliveryState::Failed,
        DeliveryState::Suppressed,
        DeliveryState::Canceled,
    ];

    /// The wire name (`delivered`, `delivery_delayed`, ...), the same string serde uses.
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryState::Scheduled => "scheduled",
            DeliveryState::Queued => "queued",
            DeliveryState::Sent => "sent",
            DeliveryState::Delivered => "delivered",
            DeliveryState::DeliveryDelayed => "delivery_delayed",
            DeliveryState::Opened => "opened",
            DeliveryState::Clicked => "clicked",
            DeliveryState::Bounced => "bounced",
            DeliveryState::Complained => "complained",
            DeliveryState::Failed => "failed",
            DeliveryState::Suppressed => "suppressed",
            DeliveryState::Canceled => "canceled",
        }
    }

    /// The reverse of [`DeliveryState::as_str`] (case-insensitive; `cancelled` is accepted).
    pub fn parse(s: &str) -> Option<DeliveryState> {
        let s = s.trim().to_ascii_lowercase();
        if s == "cancelled" {
            return Some(DeliveryState::Canceled);
        }
        DeliveryState::ALL.into_iter().find(|d| d.as_str() == s)
    }

    /// Words for a person: "Delivered", "Delivery delayed", ...
    pub fn label(self) -> &'static str {
        match self {
            DeliveryState::Scheduled => "Scheduled",
            DeliveryState::Queued => "Queued",
            DeliveryState::Sent => "Sent",
            DeliveryState::Delivered => "Delivered",
            DeliveryState::DeliveryDelayed => "Delivery delayed",
            DeliveryState::Opened => "Opened",
            DeliveryState::Clicked => "Clicked",
            DeliveryState::Bounced => "Bounced",
            DeliveryState::Complained => "Marked as spam",
            DeliveryState::Failed => "Failed",
            DeliveryState::Suppressed => "Suppressed",
            DeliveryState::Canceled => "Canceled",
        }
    }

    /// The message did not reach the person (or they reported it): worth showing as a problem.
    pub fn is_problem(self) -> bool {
        matches!(self, DeliveryState::Bounced | DeliveryState::Complained | DeliveryState::Failed | DeliveryState::Suppressed)
    }
}

/// A message's content, fetched on demand and then cached.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Body {
    pub text: Option<String>,
    /// As received: NOT sanitised. Only `lumbridge-mail-render` may turn this into something shown.
    pub html: Option<String>,
    pub attachments: Vec<Attachment>,
    /// Selected headers worth showing or acting on, as `(name, value)` with the value unfolded onto one
    /// line. Every header named in [`BODY_HEADERS`] must be included when the message carries it
    /// (List-Id, List-Unsubscribe, List-Unsubscribe-Post, Precedence, Auto-Submitted, Reply-To,
    /// Authentication-Results, X-GitHub-Reason); a header that appears more than once appears once per
    /// occurrence. Match names without case.
    pub headers: Vec<(String, String)>,
}

impl Body {
    /// The first value of a header, matched without case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The delivery state in [`DELIVERY_HEADER`], for mail sent through an API that reports it.
    pub fn delivery(&self) -> Option<DeliveryState> {
        self.header(DELIVERY_HEADER).and_then(DeliveryState::parse)
    }
}

/// One row of a thread list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: ThreadId,
    pub account: AccountId,
    pub subject: String,
    /// The latest message's snippet.
    pub snippet: String,
    /// Everyone who wrote in the thread, oldest first, no repeats.
    pub participants: Vec<Address>,
    pub message_count: u32,
    pub unread_count: u32,
    /// Date of the latest message.
    pub date: i64,
    pub flagged: bool,
    pub has_attachments: bool,
    pub mailboxes: Vec<MailboxId>,
    /// Filled in by the AI layer when it has looked at the thread.
    pub triage: Option<crate::ai::Triage>,
}

/// A file on disk to attach to an outgoing message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftAttachment {
    pub path: String,
    pub filename: String,
    pub mime: String,
}

/// An outgoing message, before it becomes MIME.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Draft {
    /// Ours, so the store can find the draft again.
    pub local_id: String,
    /// The provider's id for this draft (a Gmail draft id, an IMAP `folder:validity:uid`), once a
    /// `SaveDraft` has reported it in its [`ApplyOutcome`](crate::ApplyOutcome). The store keeps it and
    /// fills it into later draft actions, so providers need not remember drafts in memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    pub text: String,
    pub html: Option<String>,
    /// The message being answered, so the provider can thread it and set Answered.
    pub reply_to: Option<MessageId>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub attachments: Vec<DraftAttachment>,
}

/// A search or a list request. Empty means "everything, newest first".
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Query {
    /// None = all accounts (the unified view). Anon accounts are only included when named here.
    pub accounts: Option<Vec<AccountId>>,
    pub mailbox: Option<MailboxId>,
    /// With no `mailbox`: every mailbox with this role across the chosen accounts (the unified inbox).
    pub role: Option<MailboxRole>,
    /// Free text; the store parses operators like `from:`, `has:attachment`, `is:unread`.
    pub text: Option<String>,
    pub category: Option<crate::ai::Category>,
    pub unread_only: bool,
    pub limit: u32,
    /// Opaque, from the previous page.
    pub page: Option<String>,
    /// Threads with mail at or after this time (Unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    /// Threads whose triage says a reply is needed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_reply: bool,
    /// Threads whose triage priority is at least this (0-100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_priority: Option<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_is_normalised() {
        let a = Address::new(Some("  "), " Ada@Example.COM ");
        assert_eq!(a.email, "ada@example.com");
        assert_eq!(a.display(), "ada@example.com");
        assert_eq!(a.domain(), "example.com");
        assert_eq!(Address::new(Some("Ada"), "a@b.c").display(), "Ada");
    }

    #[test]
    fn roles_have_names() {
        for r in MailboxRole::ALL {
            assert_eq!(serde_json::to_string(&r).unwrap(), format!("\"{}\"", r.as_str()));
            assert_eq!(MailboxRole::parse(r.as_str()), Some(r));
        }
        assert_eq!(MailboxRole::parse("Junk"), Some(MailboxRole::Spam));
        assert_eq!(MailboxRole::parse("nope"), None);
    }

    #[test]
    fn drafts_without_a_server_id_keep_their_old_shape() {
        let d: Draft = serde_json::from_str(r#"{"local_id":"d1","to":[],"cc":[],"bcc":[],"subject":"","text":"",
            "html":null,"reply_to":null,"in_reply_to":null,"references":[],"attachments":[]}"#).unwrap();
        assert_eq!(d.server_id, None);
        assert!(!serde_json::to_string(&d).unwrap().contains("server_id"));
        let b = Body { headers: vec![("list-id".into(), "<x>".into())], ..Body::default() };
        assert_eq!(b.header("List-Id"), Some("<x>"));
    }

    #[test]
    fn delivery_states_have_names() {
        for d in DeliveryState::ALL {
            assert_eq!(serde_json::to_string(&d).unwrap(), format!("\"{}\"", d.as_str()));
            assert_eq!(DeliveryState::parse(d.as_str()), Some(d));
        }
        assert_eq!(DeliveryState::parse("Cancelled"), Some(DeliveryState::Canceled));
        assert!(DeliveryState::Bounced.is_problem() && !DeliveryState::Opened.is_problem());
        let b = Body { headers: vec![("x-lumbridge-delivery".into(), "bounced".into())], ..Body::default() };
        assert_eq!(b.delivery(), Some(DeliveryState::Bounced));
        assert_eq!(Body::default().delivery(), None);
        assert_eq!(serde_json::to_string(&ProviderKind::Resend).unwrap(), "\"resend\"");
        assert_eq!(serde_json::to_string(&ProviderKind::Zoho).unwrap(), "\"zoho\"");
    }

    #[test]
    fn ids_are_plain_strings_on_the_wire() {
        assert_eq!(serde_json::to_string(&MessageId::new("m1")).unwrap(), "\"m1\"");
        assert_eq!(serde_json::to_string(&Persona::Anon).unwrap(), "\"anon\"");
    }
}
