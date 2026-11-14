//! The daemon's wire protocol: JSON-RPC 2.0, one JSON object per line, over a Unix socket
//! (`$XDG_RUNTIME_DIR/lumbridge-mail/maild.sock`, mode 0600). The app, the `lbmail` CLI and an
//! assistant's tools are all clients of it. See docs/ARCHITECTURE.md.
//!
//! Requests carry `params` as one of the structs below; results are the types named in the comments.
//! The daemon also sends notifications (no `id`) with method `event` and an `Event` as params.
//! Errors are JSON-RPC error objects whose `data` is a core [`Error`](crate::Error) (see [`ErrorObject`]).
//! docs/ARCHITECTURE.md, "Protocol", lists every method and every change to the wire.

use crate::model::*;
use crate::provider::Action;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

pub mod method {
    /// HelloParams (or {}) -> Hello. With a valid `person_token` the connection may act as a person.
    pub const HELLO: &str = "hello";
    /// {} -> Vec<Account>
    pub const ACCOUNTS: &str = "accounts.list";
    /// {account: Option<AccountId>} -> Vec<Mailbox>
    pub const MAILBOXES: &str = "mailboxes.list";
    /// Query -> ThreadPage
    pub const THREADS: &str = "threads.list";
    /// ThreadRef -> Vec<MessageMeta> (oldest first)
    pub const THREAD: &str = "thread.get";
    /// MessageRef -> Body (fetched from the provider and cached if not stored yet)
    pub const BODY: &str = "message.body";
    /// AttachmentRef -> {path: String} (saved under the daemon's cache directory)
    pub const ATTACHMENT: &str = "message.attachment";
    /// AttachmentRead -> AttachmentChunk: the attachment's bytes from `offset`, at most
    /// `ATTACHMENT_CHUNK_MAX` per call, base64 (for a client on another machine than the daemon).
    pub const ATTACHMENT_READ: &str = "message.attachment.read";
    /// Act -> {queued: u64} (applied locally at once, sent in the background)
    pub const ACT: &str = "act";
    /// {account: Option<AccountId>} -> {} (sync now)
    pub const SYNC: &str = "sync.now";
    /// {} -> Vec<AccountStatus>
    pub const STATUS: &str = "status";
    /// ThreadRef -> Triage (run or re-run the AI on one thread)
    pub const AI_TRIAGE: &str = "ai.triage";
    /// ThreadRef -> {summary: String}
    pub const AI_SUMMARISE: &str = "ai.summarise";
    /// DraftRequest -> Draft (a draft to edit; never sent by the daemon on its own)
    pub const AI_DRAFT: &str = "ai.draft";
    /// {question: String, accounts: Option<Vec<AccountId>>} -> Answer
    pub const AI_ASK: &str = "ai.ask";
    /// ThreadRef -> Extracted (action items and dates; refused for sensitive mail)
    pub const AI_EXTRACT: &str = "ai.extract";
    /// CountsRequest -> Vec<CategoryCount> (threads per AI category, and how many are unread)
    pub const COUNTS: &str = "counts";
    /// {} -> {accounts: u32} (re-read config.toml: accounts added, changed or removed)
    pub const CONFIG_RELOAD: &str = "config.reload";
    /// NewAccount -> Account (written to config.toml, the password to the keyring, then reloaded).
    /// Only from a connection that showed the person token.
    pub const ACCOUNTS_ADD: &str = "accounts.add";
    /// {email: String} -> Discovered (IMAP/SMTP servers guessed from the address; no network)
    pub const ACCOUNTS_DISCOVER: &str = "accounts.discover";
    /// {account: Option<AccountId>} -> Vec<AccountCapabilities> (what each account can do, and where)
    pub const ACCOUNTS_CAPABILITIES: &str = "accounts.capabilities";
    /// {account: AccountId} -> Vec<lumbridge_mail_resend::DomainInfo> (a Resend account's domains and
    /// their verification status). `Unsupported` for other accounts.
    pub const RESEND_DOMAINS: &str = "resend.domains";
    /// {account: AccountId} -> Vec<lumbridge_mail_resend::ApiKeyInfo> (names and dates, never a key).
    pub const RESEND_API_KEYS: &str = "resend.api_keys";
    /// Notification from the daemon. params: Event
    pub const EVENT: &str = "event";
    /// BackfillParams -> {accounts: u32} (fill in mail older than the sync window, slowly)
    pub const SYNC_BACKFILL: &str = "sync.backfill";
    /// {} -> DaemonInfo (paused?, provider load per ssh host, accounts kept after removal)
    pub const DAEMON_INFO: &str = "daemon.info";
    /// {} -> DaemonInfo. Stops every provider call (sync, bodies, actions, triage fetches) until
    /// `daemon.resume`; persisted across restarts.
    pub const DAEMON_PAUSE: &str = "daemon.pause";
    /// {} -> DaemonInfo
    pub const DAEMON_RESUME: &str = "daemon.resume";
    /// {} -> {} The daemon writes its "stopped" marker (clients must not start it again; see
    /// `paths::stopped_marker_for`), answers, and exits.
    pub const DAEMON_QUIT: &str = "daemon.quit";
    /// {account: AccountId} -> {purged: bool}. Deletes the local mail of an account that is no longer
    /// in the config (the server is not touched). Only from a person's connection.
    pub const ACCOUNTS_PURGE: &str = "accounts.purge";
    /// BodyFillParams -> {accounts: u32}. Wakes the background body fill now (also when `[daemon]
    /// body_fill` is off: then it runs for the named accounts until it is done); with `since`, looks
    /// again at every message dated at or after it that has no body.
    pub const SYNC_BODIES: &str = "sync.bodies";
}

