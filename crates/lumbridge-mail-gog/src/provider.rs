//! `GogProvider`: the core `Provider` for one Gmail account, through gog. See the crate README for the
//! exact gog command each method runs.

use crate::api;
use crate::convert::{self, ALL_MAIL};
use crate::mime::{self, Envelope, LoadedAttachment};
use crate::runner::{Arg, GogCall, GogOutput, Runner, classify};
use futures::StreamExt;
use futures::future::BoxFuture;
use lumbridge_mail_core::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How outgoing mail (drafts and sends) is handed to gog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ComposeMode {
    /// Ask `gog version` once: `Raw` from 0.41.0 on, else `Flags`.
    #[default]
    Auto,
    /// We build the MIME (`mime::build`) and pass it with `--raw-file -`. Needs gog 0.41+.
    /// Exact draft lookup and exact send de-duplication by Message-ID.
    Raw,
    /// gog builds the MIME from `--to/--subject/--body-file/--attach/--reply-to-message-id` (gog 0.29).
    /// Drafts are tracked in memory only; send de-duplication is by subject + recipients + snippet.
    Flags,
}

#[derive(Debug, Clone)]
pub struct GogConfig {
    pub account: AccountId,
    /// The Gmail address, passed as `--account`, and the From of raw mail.
    pub email: String,
    /// The name mail is sent as (raw mode).
    pub name: Option<String>,
    pub compose: ComposeMode,
    /// Messages per sync page (one `gmail messages search` each).
    pub page_size: u32,
    /// History entries per incremental page.
    pub history_page: u32,
    /// gog calls in flight at once for the rare per-message lookups (the runner's budget still applies).
    pub concurrency: usize,
    /// What a full sync lists. `in:anywhere` includes Spam and Trash.
    pub full_sync_query: String,
    /// The first (and any later full) sync lists only the newest mail: the last `window_days` days
    /// (`newer_than:<n>d`) and at most `window_max` messages, whichever is smaller. 0 and 0 = all mail.
    /// Older mail comes in only by backfill.
    pub window_days: u32,
    pub window_max: u32,
    /// gog's history only reports new messages, so label changes made elsewhere (read on the phone,
    /// archived on the web) are picked up by re-listing recent mail with this query. None turns it off.
    pub reconcile_query: Option<String>,
    pub reconcile_max: u32,
    /// Labels whose unread/total counts are fetched on every sync (one `labels get` each). The store
    /// counts from messages itself, so the default is none.
    pub count_labels: Vec<String>,
    /// The label list is fetched again at most this often (and on every full sync's first page).
    pub labels_ttl: Duration,
    /// Ask gog for each listed message's body text (`--include-body`) to make snippets. Same number of
    /// processes; bigger output. Off: list rows have no preview until the body is fetched.
    pub search_bodies: bool,
    /// Backfill: messages per page, and the shortest time between two pages.
    pub backfill_page: u32,
    pub backfill_every: Duration,
    /// Start a backfill of all older mail on its own once the window is synced (default: only when asked).
    pub backfill_auto: bool,
    pub timeout: Duration,
    pub send_timeout: Duration,
    /// Flags mode only: how far back a sent message counts as "this draft, already sent".
    pub dedupe_window_s: i64,
}

impl GogConfig {
    pub fn new(account: AccountId, email: impl Into<String>) -> Self {
        Self {
            account,
            email: email.into(),
            name: None,
            compose: ComposeMode::Auto,
            page_size: 100,
            history_page: 100,
            concurrency: 2,
            full_sync_query: "in:anywhere".into(),
            window_days: 90,
            window_max: 2000,
            reconcile_query: Some("in:anywhere newer_than:7d".into()),
            reconcile_max: 50,
            count_labels: Vec::new(),
            labels_ttl: Duration::from_secs(300),
            search_bodies: true,
            backfill_page: 100,
            backfill_every: Duration::from_secs(60),
            backfill_auto: false,
            timeout: Duration::from_secs(60),
            send_timeout: Duration::from_secs(180),
            dedupe_window_s: 6 * 3600,
        }
    }

    /// The query of a new full sync, and whether it is windowed.
    pub fn window_query(&self) -> (String, bool) {
        let windowed = self.window_days > 0 || self.window_max > 0;
        let q = if self.window_days > 0 {
            format!("{} newer_than:{}d", self.full_sync_query, self.window_days)
        } else {
            self.full_sync_query.clone()
        };
        (q, windowed)
    }
}

/// What the installed gog can do, from `gog version` (asked once per provider). Unknown versions are
/// treated as the oldest supported (0.28).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Features {
    pub version: Option<(u32, u32, u32)>,
}

impl Features {
    fn at_least(&self, v: (u32, u32, u32)) -> bool {
        self.version.is_some_and(|x| x >= v)
    }
    /// `messages search` prints `internalDateIso` (0.34.2).
    pub fn iso_dates(&self) -> bool {
        self.at_least((0, 34, 2))
    }
    /// `messages search --include-recipients` (0.44.0).
    pub fn recipients(&self) -> bool {
        self.at_least((0, 44, 0))
    }
    /// `messages search --headers a,b` (0.47.0): Message-ID, References and Content-Type in the listing,
    /// so listed records are complete except for the size.
    pub fn headers(&self) -> bool {
        self.at_least((0, 47, 0))
    }
    /// `--raw-file` on drafts and send (0.41.0).
    pub fn raw_compose(&self) -> bool {
        self.at_least((0, 41, 0))
    }
}

/// Headers asked for in listings on gog 0.47+.
const SEARCH_HEADERS: &str = "Message-Id,In-Reply-To,References,Content-Type";

/// The only command that runs without `--gmail-no-send`.
pub const SEND_COMMAND: &str = "gmail.send";

