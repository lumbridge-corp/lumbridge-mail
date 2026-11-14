//! One Zoho account (one address) and the connection it uses. Several accounts may share one connection
//! (one Zoho sign-in, one token file): they then share its token and its budget.

use lumbridge_mail_core::AccountId;
use std::path::PathBuf;
use std::time::Duration;

/// `fields` asked of `ZohoMail_list_mails` and `get_mail`: none. The tool's default row already
/// carries read state (`status`), the flag (`flagId`) and the summary; asking for named fields made the
/// live server drop some of them.
pub const DEFAULT_LIST_FIELDS: &str = "";

/// Where the connection goes and how it signs in. Never logged with values: URLs and the client id are
/// kept out of every error and log line.
#[derive(Clone, PartialEq, Eq)]
pub struct Connection {
    /// The MCP server's message endpoint (POST JSON-RPC here). Also the `resource` of a token refresh.
    pub mcp_url: String,
    /// The OAuth token endpoint a refresh is posted to.
    pub token_url: String,
    /// The OAuth client id the token was issued to.
    pub client_id: String,
    /// The token file (`mcp-remote` format: `access_token`, `refresh_token`, `expires_in`, ...). Mode
    /// 0600; rewritten atomically, mode kept, other keys kept.
    pub token_file: PathBuf,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("token_file", &self.token_file)
            .finish_non_exhaustive()
    }
}

/// The budget one connection keeps to, across every account that uses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Calls at once.
    pub max_in_flight: u32,
    /// Calls started per minute (token bucket), and how many may start back to back.
    pub max_calls_per_min: u32,
    pub burst: u32,
    /// Transport failures in a row that open the circuit breaker, and how long it stays open.
    pub breaker_failures: u32,
    pub breaker_cool: Duration,
    /// First wait after a transport failure; doubles per failure in a row (with jitter) up to the max.
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// Extra tries for a read or an idempotent change that failed on transport (never for a send).
    pub retries: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_in_flight: 2,
            max_calls_per_min: 60,
            burst: 4,
            breaker_failures: 5,
            breaker_cool: Duration::from_secs(300),
            backoff_base: Duration::from_secs(2),
            backoff_max: Duration::from_secs(120),
            retries: 2,
        }
    }
}

/// Which Zoho flag "starred" maps to (Zoho has three coloured flags and no star).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagKind {
    Info = 1,
    Important = 2,
    FollowUp = 3,
}

impl FlagKind {
    pub fn parse(s: &str) -> Option<FlagKind> {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace(['-', '_', ' '], "")
            .as_str()
        {
            "info" | "1" => Some(FlagKind::Info),
            "important" | "2" => Some(FlagKind::Important),
            "followup" | "3" => Some(FlagKind::FollowUp),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ZohoConfig {
    pub account: AccountId,
    /// This account's address. Picks the Zoho mail account (`ZohoMail_list_accounts`) unless
    /// `zoho_account_id` is set, and is the From of mail sent from here.
    pub email: String,
    pub name: Option<String>,
    pub connection: Connection,
    /// Zoho's numeric account id for this address, when known (saves the lookup).
    pub zoho_account_id: Option<String>,
    pub limits: Limits,
    /// The first (and any later full) sync lists only mail of the last `window_days` days and at most
    /// `window_max` messages in all, whichever is smaller (0 = no limit). Older mail comes by backfill.
    pub window_days: u32,
    pub window_max: u32,
    /// Messages per `list_mails` page in a full sync and a backfill (Zoho allows 1..=200).
    pub page_size: u32,
    /// Messages per folder an incremental poll looks at (the newest), and the most extra pages it
    /// follows when a burst of new mail is bigger than that.
    pub poll_size: u32,
    pub max_new_pages: u32,
    /// Inbox, Sent and Drafts are polled every round; other folders every `cold_every` rounds.
    pub cold_every: u32,
    /// Every `deep_every` rounds, `deep_pages` pages per folder are listed to pick up read state and
    /// flags changed elsewhere on mail older than the polled page (0 = never).
    pub deep_every: u32,
    pub deep_pages: u32,
    /// Messages that left a polled page without turning up in another folder are looked up one by one
    /// (`get_mail`), at most this many per round; the rest wait for the next deep pass.
    pub max_lookups: usize,
    /// Also list archived mail (Zoho keeps it out of its folder's listing) into an Archive view.
    pub sync_archive: bool,
    /// Backfill: start on its own once the window is synced, and the shortest time between pages.
    pub backfill_auto: bool,
    pub backfill_every: Duration,
    /// `wait_for_change` sleeps at most this long (Zoho's MCP has no push).
    pub poll_interval: Duration,
    /// Which Zoho flag stands for "starred".
    pub flag: FlagKind,
    /// `fields` for `list_mails` and `get_mail` (empty: not sent, the tool's default row).
    pub list_fields: String,
    /// A send is not repeated when Sent already has mail with the same subject, recipients and text
    /// from the last this-many seconds (a send that timed out may have gone out).
    pub dedupe_window_s: i64,
    pub timeout: Duration,
    /// Largest answer read (raw messages carry their attachments).
    pub max_response_bytes: u64,
}

impl ZohoConfig {
    pub fn new(account: AccountId, email: impl Into<String>, connection: Connection) -> Self {
        Self {
            account,
            email: email.into().trim().to_ascii_lowercase(),
            name: None,
            connection,
            zoho_account_id: None,
            limits: Limits::default(),
            window_days: 90,
            window_max: 2000,
            page_size: 100,
            poll_size: 50,
            max_new_pages: 5,
            cold_every: 10,
            deep_every: 60,
            deep_pages: 2,
            max_lookups: 20,
            sync_archive: false,
            backfill_auto: false,
            backfill_every: Duration::from_secs(60),
            poll_interval: Duration::from_secs(60),
            flag: FlagKind::Important,
            list_fields: DEFAULT_LIST_FIELDS.into(),
            dedupe_window_s: 6 * 3600,
            timeout: Duration::from_secs(60),
            max_response_bytes: 64 << 20,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_parse_and_connections_do_not_print_urls() {
        assert_eq!(FlagKind::parse("Follow-up"), Some(FlagKind::FollowUp));
        assert_eq!(FlagKind::parse("important"), Some(FlagKind::Important));
        assert_eq!(FlagKind::parse("red"), None);
        let c = Connection {
            mcp_url: "https://mcp.example.test/secret-path".into(),
            token_url: "https://accounts.example.test/token".into(),
            client_id: "client-123".into(),
            token_file: "/tmp/x.json".into(),
        };
        let s = format!("{c:?}");
        assert!(
            !s.contains("secret-path") && !s.contains("client-123"),
            "{s}"
        );
    }
}
