//! `config.toml` under the config directory (`core::paths`). Accounts, the AI endpoint and timings.
//! No secrets: IMAP passwords and Resend API keys come from the keyring (or `password_env` /
//! `password_command`, `api_key_env` / `api_key_command` on a headless box), the AI key from
//! `ai.api_key_env`, gog keeps its own tokens, and Zoho's token is in its own file (`token_file`).
//!
//! ```toml
//! [daemon]
//! sync_interval_s = 300        # how long one wait for new mail lasts (IMAP IDLE, or a sleep for gog)
//!
//! [agents]                     # what callers without the person token may change
//! actions = false              # archive / trash / star / read / move: refused for agents (default)
//!
//! [ai]                         # no endpoint by default: AI is off and triage runs on rules only
//! enabled = true
//! base_url = "http://127.0.0.1:8000/v1"        # any OpenAI-compatible server; or LBMAIL_AI_BASE
//! model = "my-model"                           # or LBMAIL_AI_MODEL
//!
//! [gog]                        # the budget every gog process goes through (see the crate README)
//! ssh_max_in_flight = 2        # per ssh host, across accounts
//!
//! [pg]                         # the one-way Postgres mirror (off by default; see src/mirror.rs)
//! enabled = true
//! dsn_file = "~/.config/lumbridge-mail/pg.dsn"   # mode 0600: postgres://mail_writer:...@host/db
//!
//! [pg.embed]                   # embeddings for semantic search (off by default; see src/pgwork/)
//! enabled = true
//! base_url = "http://embed-host:8000/v1"
//! model = "embed-model"
//! headers = [["x-client", "lumbridge-mail"]]
//!
//! [pg.parse]                   # structured facts out of mail: rules, then the [ai] model (off by default)
//! enabled = true
//!
//! [[accounts]]
//! id = "personal"
//! persona = "personal"         # personal | work | anon | { other = "..." }
//! label = "Personal"
//! email = "sam@example.com"
//! name = "Sam Example"
//! color = "#3b82f6"
//! ai = "local_only"            # off | local_only | any
//! readonly = true              # refuse every action that would change the server
//! mirror = false               # keep this account out of the Postgres mirror (anon ones always are)
//! [accounts.provider]
//! kind = "gog"                 # gog | imap | resend | zoho | demo
//! ssh_host = "gog-host"        # or binary = "/usr/local/bin/gog" to run it here
//! env_file = "~/.config/gog.env"
//! ```

use lumbridge_mail_ai::{AiConfig, EndpointConfig};
use lumbridge_mail_core::{Account, AccountId, AiPolicy, Error, Persona, ProviderKind, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub daemon: DaemonConfig,
    pub agents: AgentsSection,
    pub ai: AiSection,
    pub gog: GogSection,
    pub pg: PgSection,
    pub accounts: Vec<AccountConfig>,
}

/// `[agents]`: what a caller that is not a person (no person token: an assistant, scripts, a model driving
/// `lbmail`) may change. Reading, asking and drafting are always allowed; sending never is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct AgentsSection {
    /// Archive, trash, star, read state, move and deleting drafts from an agent (default false: refused
    /// like Send; saving a draft is always allowed). Read-only accounts refuse them for everyone.
    pub actions: bool,
}

/// `[pg]`: the one-way mirror of the store into Postgres (`crate::mirror`). Off by default; the
/// daemon works the same without it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PgSection {
    pub enabled: bool,
    /// A file (mode 0600, this user's) holding the DSN of the `mail_writer` role. `~/` is expanded.
    pub dsn_file: Option<String>,
    /// Extra CA certificates (PEM) for a server with its own CA (`sslmode` other than `disable`).
    pub ca_file: Option<String>,
    /// Outbox entries per Postgres transaction.
    pub batch: u32,
    /// How often an idle mirror looks for new entries.
    pub poll_ms: u64,
    /// Longest wait between attempts while Postgres is unreachable (doubling from 1 s).
    pub max_backoff_s: u64,
    /// `[pg.embed]`: the embedding worker (off by default; see `crate::pgwork`).
    pub embed: crate::pgwork::EmbedSection,
    /// `[pg.parse]`: the parsing worker (off by default).
    pub parse: crate::pgwork::ParseSection,
}