/// Where a sync stands. Serialised into the opaque `SyncCursor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
enum Cursor {
    /// A paged full sync. `h` is the historyId to resume from once the pages are done.
    Full {
        h: u64,
        page: Option<String>,
        /// Messages listed so far (for `window_max`).
        #[serde(default, skip_serializing_if = "is_zero")]
        n: u32,
        /// The windowed query this run pages through (page tokens belong to one query). None: an
        /// unwindowed run over `full_sync_query` (also every cursor from before windows existed).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        q: Option<String>,
        /// Oldest date listed so far.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lo: Option<i64>,
        /// The oldest date the store had before this run (a resync after backfill): kept for backfill,
        /// never used to decide what this run may delete.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        old: Option<i64>,
    },
    /// Incremental from historyId `h`; `page` while a history listing spans pages.
    Hist {
        h: u64,
        page: Option<String>,
        /// The resume point is older than Gmail keeps history for; on a 404, catch up instead of a full sync.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        quiet: bool,
        /// Oldest date the store has from us: the window's floor, lowered by backfill. None = all mail.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lo: Option<i64>,
        /// A backfill in progress.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bf: Option<Backfill>,
        /// The next call is a backfill page (set after an incremental pass, with `more: true`).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        step: bool,
    },
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Filling in mail older than the window: `before:<before>` (and `after:<since>`), page by page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Backfill {
    before: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    since: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    page: Option<String>,
}

/// One message fetched during sync: its metadata, or None when Gmail no longer has it.
struct Fetched {
    id: String,
    meta: Option<MessageMeta>,
}

impl Cursor {
    fn parse(c: Option<&SyncCursor>) -> Option<Cursor> {
        let s = c?.0.trim();
        if let Ok(h) = s.parse::<u64>() {
            return Some(Cursor::Hist { h, page: None, quiet: false, lo: None, bf: None, step: false });
        }
        serde_json::from_str(s).ok()
    }

    /// None for no cursor; `ResyncNeeded` for one we cannot read (from another provider, or corrupt).
    fn read(c: Option<&SyncCursor>) -> Result<Option<Cursor>> {
        match c {
            None => Ok(None),
            Some(_) => Cursor::parse(c).map(Some).ok_or_else(|| Error::ResyncNeeded("not a gog sync cursor".into())),
        }
    }
    fn encode(&self) -> SyncCursor {
        SyncCursor(serde_json::to_string(self).unwrap_or_default())
    }
}

/// When the label list was fetched, the mailboxes it made, and the known label ids.
type LabelCache = (tokio::time::Instant, Vec<Mailbox>, HashSet<String>);

pub struct GogProvider {
    cfg: GogConfig,
    runner: Arc<dyn Runner>,
    mode: tokio::sync::OnceCell<ComposeMode>,
    /// local_id -> Gmail draft id, for drafts saved by this process.
    drafts: Mutex<HashMap<String, String>>,
    /// message id -> sorted label ids we last reported, so reconciling only refetches what changed.
    labels_seen: Mutex<HashMap<String, Vec<String>>>,
    /// Label name -> id from the last label list (gog's search output names labels).
    name_to_id: Mutex<HashMap<String, String>>,
    /// The last label list, and when it was fetched.
    labels_cache: Mutex<Option<LabelCache>>,
    features: tokio::sync::OnceCell<Features>,
    /// A backfill asked for by `request_backfill` (since), taken by the next incremental sync.
    backfill_req: Mutex<Option<Option<i64>>>,
    last_backfill: Mutex<Option<tokio::time::Instant>>,
}

fn valid_id(kind: &str, id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 256
        && !id.starts_with('-')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if ok { Ok(()) } else { Err(Error::Invalid(format!("not a Gmail {kind} id: {id:?}"))) }
}

fn valid_query(q: &str) -> Result<()> {
    if q.trim().is_empty() || q.trim_start().starts_with('-') {
        return Err(Error::Invalid(format!("search query must not be empty or start with '-': {q:?}")));
    }
    Ok(())
}

/// Gmail search operators take quoted phrases; quotes and backslashes inside are dropped.
fn quoted(s: &str) -> String {
    let clean: String = s.chars().filter(|c| !matches!(c, '"' | '\\') && !c.is_control()).collect();
    format!("\"{}\"", clean.trim())
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

fn parse_json<T: DeserializeOwned + Default>(what: &str, out: &GogOutput) -> Result<T> {
    if out.stdout.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(T::default());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| Error::Other(format!("{what}: unexpected JSON from gog: {e}")))
}

impl GogProvider {
    pub fn new(cfg: GogConfig, runner: Arc<dyn Runner>) -> Self {
        Self {
            cfg,
            runner,
            mode: tokio::sync::OnceCell::new(),
            drafts: Mutex::new(HashMap::new()),
            labels_seen: Mutex::new(HashMap::new()),
            name_to_id: Mutex::new(HashMap::new()),
            labels_cache: Mutex::new(None),
            features: tokio::sync::OnceCell::new(),
            backfill_req: Mutex::new(None),
            last_backfill: Mutex::new(None),
        }
    }

    pub fn config(&self) -> &GogConfig {
        &self.cfg
    }

    /// The full argument list for one gog command: the guards, then the command, then its arguments.
    pub fn args(&self, command: &str, rest: Vec<Arg>) -> Vec<Arg> {
        let mut a: Vec<Arg> = ["--no-input", "--color", "never", "--json", "--wrap-untrusted=false", "--account"]
            .into_iter()
            .map(Arg::from)
            .collect();
        a.push(Arg::from(self.cfg.email.as_str()));
        a.push(Arg::from("--enable-commands-exact"));
        a.push(Arg::from(command));
        if command != SEND_COMMAND {
            a.push(Arg::from("--gmail-no-send"));
        }
        a.extend(command.split('.').map(Arg::from));
        a.extend(rest);
        a
    }

    async fn call_with(&self, command: &str, rest: Vec<Arg>, stdin: Option<Vec<u8>>, inputs: Vec<(String, Vec<u8>)>, timeout: Duration) -> Result<GogOutput> {
        let mut call = GogCall::new(self.args(command, rest), timeout);
        call.stdin = stdin;
        call.inputs = inputs;
        let out = self.runner.run(call).await?;
        if out.code != 0 && out.code != crate::runner::EXIT_EMPTY {
            return Err(classify(command, &out));
        }
        Ok(out)
    }

    async fn call(&self, command: &str, rest: Vec<Arg>) -> Result<GogOutput> {
        self.call_with(command, rest, None, Vec::new(), self.cfg.timeout).await
    }

    async fn json<T: DeserializeOwned + Default>(&self, command: &str, rest: Vec<Arg>) -> Result<T> {
        let out = self.call(command, rest).await?;
        parse_json(command, &out)
    }

    // ---- reads ---------------------------------------------------------------------------------------

    /// What this gog can do (`gog version`, once). A failure to reach gog is an error (asked again next
    /// time); an answer we cannot read means "oldest supported".
    pub async fn features(&self) -> Result<Features> {
        self.features
            .get_or_try_init(|| async {
                match self.json::<api::Version>("version", vec![]).await {
                    Ok(v) => Ok(Features { version: v.parse() }),
                    Err(e @ (Error::Offline(_) | Error::Auth(_))) => Err(e),
                    Err(_) => Ok(Features { version: None }),
                }
            })
            .await
            .copied()
    }