/// `sync.bodies` params.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BodyFillParams {
    /// One account, or every account that does not opt out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountId>,
    /// Unix seconds: look again at every message from this time on that has no body (widen).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
}

/// How far the background body fill is for one account (`AccountStatus.body_fill`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BodyFillStatus {
    /// The fill runs for this account (config, or asked for with `sync.bodies`).
    pub on: bool,
    /// Messages (drafts left out) with a cached body, and all of them.
    pub have: u64,
    pub total: u64,
    /// The configured rate (fetches per minute) and the fetches made in the last hour.
    pub per_min: u32,
    pub last_hour: u32,
    /// The walk reached the oldest message; only new mail is filled now.
    pub done: bool,
    /// Why it is not fetching right now, if it is not (paused, offline, breaker, busy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<String>,
}

/// `sync.backfill` params.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BackfillParams {
    /// One account, or every account whose provider can backfill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountId>,
    /// Go back to this time (Unix seconds); None = all mail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
}

/// `daemon.info`: the daemon as a whole.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub version: String,
    /// `daemon.pause` is in force: no provider is called.
    pub paused: bool,
    /// Per budget scope (ssh host or local): processes running, starts per minute, breaker.
    #[serde(default)]
    pub load: Vec<crate::provider::ProviderLoad>,
    /// Accounts removed from the config whose mail is still stored (`accounts.purge` deletes it).
    #[serde(default)]
    pub kept_accounts: Vec<AccountId>,
}

/// Who is on the other end. The daemon refuses `Action::Send` unless this is `Person`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Caller {
    /// The app or the CLI with a human at the keyboard.
    Person,
    /// An assistant, a script, any model-driven tool.
    Agent,
}