impl Default for PgSection {
    fn default() -> Self {
        Self {
            enabled: false,
            dsn_file: None,
            ca_file: None,
            batch: 500,
            poll_ms: 1000,
            max_backoff_s: 300,
            embed: Default::default(),
            parse: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// One `wait_for_change` lasts at most this long; then the engine syncs anyway.
    pub sync_interval_s: u64,
    /// Longest wait between retries when a provider is offline (doubling from 2 s).
    pub max_backoff_s: u64,
    /// Triage new mail in the background.
    pub triage: bool,
    /// Background triage may fetch the latest message's body when the rules could not decide from the
    /// metadata and a model will look at it. Never for mail the rules classified.
    pub fetch_bodies_for_triage: bool,
    /// At most this many of those body fetches per account per minute (newest mail first); beyond it
    /// triage uses the snippet it has. Opening a message is not limited by this.
    pub triage_bodies_per_min: u32,
    /// Log one line per minute per busy budget (gog process starts per host, Zoho calls per sign-in).
    pub log_budget: bool,
    /// Action queue retries (see `lumbridge_mail_store::RetryPolicy`).
    pub first_retry_s: i64,
    pub max_retry_s: i64,
    pub max_attempts: u32,
    /// Bodies not read for this many days are dropped from the cache (0 = keep).
    pub body_cache_days: u32,
    /// Fetch the bodies of stored mail that has none, slowly, newest first, in the background
    /// (`crate::fill`): so embeddings, semantic search and parsed facts cover all mail, not only what
    /// was opened. Lowest priority: only while the account's provider is idle and well inside its
    /// budget. Off by default; per account `body_fill = false` opts out, `true` opts in.
    pub body_fill: bool,
    /// At most this many fill fetches per account per minute (one at a time, evenly spaced; gog fetches
    /// a whole thread per call). Per account `body_fill_per_min` overrides it.
    pub body_fill_per_min: u32,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            sync_interval_s: 300,
            max_backoff_s: 300,
            triage: true,
            fetch_bodies_for_triage: true,
            triage_bodies_per_min: 20,
            log_budget: true,
            first_retry_s: 5,
            max_retry_s: 30 * 60,
            max_attempts: 12,
            body_cache_days: 0,
            body_fill: false,
            body_fill_per_min: 10,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AiSection {
    pub enabled: bool,
    /// The endpoint (`base_url`, `model`, `place`, `label`, `headers`, `timeout_s`, `thinking`, ...).
    #[serde(flatten)]
    pub endpoint: EndpointConfig,
    /// Prompt sizes and concurrency (`concurrency`, `max_thread_chars`, `utc_offset_minutes`, ...).
    #[serde(flatten)]
    pub settings: AiConfig,
    /// Name of an environment variable holding the endpoint's bearer token, if it needs one.
    pub api_key_env: Option<String>,
}

impl Default for AiSection {
    fn default() -> Self {
        Self { enabled: true, endpoint: EndpointConfig::default(), settings: AiConfig::default(), api_key_env: None }
    }
}

/// `[gog]`: the hard budget on gog processes. Per ssh host (shared by every account whose gog runs
/// there) or for local gog; per account below that. Defaults are deliberately gentle.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct GogSection {
    /// gog processes at once on one ssh host / on this machine.
    pub ssh_max_in_flight: u32,
    pub local_max_in_flight: u32,
    /// Process starts per minute per host (a token bucket), and how many may start back to back.
    pub max_starts_per_min: u32,
    pub burst: u32,
    /// One account's calls at once (inside the host's limit); per-account `max_in_flight` overrides it.
    pub account_max_in_flight: u32,
    /// Retries of a call that failed on transport (ssh exit 255, connection reset, timeout), each after
    /// the host's backoff: `backoff_base_s` doubling with jitter up to `backoff_max_s`.
    pub retries: u32,
    pub backoff_base_s: u64,
    pub backoff_max_s: u64,
    /// This many transport failures in a row and the host's accounts go offline for `breaker_cool_s`.
    pub breaker_failures: u32,
    pub breaker_cool_s: u64,
    /// `nice -n` for gog (remote and local); `ionice -c3` too where it exists.
    pub nice: i32,
    pub ionice: bool,
    /// Share one ssh connection per host (ControlMaster), kept `ssh_persist_s` after the last call.
    pub ssh_multiplex: bool,
    pub ssh_persist_s: u64,
    /// Sessions open at once on one shared connection; `ssh_max_in_flight` is capped to it. Keep it
    /// below the server's MaxSessions (OpenSSH: 10).
    pub ssh_max_sessions: u32,
    /// One gog call's deadline.
    pub timeout_s: u64,
}

impl Default for GogSection {
    fn default() -> Self {
        Self {
            ssh_max_in_flight: 2,
            local_max_in_flight: 3,
            max_starts_per_min: 120,
            burst: 2,
            account_max_in_flight: 2,
            retries: 3,
            backoff_base_s: 2,
            backoff_max_s: 120,
            breaker_failures: 5,
            breaker_cool_s: 300,
            nice: 15,
            ionice: true,
            ssh_multiplex: true,
            ssh_persist_s: 300,
            ssh_max_sessions: 8,
            timeout_s: 60,
        }
    }
}

impl GogSection {
    pub fn host_limits(&self, ssh: bool) -> lumbridge_mail_gog::HostLimits {
        let max = if ssh { self.ssh_max_in_flight.min(self.ssh_max_sessions.max(1)) } else { self.local_max_in_flight };
        lumbridge_mail_gog::HostLimits {
            max_in_flight: max.max(1),
            max_starts_per_min: self.max_starts_per_min.max(1),
            burst: self.burst.max(1),
            breaker_failures: self.breaker_failures.max(1),
            breaker_cool: std::time::Duration::from_secs(self.breaker_cool_s.max(1)),
            backoff_base: std::time::Duration::from_secs(self.backoff_base_s.max(1)),
            backoff_max: std::time::Duration::from_secs(self.backoff_max_s.max(1)),
        }
    }
}

/// Per-account gog settings (`[accounts.provider]` keys next to `kind = "gog"`). Unset = the default.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GogOptions {
    /// The first sync lists only the newest mail: this many days (default 90; 0 = no day limit)...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_days: Option<u32>,
    /// ...and at most this many messages (default 2000; 0 = no count limit), whichever is smaller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_max: Option<u32>,
    /// Fill in all older mail slowly on its own once the window is synced (default false; `lbmail sync
    /// --backfill` starts it by hand).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backfill: Option<bool>,
    /// Shortest time between two backfill pages (default 60 s; one page per sync round at most).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backfill_every_s: Option<u64>,
    /// Messages per listing (default 100).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_size: Option<u32>,
    /// Ask gog for body text in listings to make snippets (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_bodies: Option<bool>,
    /// This account's gog calls at once (default `[gog] account_max_in_flight`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_in_flight: Option<u32>,
    /// Background triage body fetches per minute (default `[daemon] triage_bodies_per_min`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_bodies_per_min: Option<u32>,
}

/// Per-account Zoho settings (`[accounts.provider]` keys next to `kind = "zoho"`). Unset = the default
/// (see `crates/lumbridge-mail-zoho/README.md`). The budget keys apply to the whole connection: give
/// accounts that share a token file the same values.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ZohoOptions {
    /// The first sync: the last this many days (default 90; 0 = no day limit)...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_days: Option<u32>,
    /// ...and at most this many messages per folder (default 2000; 0 = no count limit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_max: Option<u32>,
    /// Fill in older mail on its own once the window is synced (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backfill: Option<bool>,
    /// Shortest time between two backfill pages (default 60 s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backfill_every_s: Option<u64>,
    /// Messages per listing in a full sync or backfill (default 100, at most 200).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_size: Option<u32>,
    /// Newest messages per folder an incremental round looks at (default 50).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_size: Option<u32>,
    /// Seconds between incremental rounds at most (default 60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval_s: Option<u64>,
    /// Calls at once and calls per minute for the connection (default 2 and 60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_in_flight: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_calls_per_min: Option<u32>,
    /// The Zoho flag a star sets: info | important | follow_up (default important).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
    /// List archived mail into an Archive view (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_archive: Option<bool>,
    /// `fields` for `list_mails` and `get_mail` (default: none; the tool's default row is what sync reads).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_fields: Option<String>,
    /// Background triage body fetches per minute (default `[daemon] triage_bodies_per_min`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_bodies_per_min: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountConfig {
    pub id: String,
    pub persona: Persona,
    #[serde(default)]
    pub label: String,
    pub email: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default)]
    pub ai: AiPolicy,
    /// Refuse every action that would change the server (first live tests).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub readonly: bool,
    /// Mirror this account into Postgres when `[pg]` is on (default true). Anon accounts never are.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub mirror: bool,
    /// The background body fill for this account: unset = `[daemon] body_fill`; false opts out (also
    /// of `lbmail sync --bodies`), true opts in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_fill: Option<bool>,
    /// Fill fetches per minute for this account (unset = `[daemon] body_fill_per_min`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_fill_per_min: Option<u32>,
    pub provider: ProviderConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderConfig {
    /// Gmail through gog: locally (`binary`) or on the machine that holds the tokens (`ssh_host`).
    Gog {
        #[serde(default = "default_gog")]
        binary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ssh_host: Option<String>,
        /// Sourced before gog runs (on the ssh host when `ssh_host` is set). With `ssh_host`, `~/` in
        /// this and in `binary` is the ssh user's home on that host.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env_file: Option<String>,
        #[serde(flatten)]
        options: GogOptions,
    },
    Imap {
        imap_host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        imap_port: Option<u16>,
        #[serde(default = "default_security")]
        imap_security: lumbridge_mail_imap::Security,
        /// None = a read-only account (no sending).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        smtp_host: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        smtp_port: Option<u16>,
        #[serde(default = "default_security")]
        smtp_security: lumbridge_mail_imap::Security,
        /// Defaults to the email address.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        #[serde(default)]
        auth: AuthKind,
        /// Headless boxes: read the password (or token) from this environment variable...
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password_env: Option<String>,
        /// ...or from this command's stdout (e.g. `["pass", "show", "mail/work"]`). Else the OS keyring.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password_command: Option<Vec<String>>,
        /// Proton Mail Bridge on loopback uses a self-signed certificate (ignored for other hosts).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        accept_invalid_certs: bool,
        /// The server files sent mail itself (Gmail, Microsoft 365). None = guess.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server_saves_sent: Option<bool>,
    },
    /// Resend's HTTP API for a domain (sent mail with delivery state, scheduled mail, received mail).
    /// The API key comes from `api_key_env`, then `api_key_command`, then the OS keyring (user
    /// `resend:<id>`); never from this file.
    Resend {
        /// Default `https://api.resend.com` (tests: the fake server on loopback).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        /// Addresses (`a@shop.example`) or domains (`@shop.example`) this account shows and sends from.
        /// Resend's lists are team-wide; empty = the domain of `email`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        addresses: Vec<String>,
        /// List received mail (receiving is on for the domain).
        #[serde(default = "yes", skip_serializing_if = "is_true")]
        receiving: bool,
        /// Reply-To on every send.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key_env: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key_command: Option<Vec<String>>,
        /// Requests per second for this account (default 4; Resend allows 10 per team).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requests_per_second: Option<u32>,
    },
    /// Zoho Mail through Zoho's hosted MCP server. One Zoho sign-in (one token file) may serve several
    /// addresses: give each its own account with the same connection keys; they share one connection,
    /// one token and one budget. The token itself stays in `token_file` (mcp-remote format, 0600).
    Zoho {
        /// The MCP server's message URL (https).
        mcp_url: String,
        /// The OAuth token endpoint a refresh is posted to (https).
        token_url: String,
        client_id: String,
        /// `~/` is expanded.
        token_file: String,
        /// Zoho's numeric account id for this address; looked up by `email` when unset.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        zoho_account_id: Option<String>,
        #[serde(flatten)]
        options: ZohoOptions,
    },
    /// Made-up mail, no network (`set`: personal | work | anon; default from the persona).
    Demo {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        set: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    #[default]
    Password,
    /// The secret is an OAuth 2 access token (XOAUTH2). Refreshing it is not done by the daemon yet.
    Oauth2,
}

fn default_gog() -> String {
    "gog".into()
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

fn default_security() -> lumbridge_mail_imap::Security {
    lumbridge_mail_imap::Security::Tls
}

impl AccountConfig {
    pub fn account_id(&self) -> AccountId {
        AccountId::new(&self.id)
    }

    pub fn kind(&self) -> ProviderKind {
        match self.provider {
            ProviderConfig::Gog { .. } => ProviderKind::Gog,
            ProviderConfig::Imap { .. } => ProviderKind::Imap,
            ProviderConfig::Resend { .. } => ProviderKind::Resend,
            ProviderConfig::Zoho { .. } => ProviderKind::Zoho,
            ProviderConfig::Demo { .. } => ProviderKind::Demo,
        }
    }

    /// The core account this entry describes.
    pub fn account(&self) -> Account {
        Account {
            id: self.account_id(),
            persona: self.persona.clone(),
            label: if self.label.is_empty() { self.id.clone() } else { self.label.clone() },
            email: self.email.trim().to_ascii_lowercase(),
            name: if self.name.is_empty() { self.email.clone() } else { self.name.clone() },
            provider: self.kind(),
            color: self.color.clone(),
            ai: self.ai,
            readonly: self.readonly,
        }
    }

    pub fn check(&self) -> Result<()> {
        let ok_id = !self.id.is_empty()
            && self.id.len() <= 64
            && self.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok_id {
            return Err(Error::Invalid(format!("account id {:?}: use 1-64 of a-z, 0-9, - and _", self.id)));
        }
        if !self.email.contains('@') {
            return Err(Error::Invalid(format!("account {}: email {:?} is not an address", self.id, self.email)));
        }
        if let ProviderConfig::Zoho { token_file, options, .. } = &self.provider {
            if token_file.trim().is_empty() || (token_file.starts_with('~') && !token_file.starts_with("~/")) {
                return Err(Error::Invalid(format!("account {}: token_file {token_file:?}: a path (only ~/ is expanded)", self.id)));
            }
            if options.flag.as_deref().is_some_and(|f| lumbridge_mail_zoho::FlagKind::parse(f).is_none()) {
                return Err(Error::Invalid(format!("account {}: flag must be info, important or follow_up", self.id)));
            }
        }
        if let ProviderConfig::Gog { binary, ssh_host, env_file, .. } = &self.provider {
            let at = |e: Error| Error::Invalid(format!("account {}: {}", self.id, e.to_string().trim_start_matches("bad input: ")));
            match ssh_host {
                Some(h) => {
                    if h.trim().is_empty() || h.starts_with('-') {
                        return Err(Error::Invalid(format!("account {}: ssh_host {h:?} is not a host", self.id)));
                    }
                    lumbridge_mail_gog::runner::check_remote_path("binary", binary, true).map_err(at)?;
                    if let Some(e) = env_file {
                        lumbridge_mail_gog::runner::check_remote_path("env_file", e, false).map_err(at)?;
                    }
                }
                None => {
                    for (what, p) in [("binary", Some(binary)), ("env_file", env_file.as_ref())] {
                        if let Some(p) = p
                            && p.starts_with('~')
                            && !p.starts_with("~/")
                        {
                            return Err(Error::Invalid(format!("account {}: {what} {p:?}: only ~/ is expanded, not ~user", self.id)));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

impl Config {
    /// Parse and check. Duplicate or malformed account ids are errors.
    pub fn parse(text: &str) -> Result<Config> {
        let c: Config = toml::from_str(text).map_err(|e| Error::Invalid(format!("config.toml: {e}")))?;
        let mut seen = std::collections::HashSet::new();
        for a in &c.accounts {
            a.check()?;
            if !seen.insert(a.id.clone()) {
                return Err(Error::Invalid(format!("config.toml: account {} appears twice", a.id)));
            }
        }
        Ok(c)
    }

    /// The file at `path`; a missing file is an empty config.
    pub fn load(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(t) => Self::parse(&t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(Error::Other(format!("{}: {e}", path.display()))),
        }
    }

    pub fn find(&self, id: &AccountId) -> Option<&AccountConfig> {
        self.accounts.iter().find(|a| a.id == id.as_str())
    }
}

/// Edits of the file that keep every other section as it was (comments are not kept).
pub mod edit {
    use super::*;

    fn read_table(path: &Path) -> Result<toml::Table> {
        match std::fs::read_to_string(path) {
            Ok(t) => t.parse::<toml::Table>().map_err(|e| Error::Invalid(format!("config.toml: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
            Err(e) => Err(Error::Other(format!("{}: {e}", path.display()))),
        }
    }

    fn write_table(path: &Path, table: &toml::Table) -> Result<()> {
        let text = toml::to_string_pretty(table).map_err(|e| Error::Other(format!("config.toml: {e}")))?;
        Config::parse(&text)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::Other(format!("{}: {e}", dir.display())))?;
        }
        let tmp = path.with_extension("toml.new");
        std::fs::write(&tmp, text).map_err(|e| Error::Other(format!("{}: {e}", tmp.display())))?;
        crate::fsutil::chmod(&tmp, 0o600);
        std::fs::rename(&tmp, path).map_err(|e| Error::Other(format!("{}: {e}", path.display())))
    }

    fn accounts(table: &mut toml::Table) -> Result<&mut Vec<toml::Value>> {
        let v = table.entry("accounts").or_insert_with(|| toml::Value::Array(Vec::new()));
        v.as_array_mut().ok_or_else(|| Error::Invalid("config.toml: `accounts` must be an array of tables".into()))
    }

    /// Adds an account; `Invalid` if the id is taken.
    pub fn add_account(path: &Path, account: &AccountConfig) -> Result<()> {
        account.check()?;
        let mut table = read_table(path)?;
        let list = accounts(&mut table)?;
        if list.iter().any(|a| a.get("id").and_then(|v| v.as_str()) == Some(account.id.as_str())) {
            return Err(Error::Invalid(format!("there is already an account {}", account.id)));
        }
        let value = toml::Value::try_from(account).map_err(|e| Error::Other(format!("account: {e}")))?;
        list.push(value);
        write_table(path, &table)
    }

    /// Removes an account; false if there was none.
    pub fn remove_account(path: &Path, id: &str) -> Result<bool> {
        let mut table = read_table(path)?;
        let list = accounts(&mut table)?;
        let before = list.len();
        list.retain(|a| a.get("id").and_then(|v| v.as_str()) != Some(id));
        if list.len() == before {
            return Ok(false);
        }
        write_table(path, &table)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"
[daemon]
sync_interval_s = 60

[ai]
model = "small"
concurrency = 2

[[accounts]]
id = "personal"
persona = "personal"
label = "Personal"
email = "Sam@Example.com"
name = "Sam Example"
readonly = true
[accounts.provider]
kind = "gog"
ssh_host = "gog-host"

[[accounts]]
id = "work"
persona = "work"
email = "sam@work.example"
ai = "off"
[accounts.provider]
kind = "imap"
imap_host = "imap.work.example"
smtp_host = "smtp.work.example"
smtp_security = "start_tls"
password_env = "WORK_MAIL_PASSWORD"

[[accounts]]
id = "shop"
persona = "work"
email = "hello@shop.example"
[accounts.provider]
kind = "resend"
addresses = ["hello@shop.example", "@help.shop.example"]
receiving = false
api_key_env = "SHOP_RESEND_KEY"

[[accounts]]
id = "zoho-one"
persona = "work"
email = "sam@one.example.test"
readonly = true
[accounts.provider]
kind = "zoho"
mcp_url = "https://mcp.example.test/message"
token_url = "https://accounts.example.test/oauth/token"
client_id = "made-up-client"
token_file = "~/.config/zoho/tokens.json"
window_days = 30
flag = "follow_up"

[[accounts]]
id = "forum"
persona = { other = "side project" }
email = "otter@anon.example"
[accounts.provider]
kind = "demo"
set = "anon"
"##;

    #[test]
    fn a_full_config_parses_with_defaults() {
        let c = Config::parse(SAMPLE).unwrap();
        assert_eq!(c.daemon.sync_interval_s, 60);
        assert_eq!(c.daemon.max_attempts, 12);
        assert_eq!(c.ai.endpoint.model, "small");
        assert_eq!(c.ai.endpoint.base_url, "", "no endpoint unless the config or the environment sets one");
        assert_eq!(c.ai.settings.concurrency, 2);
        let p = c.find(&AccountId::new("personal")).unwrap();
        assert!(p.readonly);
        assert_eq!(p.account().email, "sam@example.com");
        assert_eq!(p.provider, ProviderConfig::Gog { binary: "gog".into(), ssh_host: Some("gog-host".into()), env_file: None, options: GogOptions::default() });
        assert_eq!(c.gog, GogSection::default());
        let w = c.find(&AccountId::new("work")).unwrap();
        assert_eq!(w.ai, AiPolicy::Off);
        assert_eq!(w.account().label, "work");
        assert!(matches!(&w.provider, ProviderConfig::Imap { smtp_security: lumbridge_mail_imap::Security::StartTls, .. }));
        assert_eq!(c.find(&AccountId::new("forum")).unwrap().persona, Persona::Other("side project".into()));
        let shop = c.find(&AccountId::new("shop")).unwrap();
        assert_eq!(shop.kind(), ProviderKind::Resend);
        match &shop.provider {
            ProviderConfig::Resend { addresses, receiving, api_key_env, base_url, .. } => {
                assert_eq!(addresses.len(), 2);
                assert!(!receiving && base_url.is_none());
                assert_eq!(api_key_env.as_deref(), Some("SHOP_RESEND_KEY"));
            }
            other => panic!("{other:?}"),
        }
        let z = c.find(&AccountId::new("zoho-one")).unwrap();
        assert_eq!(z.kind(), ProviderKind::Zoho);
        match &z.provider {
            ProviderConfig::Zoho { token_file, zoho_account_id, options, .. } => {
                assert_eq!(token_file, "~/.config/zoho/tokens.json");
                assert!(zoho_account_id.is_none());
                assert_eq!((options.window_days, options.flag.as_deref(), options.window_max), (Some(30), Some("follow_up"), None));
            }
            other => panic!("{other:?}"),
        }
        let bad_flag = SAMPLE.replace("flag = \"follow_up\"", "flag = \"purple\"");
        assert!(Config::parse(&bad_flag).is_err());
        let r = Config::parse("[[accounts]]\nid = \"r\"\npersona = \"work\"\nemail = \"x@y.example\"\n[accounts.provider]\nkind = \"resend\"").unwrap();
        assert!(matches!(r.accounts[0].provider, ProviderConfig::Resend { receiving: true, .. }), "receiving defaults to on");
    }

    #[test]
    fn the_mirror_is_off_unless_configured() {
        let c = Config::parse(SAMPLE).unwrap();
        assert!(!c.pg.enabled && c.pg.dsn_file.is_none());
        assert!(c.accounts.iter().all(|a| a.mirror));
        let c = Config::parse(
            "[pg]\nenabled = true\ndsn_file = \"~/.config/lumbridge-mail/pg.dsn\"\nbatch = 100\n\n[[accounts]]\nid = \"w\"\npersona = \"work\"\nemail = \"s@work.example\"\nmirror = false\n[accounts.provider]\nkind = \"demo\"\n",
        )
        .unwrap();
        assert!(c.pg.enabled);
        assert_eq!((c.pg.batch, c.pg.poll_ms), (100, 1000));
        assert!(!c.accounts[0].mirror);
        let back = toml::to_string(&c.accounts[0]).unwrap();
        assert!(back.contains("mirror = false"), "{back}");
        assert!(!toml::to_string(&Config::parse(SAMPLE).unwrap().accounts[0]).unwrap().contains("mirror"), "the default is not written");
    }

    #[test]
    fn bad_configs_are_refused() {
        assert!(Config::parse("[[accounts]]\nid = \"a b\"\npersona = \"work\"\nemail = \"x@y\"\n[accounts.provider]\nkind = \"demo\"").is_err());
        let twice = "[[accounts]]\nid = \"a\"\npersona = \"work\"\nemail = \"x@y\"\n[accounts.provider]\nkind = \"demo\"\n".repeat(2);
        assert!(Config::parse(&twice).is_err());
        assert!(Config::parse("[[accounts]]\nid = \"a\"\npersona = \"work\"\nemail = \"x@y\"\n[accounts.provider]\nkind = \"pop3\"").is_err());
    }

    #[test]
    fn accounts_are_added_and_removed_keeping_other_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[daemon]\nsync_interval_s = 42\n").unwrap();
        let a = AccountConfig {
            id: "demo".into(),
            persona: Persona::Work,
            label: "Demo".into(),
            email: "sam@work.example".into(),
            name: "Sam".into(),
            color: None,
            ai: AiPolicy::LocalOnly,
            readonly: false,
            mirror: true,
            body_fill: None,
            body_fill_per_min: None,
            provider: ProviderConfig::Demo { set: None },
        };
        edit::add_account(&path, &a).unwrap();
        assert!(edit::add_account(&path, &a).is_err());
        let c = Config::load(&path).unwrap();
        assert_eq!(c.daemon.sync_interval_s, 42);
        assert_eq!(c.accounts, vec![a]);
        assert!(edit::remove_account(&path, "demo").unwrap());
        assert!(!edit::remove_account(&path, "demo").unwrap());
        assert!(Config::load(&path).unwrap().accounts.is_empty());
        assert!(Config::load(&dir.path().join("missing.toml")).unwrap().accounts.is_empty());
    }

    #[test]
    fn gog_options_and_budget_parse_flat_next_to_kind() {
        let c = Config::parse(
            "[gog]\nssh_max_in_flight = 12\nmax_starts_per_min = 30\n\n[[accounts]]\nid = \"g\"\npersona = \"personal\"\nemail = \"s@example.com\"\n\
             [accounts.provider]\nkind = \"gog\"\nssh_host = \"gog-host\"\nbinary = \"~/.local/bin/gog\"\nenv_file = \"~/.config/gog.env\"\nwindow_days = 30\nbackfill = true\n",
        )
        .unwrap();
        let ProviderConfig::Gog { options, binary, .. } = &c.accounts[0].provider else { panic!() };
        assert_eq!(binary, "~/.local/bin/gog");
        assert_eq!((options.window_days, options.window_max, options.backfill), (Some(30), None, Some(true)));
        // The host cap never goes above the sessions one shared ssh connection allows.
        assert_eq!(c.gog.host_limits(true).max_in_flight, 8);
        assert_eq!(c.gog.host_limits(false).max_in_flight, 3);
        assert_eq!(c.gog.host_limits(true).max_starts_per_min, 30);
        // Written back flat, and read again the same.
        let back = toml::to_string(&c.accounts[0]).unwrap();
        assert!(back.contains("window_days = 30") && !back.contains("window_max"), "{back}");
    }

    #[test]
    fn gog_paths_for_an_ssh_host_are_checked() {
        let gog = |extra: &str| format!("[[accounts]]\nid = \"g\"\npersona = \"personal\"\nemail = \"s@example.com\"\n[accounts.provider]\nkind = \"gog\"\n{extra}");
        assert!(Config::parse(&gog("ssh_host = \"h\"\nbinary = \"~/.local/bin/gog\"\nenv_file = \"~/.config/gog.env\"")).is_ok());
        assert!(Config::parse(&gog("ssh_host = \"h\"")).is_ok(), "a bare gog is looked up on the remote PATH");
        let e = Config::parse(&gog("ssh_host = \"h\"\nbinary = \"~sam/bin/gog\"")).unwrap_err().to_string();
        assert!(e.contains("account g") && e.contains("only ~/"), "{e}");
        assert!(Config::parse(&gog("ssh_host = \"h\"\nenv_file = \"gog.env\"")).is_err(), "relative env file on a remote host");
        assert!(Config::parse(&gog("ssh_host = \"-oProxyCommand=x\"")).is_err());
        assert!(Config::parse(&gog("binary = \"~other/gog\"")).is_err());
    }
}