    /// The label list: from the cache while it is younger than `labels_ttl` (then the delta says
    /// "mailboxes unchanged"), else fetched. Returns (Some(mailboxes) when fetched, known label ids).
    async fn labels_now(&self, force: bool) -> Result<(Option<Vec<Mailbox>>, HashSet<String>)> {
        if !force
            && let Some((at, _, known)) = self.labels_cache.lock().unwrap().as_ref()
            && at.elapsed() < self.cfg.labels_ttl
        {
            return Ok((None, known.clone()));
        }
        let labels = self.labels().await?;
        {
            let mut map = self.name_to_id.lock().unwrap();
            map.clear();
            for l in &labels {
                map.insert(l.name.clone(), l.id.clone());
            }
        }
        let mailboxes = convert::mailboxes(&self.cfg.account, &labels);
        let known: HashSet<String> = mailboxes.iter().map(|m| m.id.0.clone()).collect();
        *self.labels_cache.lock().unwrap() = Some((tokio::time::Instant::now(), mailboxes.clone(), known.clone()));
        Ok((Some(mailboxes), known))
    }

    async fn labels(&self) -> Result<Vec<api::Label>> {
        let list: api::LabelList = self.json("gmail.labels.list", vec![]).await?;
        let mut labels = list.labels;
        let wanted: Vec<usize> = labels
            .iter()
            .enumerate()
            .filter(|(_, l)| self.cfg.count_labels.iter().any(|c| c == &l.id))
            .map(|(i, _)| i)
            .collect();
        for i in wanted {
            let id = labels[i].id.clone();
            valid_id("label", &id)?;
            let got = self.json::<api::LabelGet>("gmail.labels.get", vec![Arg::from(id)]).await?.label;
            labels[i].messages_total = got.messages_total;
            labels[i].messages_unread = got.messages_unread;
        }
        Ok(labels)
    }

    async fn message(&self, id: &str, format: &str) -> Result<api::Message> {
        valid_id("message", id)?;
        self.json("gmail.raw", vec![Arg::from(id), Arg::from("--format"), Arg::from(format)]).await
    }

    /// Full metadata for a few messages, one `gmail raw` each: only for ids a listing did not have
    /// (rare: mail imported with an old date). Messages that are gone come back with `meta: None`.
    async fn metas(&self, ids: &[String], known: &HashSet<String>) -> Result<Vec<Fetched>> {
        let futs: Vec<BoxFuture<'_, Result<Option<api::Message>>>> = ids
            .iter()
            .map(|id| {
                Box::pin(async move {
                    match self.message(id, "metadata").await {
                        Ok(m) => Ok(Some(m)),
                        Err(Error::NotFound(_)) => Ok(None),
                        Err(e) => Err(e),
                    }
                }) as BoxFuture<'_, _>
            })
            .collect();
        let results = bounded(self.cfg.concurrency, futs).await;
        let mut out = Vec::with_capacity(results.len());
        let mut seen = self.labels_seen.lock().unwrap();
        for (id, r) in ids.iter().zip(results) {
            match r? {
                Some(m) => {
                    seen.insert(id.clone(), sorted(m.label_ids.clone()));
                    out.push(Fetched { id: id.clone(), meta: Some(convert::meta(&self.cfg.account, &m, Some(known))) });
                }
                None => {
                    seen.remove(id);
                    out.push(Fetched { id: id.clone(), meta: None });
                }
            }
        }
        Ok(out)
    }

    /// Plain search (ids and labels only): for draft and sent lookups.
    async fn search(&self, query: &str, max: u32, page: Option<&str>) -> Result<api::MessageSearch> {
        valid_query(query)?;
        let mut args = vec![Arg::from(query), Arg::from("--max"), Arg::from(max.max(1).to_string())];
        if let Some(p) = page.filter(|p| !p.is_empty()) {
            args.push(Arg::from("--page"));
            args.push(Arg::from(p));
        }
        self.json("gmail.messages.search", args).await
    }

    /// One listing with everything this gog can put in it: the sync's only per-page call.
    async fn listing(&self, query: &str, max: u32, page: Option<&str>) -> Result<api::MessageSearch> {
        valid_query(query)?;
        let f = self.features().await?;
        let mut args = vec![Arg::from(query), Arg::from("--max"), Arg::from(max.clamp(1, 500).to_string())];
        if let Some(p) = page.filter(|p| !p.is_empty()) {
            args.push(Arg::from("--page"));
            args.push(Arg::from(p));
        }
        args.extend(["--timezone", "UTC"].map(Arg::from));
        if self.cfg.search_bodies {
            args.push(Arg::from("--include-body"));
        }
        if f.recipients() {
            args.push(Arg::from("--include-recipients"));
        }
        if f.headers() {
            args.push(Arg::from("--headers"));
            args.push(Arg::from(SEARCH_HEADERS));
        }
        self.json("gmail.messages.search", args).await
    }

    /// Records from listed rows, remembering their labels.
    fn listed(&self, items: &[api::MessageItem], known: &HashSet<String>) -> Vec<MessageMeta> {
        let by_name = self.name_to_id.lock().unwrap().clone();
        let mut seen = self.labels_seen.lock().unwrap();
        items
            .iter()
            .filter(|i| !i.id.is_empty() && valid_id("message", &i.id).is_ok())
            .map(|i| {
                seen.insert(i.id.clone(), convert::item_label_ids(i, &by_name));
                convert::meta_from_item(&self.cfg.account, i, &by_name, Some(known))
            })
            .collect()
    }

    // ---- sync ----------------------------------------------------------------------------------------