/// `hello` params. All optional: `{}` is a plain hello.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HelloParams {
    /// The contents of the daemon's person-token file (`paths::Paths::person_token`, mode 0600, new at
    /// every daemon start). A connection that presented it may send `Caller::Person` actions; any other
    /// connection is treated as `Caller::Agent` whatever its requests say. A wrong token is `Refused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_token: Option<String>,
    /// Free text for the daemon's status, e.g. `lbmail 0.1.0` or `lumbridge-mail`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub version: String,
    /// This connection showed the person token (now or in an earlier `hello`): it may send.
    #[serde(default)]
    pub person: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRef {
    pub account: AccountId,
    pub thread: ThreadId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRef {
    pub account: AccountId,
    pub message: MessageId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentRef {
    pub account: AccountId,
    pub message: MessageId,
    pub attachment: AttachmentId,
}

/// The most bytes one `message.attachment.read` answer carries (before base64): small enough that a
/// line stays far below the 16 MiB line limit and neither side holds much at once.
pub const ATTACHMENT_CHUNK_MAX: u32 = 512 * 1024;

/// `message.attachment.read` params: which attachment, and which bytes of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentRead {
    pub account: AccountId,
    pub message: MessageId,
    pub attachment: AttachmentId,
    /// The first byte wanted.
    #[serde(default)]
    pub offset: u64,
    /// At most this many bytes (0 or more than [`ATTACHMENT_CHUNK_MAX`] = that maximum).
    #[serde(default)]
    pub max: u32,
}

/// `message.attachment.read` result: one piece of an attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentChunk {
    /// As the mail names it (not safe as a path: clients use `files::safe_file_name`).
    pub filename: String,
    pub mime: String,
    /// The whole attachment's size in bytes.
    pub size: u64,
    /// Where `data` starts.
    pub offset: u64,
    /// The bytes, standard base64 with padding.
    pub data: String,
    /// This piece ends the attachment.
    pub eof: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadPage {
    pub threads: Vec<ThreadSummary>,
    /// Pass as `Query::page` for the next page; None at the end.
    pub next: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Act {
    pub account: AccountId,
    pub action: Action,
    pub caller: Caller,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftRequest {
    pub account: AccountId,
    /// Reply to this thread; None for a new message.
    pub thread: Option<ThreadId>,
    /// What the user wants said, in their own words.
    pub instruction: String,
    /// Reply to everyone on the latest message, not just the sender.
    #[serde(default)]
    pub reply_all: bool,
    /// People the user asked to add (the only way a draft gets a recipient the thread did not have).
    #[serde(default)]
    pub extra_cc: Vec<Address>,
}

/// `counts` params. `accounts: None` = every account except anon ones (the anon wall).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CountsRequest {
    #[serde(default)]
    pub accounts: Option<Vec<AccountId>>,
    /// Only threads in a mailbox with this role (e.g. `inbox`); None = all threads.
    #[serde(default)]
    pub role: Option<MailboxRole>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryCount {
    pub category: crate::ai::Category,
    /// Threads triaged into this category.
    pub threads: u32,
    /// Of those, threads with unread mail.
    pub unread: u32,
}

/// `accounts.add` params: one account for config.toml, from the app's first-run screen or `lbmail
/// account add`. The daemon checks it, keeps the password only in the OS keyring, and reloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewAccount {
    /// a-z 0-9 - _; None = made from the address (and made unique).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub persona: Persona,
    #[serde(default)]
    pub label: String,
    pub email: String,
    /// The name mail is sent as.
    #[serde(default)]
    pub name: String,
    /// Refuse every action that would change the server (the default for a first gog account).
    #[serde(default)]
    pub readonly: bool,
    pub provider: NewProvider,
    /// IMAP: the password, saved in the OS keyring (service `lumbridge-mail`, user `imap:<id>`). Resend:
    /// the API key, saved as user `resend:<id>`. Never in the config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// The provider part of a [`NewAccount`]. Security values are `tls`, `start_tls` or `plain`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NewProvider {
    /// Made-up mail (`set`: personal | work | anon).
    Demo {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        set: Option<String>,
    },
    /// Gmail through gog: a local `binary` (default `gog`), or gog on `ssh_host`.
    Gog {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ssh_host: Option<String>,
    },
    Imap {
        imap_host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        imap_port: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        imap_security: Option<String>,
        /// None = no sending (a read-only account).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        smtp_host: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        smtp_port: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        smtp_security: Option<String>,
        /// Defaults to the address.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        username: Option<String>,
    },
    /// Resend's API for a domain: the API key goes in `NewAccount::password` (to the keyring).
    Resend {
        /// The addresses or `@domain`s this account shows and may send from; empty = `@<domain of email>`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        addresses: Vec<String>,
        /// List received mail (the domain has receiving turned on). None = yes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        receiving: Option<bool>,
    },
}

/// One row of `accounts.capabilities`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountCapabilities {
    pub account: AccountId,
    pub capabilities: crate::provider::Capabilities,
}

/// `accounts.discover` result: the servers for an address, from a built-in table of big providers or
/// the usual `imap.<domain>` / `smtp.<domain>` names. A guess the person confirms; nothing is looked up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discovered {
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: String,
    /// A known provider's name (`Fastmail`, ...); None for a guess from the domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Something the person should know (app passwords, a Bridge to run first), in plain words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// JSON-RPC error codes the daemon uses. Clients should read `data` (a core `Error`) for the kind and
/// use the code only when `data` is missing.
pub mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
    /// Any `core::Error` from the daemon; the kind is in `data`.
    pub const MAIL_ERROR: i64 = -32000;
}