    /// One page of a full sync: one listing (plus, on the first page, one `gmail raw --format minimal`
    /// for the resume point, and on the last, one history probe).
    async fn full_page(&self, start_h: u64, page: Option<&str>, n: u32, q: Option<String>, lo: Option<i64>, old: Option<i64>) -> Result<Delta> {
        let first = page.is_none();
        let (mailboxes, known) = self.labels_now(first).await?;
        let (query, windowed) = match q {
            Some(q) => (q, true),
            None if !first => (self.cfg.full_sync_query.clone(), false),
            None => self.cfg.window_query(),
        };
        let remaining = if windowed && self.cfg.window_max > 0 { self.cfg.window_max.saturating_sub(n) } else { u32::MAX };
        let max = self.cfg.page_size.min(remaining).max(1);
        let found = self.listing(&query, max, page).await?;
        let metas = self.listed(&found.messages, &known);
        let mut h = start_h;
        if start_h == 0
            && let Some(newest) = metas.first()
        {
            // The resume point is fixed on the first page: anything that changes later shows up in history.
            match self.message(newest.id.as_str(), "minimal").await {
                Ok(m) => h = m.history_id,
                Err(Error::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        let n = n.saturating_add(metas.len() as u32);
        let lo = metas.iter().map(|m| m.date).filter(|d| *d > 0).chain(lo).min();
        let mut delta = Delta { mailboxes, upserted: metas, ..Default::default() };
        // Every page of a full sync lists everything (in the window): the engine drops what none reported.
        delta.reset = true;
        delta.window_floor = windowed.then(|| self.window_floor(lo));
        let window_full = windowed && self.cfg.window_max > 0 && n >= self.cfg.window_max;
        delta.more = !found.next_page_token.is_empty() && !window_full;
        delta.cursor = if delta.more {
            Some(Cursor::Full { h, page: Some(found.next_page_token), n, q: windowed.then_some(query), lo, old }.encode())
        } else if h > 0 {
            let (h, quiet) = self.probe(h).await?;
            // What backfill starts from: the window's floor, or older mail the store kept from before.
            let lo = delta.window_floor.map(|f| old.map_or(f, |o| o.min(f)));
            let bf = if self.cfg.backfill_auto { lo.map(|before| Backfill { before, since: None, page: None }) } else { None };
            Some(Cursor::Hist { h, page: None, quiet, lo, bf, step: false }.encode())
        } else {
            None
        };
        Ok(delta)
    }

    /// The oldest date a windowed run speaks for: the oldest date it listed, but never earlier than one
    /// day inside `newer_than:<days>d` (a message whose Date header is far older than when Gmail got it
    /// must not let the run delete stored mail outside the window). With nothing listed, the day limit,
    /// or "nothing" (`i64::MAX`). Higher is always safe: the store deletes less.
    fn window_floor(&self, lo: Option<i64>) -> i64 {
        let by_days = (self.cfg.window_days > 0).then(|| now() - i64::from(self.cfg.window_days) * 86_400 + 86_400);
        match (lo, by_days) {
            (Some(l), Some(d)) => l.max(d),
            (Some(l), None) => l,
            (None, Some(d)) => d,
            (None, None) => i64::MAX,
        }
    }

    /// Check that history can resume from `h`, and move it up to the mailbox's current historyId when
    /// nothing happened in between. `quiet` = Gmail no longer keeps history that old (a mailbox where
    /// even the newest mail has not changed for a long time).
    async fn probe(&self, h: u64) -> Result<(u64, bool)> {
        let args = vec![Arg::from("--since"), Arg::from(h.to_string()), Arg::from("--max"), Arg::from("1")];
        match self.json::<api::History>("gmail.history", args).await {
            Ok(r) if r.messages.is_empty() && r.next_page_token.is_empty() => Ok((r.history_id.max(h), false)),
            Ok(_) => Ok((h, false)),
            Err(Error::NotFound(_)) => Ok((h, true)),
            Err(e) => Err(e),
        }
    }

    /// A backfill asked for since the last sync, turned into the cursor's state.
    fn take_backfill(&self, lo: Option<i64>, bf: Option<Backfill>) -> Option<Backfill> {
        let Some(since) = self.backfill_req.lock().unwrap().take() else { return bf };
        let Some(before) = lo else {
            // Not windowed: the store already has everything.
            return bf;
        };
        if since.is_some_and(|s| s >= before) {
            return bf;
        }
        Some(Backfill { before: bf.as_ref().map_or(before, |b| b.before), since, page: bf.and_then(|b| b.page) })
    }

    fn backfill_due(&self) -> bool {
        self.last_backfill.lock().unwrap().is_none_or(|t| t.elapsed() >= self.cfg.backfill_every)
    }

    #[allow(clippy::too_many_arguments)]
    async fn incremental(&self, h: u64, page: Option<&str>, quiet: bool, lo: Option<i64>, bf: Option<Backfill>, step: bool) -> Result<Delta> {
        let bf = self.take_backfill(lo, bf);
        if step && let Some(b) = bf {
            return self.backfill_page(h, quiet, lo, b).await;
        }
        let mut args = vec![Arg::from("--since"), Arg::from(h.to_string()), Arg::from("--max"), Arg::from(self.cfg.history_page.max(1).to_string())];
        if let Some(p) = page.filter(|p| !p.is_empty()) {
            args.push(Arg::from("--page"));
            args.push(Arg::from(p));
        }
        let hist: api::History = match self.json("gmail.history", args).await {
            Ok(r) => r,
            // A quiet mailbox's newest historyId can be older than Gmail keeps: catch up from the top.
            Err(Error::NotFound(_)) if quiet => return self.catch_up(h, lo, bf).await,
            // History expired (Gmail keeps about a week): start over with a (windowed) full sync. The
            // store keeps everything older than the new window.
            Err(Error::NotFound(_)) => return self.full_page(0, None, 0, None, None, lo).await,
            Err(e) => return Err(e),
        };
        let (mailboxes, known) = self.labels_now(false).await?;
        let mut ids: Vec<String> = Vec::new();
        for id in hist.messages.into_iter().filter(|i| !i.is_empty()) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        let more = !hist.next_page_token.is_empty();
        let mut delta = Delta { mailboxes, ..Default::default() };
        // One listing of recent mail gives the new messages' records and the label changes at once.
        let recent = if !ids.is_empty() || (!more && self.cfg.reconcile_query.is_some()) {
            let q = self.cfg.reconcile_query.clone().unwrap_or_else(|| "in:anywhere newer_than:7d".into());
            let max = self.cfg.reconcile_max.max((ids.len() as u32).saturating_add(10)).min(100);
            let changed_before = self.labels_seen.lock().unwrap().clone();
            let found = self.listing(&q, max, None).await?;
            Some((self.listed(&found.messages, &known), changed_before))
        } else {
            None
        };
        let mut have: HashSet<String> = HashSet::new();
        if let Some((metas, before)) = recent {
            let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
            for m in metas {
                let id = m.id.0.clone();
                let new = wanted.contains(id.as_str());
                // Reconcile: only records whose labels differ from what we last reported.
                let changed = !more
                    && self.cfg.reconcile_query.is_some()
                    && before.get(&id) != self.labels_seen.lock().unwrap().get(&id);
                if new || changed {
                    have.insert(id);
                    delta.upserted.push(m);
                }
            }
        }
        let missing: Vec<String> = ids.into_iter().filter(|i| !have.contains(i)).collect();
        for f in self.metas(&missing, &known).await? {
            match f.meta {
                Some(m) => delta.upserted.push(m),
                None => delta.removed.push(MessageId::new(f.id)),
            }
        }
        delta.more = more;
        let next_h = if more { h } else { hist.history_id.max(h) };
        let step = !more && bf.is_some() && self.backfill_due();
        if step {
            // Come straight back for one backfill page.
            delta.more = true;
        }
        let page = more.then_some(hist.next_page_token);
        delta.cursor = Some(Cursor::Hist { h: next_h, page, quiet: false, lo, bf, step }.encode());
        Ok(delta)
    }

    /// One page of older mail: `in:anywhere before:<floor> [after:<since>]`, stored but not announced.
    async fn backfill_page(&self, h: u64, quiet: bool, lo: Option<i64>, mut b: Backfill) -> Result<Delta> {
        *self.last_backfill.lock().unwrap() = Some(tokio::time::Instant::now());
        let (mailboxes, known) = self.labels_now(false).await?;
        let mut q = format!("{} before:{}", self.cfg.full_sync_query, b.before);
        if let Some(since) = b.since {
            q.push_str(&format!(" after:{since}"));
        }
        let found = self.listing(&q, self.cfg.backfill_page, b.page.as_deref()).await?;
        let metas = self.listed(&found.messages, &known);
        let done = found.next_page_token.is_empty();
        let (lo, bf) = if done {
            (b.since, None)
        } else {
            b.page = Some(found.next_page_token);
            (lo, Some(b))
        };
        Ok(Delta {
            mailboxes,
            upserted: metas,
            backfill: true,
            cursor: Some(Cursor::Hist { h, page: None, quiet, lo, bf, step: false }.encode()),
            ..Default::default()
        })
    }

    /// For a quiet mailbox: the newest page (only what changed), and a fresh resume point.
    async fn catch_up(&self, h: u64, lo: Option<i64>, bf: Option<Backfill>) -> Result<Delta> {
        let (mailboxes, known) = self.labels_now(false).await?;
        let before = self.labels_seen.lock().unwrap().clone();
        let found = self.listing(&self.cfg.full_sync_query, self.cfg.page_size, None).await?;
        let metas = self.listed(&found.messages, &known);
        let mut delta = Delta { mailboxes, ..Default::default() };
        let mut newest = h;
        if let Some(first) = metas.first() {
            match self.message(first.id.as_str(), "minimal").await {
                Ok(m) => newest = newest.max(m.history_id),
                Err(Error::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        {
            let seen = self.labels_seen.lock().unwrap();
            delta.upserted = metas.into_iter().filter(|m| before.get(&m.id.0) != seen.get(&m.id.0)).collect();
        }
        let (h, quiet) = self.probe(newest).await?;
        delta.cursor = Some(Cursor::Hist { h, page: None, quiet, lo, bf, step: false }.encode());
        Ok(delta)
    }

    /// A whole thread in one call (`gmail thread get`, format full): bodies and full records for every
    /// message in it.
    async fn thread_bundle(&self, id: &MessageId, thread: &ThreadId) -> Result<BodyBundle> {
        valid_id("thread", thread.as_str())?;
        let got: api::ThreadGet = self.json("gmail.thread.get", vec![Arg::from(thread.as_str())]).await?;
        let known = self.labels_cache.lock().unwrap().as_ref().map(|(_, _, k)| k.clone());
        let mut bundle = BodyBundle::default();
        let mut found = false;
        {
            let mut seen = self.labels_seen.lock().unwrap();
            for m in &got.thread.messages {
                if m.id.is_empty() || valid_id("message", &m.id).is_err() {
                    continue;
                }
                seen.insert(m.id.clone(), sorted(m.label_ids.clone()));
                bundle.metas.push(convert::meta(&self.cfg.account, m, known.as_ref()));
                let body = convert::body(m);
                if m.id == id.as_str() {
                    bundle.body = body;
                    found = true;
                } else {
                    bundle.others.push((MessageId::new(&m.id), body));
                }
            }
        }
        if !found {
            // Moved to another thread meanwhile, or the thread hint was wrong: just this message.
            bundle.body = convert::body(&self.message(id.as_str(), "full").await?);
        }
        Ok(bundle)
    }

    // ---- actions -------------------------------------------------------------------------------------

    async fn modify(&self, ids: &[MessageId], add: &[&str], remove: &[&str]) -> Result<()> {
        if ids.is_empty() || (add.is_empty() && remove.is_empty()) {
            return Ok(());
        }
        for id in ids {
            valid_id("message", id.as_str())?;
        }
        for l in add.iter().chain(remove) {
            if l.contains(',') {
                return Err(Error::Invalid(format!("label id with a comma: {l:?}")));
            }
            valid_id("label", l)?;
        }
        // Gmail's batchModify takes up to 1000 ids.
        for chunk in ids.chunks(1000) {
            let mut args: Vec<Arg> = chunk.iter().map(|i| Arg::from(i.as_str())).collect();
            if !add.is_empty() {
                args.push(Arg::from("--add"));
                args.push(Arg::from(add.join(",")));
            }
            if !remove.is_empty() {
                args.push(Arg::from("--remove"));
                args.push(Arg::from(remove.join(",")));
            }
            match self.call("gmail.batch.modify", args).await {
                // A message deleted meanwhile: nothing left to change.
                Ok(_) | Err(Error::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn compose_mode(&self) -> ComposeMode {
        match self.cfg.compose {
            ComposeMode::Auto => *self
                .mode
                .get_or_init(|| async {
                    match self.features().await {
                        Ok(f) if f.raw_compose() => ComposeMode::Raw,
                        _ => ComposeMode::Flags,
                    }
                })
                .await,
            m => m,
        }
    }

    async fn load_attachments(draft: &Draft) -> Result<Vec<LoadedAttachment>> {
        let mut out = Vec::new();
        for a in &draft.attachments {
            let data = tokio::fs::read(&a.path).await.map_err(|e| Error::Invalid(format!("attachment {}: {e}", a.filename)))?;
            out.push(LoadedAttachment { filename: a.filename.clone(), mime: a.mime.clone(), data });
        }
        Ok(out)
    }

    /// The message being replied to: its thread, and the threading headers for ours.
    async fn reply_context(&self, draft: &Draft) -> Result<(Option<String>, Option<String>, Vec<String>)> {
        let mut thread = None;
        let mut irt = draft.in_reply_to.clone();
        let mut refs = draft.references.clone();
        if let Some(parent) = &draft.reply_to {
            match self.message(parent.as_str(), "metadata").await {
                Ok(m) => {
                    let meta = convert::meta(&self.cfg.account, &m, None);
                    thread = Some(meta.thread.0.clone());
                    if irt.is_none() {
                        irt = meta.rfc_message_id.clone();
                    }
                    if refs.is_empty() {
                        refs = mime::reply_references(&meta.references, meta.rfc_message_id.as_deref());
                    }
                }
                // The parent is gone: send unthreaded rather than not at all.
                Err(Error::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok((thread, irt, refs))
    }

    async fn raw_mime(&self, draft: &Draft) -> Result<(Vec<u8>, String, Option<String>)> {
        let mid = mime::message_id_for(&draft.local_id, &self.cfg.email)?;
        let (thread, in_reply_to, references) = self.reply_context(draft).await?;
        let env = Envelope {
            from: Address::new(self.cfg.name.as_deref(), &self.cfg.email),
            message_id: mid.clone(),
            date: now(),
            in_reply_to,
            references,
        };
        let bytes = mime::build(draft, &env, &Self::load_attachments(draft).await?)?;
        Ok((bytes, mid, thread))
    }

    /// Flags-mode compose arguments shared by `drafts create|update` and `send`. Text goes on stdin.
    async fn compose_flags(&self, draft: &Draft, update: bool) -> Result<(Vec<Arg>, Vec<u8>, Vec<(String, Vec<u8>)>)> {
        let list = |v: &[Address]| -> String {
            v.iter()
                .filter(|a| a.email.contains('@') && !a.email.contains(','))
                .map(|a| match a.name.as_deref() {
                    Some(n) => {
                        let n: String = n.chars().filter(|c| !matches!(c, ',' | '"' | '<' | '>' | '\r' | '\n')).collect();
                        format!("{} <{}>", n.trim(), a.email)
                    }
                    None => a.email.clone(),
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        let mut args = Vec::new();
        for (flag, v) in [("--to", &draft.to), ("--cc", &draft.cc), ("--bcc", &draft.bcc)] {
            let s = list(v);
            if !s.is_empty() {
                args.push(Arg::from(flag));
                args.push(Arg::from(s));
            }
        }
        args.push(Arg::from("--subject"));
        args.push(Arg::from(draft.subject.replace(['\r', '\n'], " ")));
        args.push(Arg::from("--body-file"));
        args.push(Arg::from("-"));
        let mut inputs = Vec::new();
        if let Some(html) = draft.html.as_ref().filter(|h| !h.trim().is_empty()) {
            inputs.push(("body.html".to_owned(), html.clone().into_bytes()));
            args.push(Arg::from("--body-html-file"));
            args.push(Arg::Input("body.html".into()));
        }
        if let Some(parent) = &draft.reply_to {
            valid_id("message", parent.as_str())?;
            args.push(Arg::from("--reply-to-message-id"));
            args.push(Arg::from(parent.as_str()));
        }
        let atts = Self::load_attachments(draft).await?;
        if update && atts.is_empty() {
            args.push(Arg::from("--clear-attachments"));
        }
        for (i, a) in atts.into_iter().enumerate() {
            // gog names the attachment after the file, so the staged file keeps the real name.
            let name = format!("a{i}/{}", staged_name(&a.filename));
            args.push(Arg::from("--attach"));
            args.push(Arg::Input(name.clone()));
            inputs.push((name, a.data));
        }
        Ok((args, draft.text.clone().into_bytes(), inputs))
    }

    /// The Gmail draft for one of our drafts, if we can tell exactly: the id the store handed back
    /// (`Draft::server_id`), else one this process saved, else (raw mode) a lookup by our Message-ID.
    async fn find_draft(&self, local_id: &str, server_id: Option<&str>, mode: ComposeMode) -> Result<Option<String>> {
        if let Some(id) = server_id.filter(|s| !s.is_empty()) {
            return Ok(Some(id.to_owned()));
        }
        if let Some(id) = self.drafts.lock().unwrap().get(local_id).cloned() {
            return Ok(Some(id));
        }
        if mode != ComposeMode::Raw {
            // Without our own Message-ID there is no exact way to find it, and a guess could overwrite
            // or delete someone else's draft. Saving again makes a new draft instead (see README).
            return Ok(None);
        }
        let mid = mime::message_id_for(local_id, &self.cfg.email)?;
        let hits = self.search(&format!("in:drafts rfc822msgid:{mid}"), 5, None).await?;
        let msg_ids: HashSet<String> = hits.messages.into_iter().map(|m| m.id).collect();
        if msg_ids.is_empty() {
            return Ok(None);
        }
        let list: api::DraftList = self.json("gmail.drafts.list", vec![Arg::from("--max"), Arg::from("100"), Arg::from("--all")]).await?;
        let found = list.drafts.into_iter().find(|d| msg_ids.contains(&d.message_id)).map(|d| d.id);
        if let Some(id) = &found {
            self.drafts.lock().unwrap().insert(local_id.to_owned(), id.clone());
        }
        Ok(found)
    }

    async fn save_draft(&self, draft: &Draft) -> Result<ApplyOutcome> {
        let mode = self.compose_mode().await;
        let existing = self.find_draft(&draft.local_id, draft.server_id.as_deref(), mode).await?;
        let (args, stdin, inputs) = match mode {
            ComposeMode::Raw => {
                let (bytes, _mid, thread) = self.raw_mime(draft).await?;
                let mut args = vec![Arg::from("--raw-file"), Arg::from("-")];
                if let Some(t) = thread {
                    valid_id("thread", &t)?;
                    args.push(Arg::from("--thread-id"));
                    args.push(Arg::from(t));
                }
                (args, bytes, Vec::new())
            }
            _ => {
                let blank = draft.subject.trim().is_empty() && draft.text.trim().is_empty()
                    && draft.html.as_deref().is_none_or(|h| h.trim().is_empty()) && draft.attachments.is_empty();
                if blank {
                    // gog 0.29 cannot store an empty draft; there is nothing to lose by waiting.
                    return Ok(ApplyOutcome { server_id: existing });
                }
                self.compose_flags(draft, existing.is_some()).await?
            }
        };
        if let Some(id) = &existing {
            valid_id("draft", id)?;
            let mut a = vec![Arg::from(id.as_str())];
            a.extend(args.iter().cloned());
            match self.call_with("gmail.drafts.update", a, Some(stdin.clone()), inputs.clone(), self.cfg.timeout).await {
                Ok(_) => {
                    self.drafts.lock().unwrap().insert(draft.local_id.clone(), id.clone());
                    return Ok(ApplyOutcome::with_id(id.clone()));
                }
                // Deleted elsewhere: make a new one below.
                Err(Error::NotFound(_)) => {
                    self.drafts.lock().unwrap().remove(&draft.local_id);
                }
                Err(e) => return Err(e),
            }
        }
        // A create after an update failed for flags mode must not carry --clear-attachments.
        let args = if existing.is_some() && mode != ComposeMode::Raw { self.compose_flags(draft, false).await?.0 } else { args };
        let out = self.call_with("gmail.drafts.create", args, Some(stdin), inputs, self.cfg.timeout).await?;
        let res: api::DraftResult = parse_json("gmail.drafts.create", &out)?;
        if res.draft_id.is_empty() {
            return Ok(ApplyOutcome::default());
        }
        self.drafts.lock().unwrap().insert(draft.local_id.clone(), res.draft_id.clone());
        Ok(ApplyOutcome::with_id(res.draft_id))
    }

    async fn delete_draft(&self, local_id: &str, server_id: Option<&str>) -> Result<()> {
        let mode = self.compose_mode().await;
        let Some(id) = self.find_draft(local_id, server_id, mode).await? else { return Ok(()) };
        valid_id("draft", &id)?;
        // Drafts are not moved to Trash: gog asks for confirmation, which --force gives (we never prompt).
        match self.call("gmail.drafts.delete", vec![Arg::from(id.as_str()), Arg::from("--force")]).await {
            Ok(_) | Err(Error::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        self.drafts.lock().unwrap().remove(local_id);
        Ok(())
    }

    /// Has this draft already gone out? (A send that timed out may still have been delivered.)
    async fn already_sent(&self, draft: &Draft, mode: ComposeMode, mid: Option<&str>) -> Result<bool> {
        if mode == ComposeMode::Raw {
            let Some(mid) = mid else { return Ok(false) };
            let hits = self.search(&format!("in:sent rfc822msgid:{mid}"), 1, None).await?;
            return Ok(!hits.messages.is_empty());
        }
        if draft.subject.trim().is_empty() {
            return Ok(false);
        }
        let q = format!("in:sent newer_than:1d subject:{}", quoted(&draft.subject));
        let hits = self.search(&q, 10, None).await?;
        let want_to: Vec<String> = sorted(draft.to.iter().map(|a| a.email.clone()).collect());
        let want_text: String = convert::snippet(&draft.text).chars().take(60).collect();
        let cutoff = now() - self.cfg.dedupe_window_s;
        for hit in hits.messages {
            let m = match self.message(&hit.id, "metadata").await {
                Ok(m) => m,
                Err(Error::NotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            let meta = convert::meta(&self.cfg.account, &m, None);
            let to: Vec<String> = sorted(meta.to.iter().map(|a| a.email.clone()).collect());
            let snip: String = meta.snippet.chars().take(60).collect();
            if meta.date >= cutoff && meta.subject == draft.subject.split_whitespace().collect::<Vec<_>>().join(" ")
                && to == want_to && !want_text.is_empty() && snip == want_text
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn send(&self, draft: &Draft) -> Result<ApplyOutcome> {
        if draft.to.is_empty() && draft.cc.is_empty() && draft.bcc.is_empty() {
            return Err(Error::Invalid("no recipients".into()));
        }
        let mode = self.compose_mode().await;
        let (args, stdin, inputs, mid) = match mode {
            ComposeMode::Raw => {
                let (bytes, mid, thread) = self.raw_mime(draft).await?;
                let mut args = vec![Arg::from("--raw-file"), Arg::from("-")];
                if let Some(t) = thread {
                    valid_id("thread", &t)?;
                    args.push(Arg::from("--thread-id"));
                    args.push(Arg::from(t));
                }
                (args, bytes, Vec::new(), Some(mid))
            }
            _ => {
                let (a, s, i) = self.compose_flags(draft, false).await?;
                (a, s, i, None)
            }
        };
        let mut outcome = ApplyOutcome::default();
        if !self.already_sent(draft, mode, mid.as_deref()).await? {
            let out = self.call_with(SEND_COMMAND, args, Some(stdin), inputs, self.cfg.send_timeout).await?;
            // The message is out: a reply we cannot read must not fail (and so repeat) the send.
            if let Ok(sent) = parse_json::<api::SendResult>(SEND_COMMAND, &out)
                && !sent.message_id.is_empty()
            {
                outcome.server_id = Some(sent.message_id);
            }
        }
        // The draft it came from is done with. A failure here must not make the queue send again.
        let _ = self.delete_draft(&draft.local_id, draft.server_id.as_deref()).await;
        Ok(outcome)
    }
}

fn staged_name(filename: &str) -> String {
    let base = filename.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' ') { c } else { '_' })
        .collect();
    let clean = clean.trim().trim_start_matches('.').to_owned();
    if clean.is_empty() || clean.chars().all(|c| c == '.') { "attachment".into() } else { clean.chars().take(120).collect() }
}

/// Run futures with at most `n` in flight; results in input order.
async fn bounded<T>(n: usize, futs: Vec<BoxFuture<'_, T>>) -> Vec<T> {
    futures::stream::iter(futs).buffered(n.max(1)).collect().await
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl GogProvider {
    async fn sync_inner(&self, cursor: Option<&SyncCursor>) -> Result<Delta> {
        match Cursor::read(cursor)? {
            None => self.full_page(0, None, 0, None, None, None).await,
            Some(Cursor::Full { h, page, n, q, lo, old }) => self.full_page(h, page.as_deref(), n, q, lo, old).await,
            Some(Cursor::Hist { h, page, quiet, lo, bf, step }) => self.incremental(h, page.as_deref(), quiet, lo, bf, step).await,
        }
    }
}

#[async_trait::async_trait]
impl Provider for GogProvider {
    fn account(&self) -> &AccountId {
        &self.cfg.account
    }

    async fn sync(&self, cursor: Option<&SyncCursor>) -> Result<Delta> {
        self.sync_inner(cursor).await
    }

    async fn fetch_body(&self, id: &MessageId) -> Result<Body> {
        let m = self.message(id.as_str(), "full").await?;
        Ok(convert::body(&m))
    }

    async fn fetch_raw(&self, id: &MessageId) -> Result<Vec<u8>> {
        let m = self.message(id.as_str(), "raw").await?;
        let raw = m.raw.ok_or_else(|| Error::Other("gmail.raw: no raw field in the reply".into()))?;
        convert::b64url(&raw).ok_or_else(|| Error::Other("gmail.raw: raw field is not base64url".into()))
    }

    async fn fetch_attachment(&self, message: &MessageId, attachment: &AttachmentId) -> Result<Vec<u8>> {
        let m = self.message(message.as_str(), "full").await?;
        let root = m.payload.as_ref().ok_or_else(|| Error::NotFound(format!("{message} has no payload")))?;
        let part = convert::find_part(root, attachment).ok_or_else(|| Error::NotFound(format!("attachment {attachment} in {message}")))?;
        let body = part.body.as_ref();
        if let Some(data) = body.and_then(|b| b.data.as_deref()).filter(|d| !d.is_empty()) {
            return convert::b64url(data).ok_or_else(|| Error::Other("attachment data is not base64url".into()));
        }
        let Some(att_id) = body.and_then(|b| b.attachment_id.clone()).filter(|a| !a.is_empty()) else {
            return Ok(Vec::new());
        };
        if att_id.starts_with('-') || att_id.contains(char::is_whitespace) {
            return Err(Error::Invalid("odd attachment id from Gmail".into()));
        }
        let out = self
            .call("gmail.attachment", vec![Arg::from(message.as_str()), Arg::from(att_id), Arg::from("--out"), Arg::Out])
            .await?;
        out.file.ok_or_else(|| Error::Other("gmail.attachment: gog wrote no file".into()))
    }

    async fn apply(&self, action: &Action) -> Result<ApplyOutcome> {
        let done = |r: Result<()>| r.map(|()| ApplyOutcome::default());
        match action {
            Action::SetSeen { ids, seen: true } => done(self.modify(ids, &[], &["UNREAD"]).await),
            Action::SetSeen { ids, seen: false } => done(self.modify(ids, &["UNREAD"], &[]).await),
            Action::SetFlagged { ids, flagged: true } => done(self.modify(ids, &["STARRED"], &[]).await),
            Action::SetFlagged { ids, flagged: false } => done(self.modify(ids, &[], &["STARRED"]).await),
            Action::Move { ids, from, to } => {
                let to = to.as_str();
                let mut add: Vec<&str> = Vec::new();
                let mut remove: Vec<&str> = Vec::new();
                if to != ALL_MAIL {
                    add.push(to);
                }
                if let Some(f) = from.as_ref().map(MailboxId::as_str).filter(|f| *f != ALL_MAIL && *f != to) {
                    remove.push(f);
                }
                // Like Gmail's own Trash and Spam buttons: out of the inbox too.
                if (to == "TRASH" || to == "SPAM") && !remove.contains(&"INBOX") {
                    remove.push("INBOX");
                }
                done(self.modify(ids, &add, &remove).await)
            }
            Action::Archive { ids } => done(self.modify(ids, &[], &["INBOX"]).await),
            Action::Trash { ids } => done(self.modify(ids, &["TRASH"], &["INBOX"]).await),
            Action::SaveDraft { draft } => self.save_draft(draft).await,
            Action::DeleteDraft { local_id, server_id } => done(self.delete_draft(local_id, server_id.as_deref()).await),
            Action::Send { draft } => self.send(draft).await,
        }
    }

    async fn wait_for_change(&self, timeout_s: u64) -> Result<()> {
        // No push without Pub/Sub; the engine syncs after this returns.
        tokio::time::sleep(Duration::from_secs(timeout_s)).await;
        Ok(())
    }

    async fn fetch_body_bundle(&self, id: &MessageId, thread: Option<&ThreadId>) -> Result<BodyBundle> {
        match thread {
            Some(t) => self.thread_bundle(id, t).await,
            None => Ok(BodyBundle { body: self.fetch_body(id).await?, ..Default::default() }),
        }
    }

    async fn request_backfill(&self, since: Option<i64>) -> Result<()> {
        *self.backfill_req.lock().unwrap() = Some(since);
        Ok(())
    }

    fn load(&self) -> Option<ProviderLoad> {
        self.runner.load()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_accept_bare_history_ids() {
        let c = Cursor::Full { h: 9, page: Some("tok".into()), n: 0, q: None, lo: None, old: None };
        assert_eq!(Cursor::parse(Some(&c.encode())), Some(c));
        assert_eq!(Cursor::parse(Some(&SyncCursor("12345".into()))), Some(Cursor::Hist { h: 12345, page: None, quiet: false, lo: None, bf: None, step: false }));
        // Cursors written before windows existed still read.
        assert_eq!(
            Cursor::parse(Some(&SyncCursor(r#"{"k":"full","h":9,"page":"tok"}"#.into()))),
            Some(Cursor::Full { h: 9, page: Some("tok".into()), n: 0, q: None, lo: None, old: None })
        );
        assert_eq!(Cursor::parse(Some(&SyncCursor("garbage".into()))), None);
        assert_eq!(Cursor::parse(None), None);
    }

    #[test]
    fn ids_and_queries_cannot_become_flags() {
        assert!(valid_id("message", "18c2f1a9b0d3e4f5").is_ok());
        assert!(valid_id("draft", "r-1234567890").is_ok());
        assert!(valid_id("message", "--gmail-no-send=false").is_err());
        assert!(valid_id("message", "a b").is_err());
        assert!(valid_id("message", "").is_err());
        assert!(valid_query("-in:spam").is_err());
        assert!(valid_query("in:anywhere").is_ok());
        assert_eq!(quoted("say \"hi\" \\ now"), "\"say hi  now\"");
    }

    #[test]
    fn staged_names_keep_the_filename_but_nothing_dangerous() {
        assert_eq!(staged_name("report.pdf"), "report.pdf");
        assert_eq!(staged_name("../../etc/passwd"), "passwd");
        assert_eq!(staged_name("my $(evil).txt"), "my __evil_.txt");
        assert_eq!(staged_name(".."), "attachment");
        assert_eq!(staged_name(".hidden"), "hidden");
    }
}