/// The `error` member of a failed response:
/// `{"code": -32000, "message": "refused: sending needs a person", "data": {"kind": "refused", "detail": "sending needs a person"}}`.
/// `data` is always present for errors the daemon produces (also for bad params: `Invalid`, and unknown
/// methods: `Unsupported`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<crate::Error>,
}

impl ErrorObject {
    pub fn new(code: i64, error: crate::Error) -> Self {
        Self { code, message: error.to_string(), data: Some(error) }
    }
    /// The core error this stands for (from `data`, else guessed from the code).
    pub fn into_error(self) -> crate::Error {
        if let Some(e) = self.data {
            return e;
        }
        match self.code {
            code::METHOD_NOT_FOUND => crate::Error::Unsupported(self.message),
            code::INVALID_PARAMS | code::INVALID_REQUEST | code::PARSE_ERROR => crate::Error::Invalid(self.message),
            _ => crate::Error::Other(self.message),
        }
    }
}

impl From<crate::Error> for ErrorObject {
    fn from(e: crate::Error) -> Self {
        let code = match e {
            crate::Error::Invalid(_) => code::INVALID_PARAMS,
            crate::Error::Unsupported(_) => code::METHOD_NOT_FOUND,
            _ => code::MAIL_ERROR,
        };
        Self::new(code, e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub text: String,
    /// The threads the answer came from.
    pub sources: Vec<ThreadRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountStatus {
    pub account: AccountId,
    pub online: bool,
    pub last_sync: Option<i64>,
    /// Actions waiting to be sent to the provider.
    pub queued: u32,
    pub error: Option<crate::Error>,
    /// The daemon is paused (`daemon.pause`): nothing syncs until it resumes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
    /// The background body fill's progress (left out by daemons without it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_fill: Option<BodyFillStatus>,
}

// Events are rare and short-lived; boxing the action would change the wire types for no gain.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Threads were added or changed: re-run the current query.
    Changed { account: AccountId, threads: Vec<ThreadId> },
    NewMail { account: AccountId, thread: ThreadId, from: String, subject: String },
    Status { status: AccountStatus },
    /// A queued action failed for good (not a retry).
    ActionFailed { account: AccountId, action: Action, error: crate::Error },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_have_a_stable_wire_shape() {
        let a = Act { account: AccountId::new("p"), caller: Caller::Agent,
                      action: Action::SetSeen { ids: vec![MessageId::new("m1")], seen: true } };
        let j = serde_json::to_value(&a).unwrap();
        assert_eq!(j["action"]["op"], "set_seen");
        assert_eq!(j["caller"], "agent");
        assert_eq!(serde_json::from_value::<Act>(j).unwrap(), a);
    }

    #[test]
    fn error_objects_carry_the_core_error() {
        let e = crate::Error::Refused(crate::safety::SEND_NEEDS_A_PERSON.into());
        let o = ErrorObject::from(e.clone());
        let j = serde_json::to_value(&o).unwrap();
        assert_eq!(j["code"], code::MAIL_ERROR);
        assert_eq!(j["data"]["kind"], "refused");
        assert_eq!(serde_json::from_value::<ErrorObject>(j).unwrap().into_error(), e);
        let bare: ErrorObject = serde_json::from_str(r#"{"code":-32601,"message":"no"}"#).unwrap();
        assert_eq!(bare.into_error(), crate::Error::Unsupported("no".into()));
    }

    #[test]
    fn draft_requests_from_old_clients_still_parse() {
        let r: DraftRequest = serde_json::from_str(r#"{"account":"p","thread":null,"instruction":"hi"}"#).unwrap();
        assert!(!r.reply_all && r.extra_cc.is_empty());
        let h: HelloParams = serde_json::from_str("{}").unwrap();
        assert_eq!(h, HelloParams::default());
    }

    #[test]
    fn new_accounts_have_a_plain_wire_shape() {
        let a: NewAccount = serde_json::from_str(
            r#"{"persona":"personal","email":"sam@example.com","readonly":true,"provider":{"kind":"gog","ssh_host":"gog-host"}}"#,
        )
        .unwrap();
        assert_eq!(a.provider, NewProvider::Gog { binary: None, ssh_host: Some("gog-host".into()) });
        assert!(a.readonly && a.id.is_none() && a.password.is_none());
        let j = serde_json::to_value(NewProvider::Demo { set: Some("work".into()) }).unwrap();
        assert_eq!(j["kind"], "demo");
        let r: NewProvider = serde_json::from_str(r#"{"kind":"resend"}"#).unwrap();
        assert_eq!(r, NewProvider::Resend { addresses: vec![], receiving: None });
    }

    #[test]
    fn attachment_reads_default_to_the_start() {
        let r: AttachmentRead = serde_json::from_str(r#"{"account":"p","message":"m","attachment":"a1"}"#).unwrap();
        assert_eq!((r.offset, r.max), (0, 0));
    }

    #[test]
    fn errors_round_trip() {
        let e = crate::Error::Refused(crate::safety::SEND_NEEDS_A_PERSON.into());
        let j = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<crate::Error>(&j).unwrap(), e);
        assert!(!e.is_retryable());
        assert!(crate::Error::Offline("x".into()).is_retryable());
    }

    #[test]
    fn the_fill_and_list_additions_are_optional_on_the_wire() {
        let e = crate::Error::NoAnswer("the draft was empty".into());
        let j = serde_json::to_value(&e).unwrap();
        assert_eq!(j["kind"], "no_answer");
        assert_eq!(serde_json::from_value::<crate::Error>(j).unwrap(), e);
        assert!(!e.is_retryable());
        // Older daemons' statuses and older clients' queries still parse, and say nothing new.
        let s: AccountStatus = serde_json::from_str(r#"{"account":"p","online":true,"last_sync":null,"queued":0,"error":null}"#).unwrap();
        assert!(s.body_fill.is_none());
        assert!(serde_json::to_value(&s).unwrap().get("body_fill").is_none());
        let q: crate::Query = serde_json::from_str(
            r#"{"accounts":null,"mailbox":null,"role":null,"text":null,"category":null,"unread_only":false,"limit":5,"page":null}"#,
        )
        .unwrap();
        assert_eq!(q, crate::Query { limit: 5, ..Default::default() });
        let j = serde_json::to_value(&q).unwrap();
        assert!(j.get("since").is_none() && j.get("needs_reply").is_none() && j.get("min_priority").is_none());
        let p: BodyFillParams = serde_json::from_str("{}").unwrap();
        assert_eq!(p, BodyFillParams::default());
    }
}
