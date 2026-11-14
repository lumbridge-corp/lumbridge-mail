//! The one-way Postgres mirror (`[pg]` in the config, off by default).
//!
//! SQLite stays the daemon's working store; Postgres is a copy for other devices and agent tools,
//! written only from here. The store's triggers note what changed in a durable outbox (schema V4,
//! `lumbridge_mail_store::OutboxItem`); a background task drains it in batches on its own SQLite
//! connection and its own Postgres connection, so sync never waits for it. Each entry is resolved
//! against the store's current rows when it is sent, so sending is idempotent and a restart or a
//! Postgres outage only delays it (retries back off up to `[pg] max_backoff_s`).
//!
//! The privacy rules, all here and all tested (`tests/pg_mirror.rs`):
//! - Anon accounts are never mirrored, not even their account row. Accounts with `mirror = false`
//!   neither. Either kind that is in Postgres (the rule changed) is deleted from it, with all its mail.
//! - Sensitive mail (`safety::sensitive_kind` on sender, subject, snippet or body; or a thread triaged
//!   `sensitive`) is mirrored as metadata with its fixed one line as subject: no snippet, no body, no
//!   attachments, nothing for embeddings or parsing. The schema refuses those too.
//! - Bodies go as clean text and the sanitised block tree, never the received HTML.

use crate::config::PgSection;
use crate::engine::{Shared, now};
use lumbridge_mail_core::safety::sensitive_sender_kind;
use lumbridge_mail_core::*;
use lumbridge_mail_pg::{
    AccountRow, BodyChange, BodyRow, Change, ConnectOptions, MailboxRow, MessageRow, TriageRow,
    Writer,
};
use lumbridge_mail_store::{MirrorState, OutboxItem, OutboxKind, Store};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// The running mirror, kept in `Shared`.
#[derive(Default)]
pub(crate) struct MirrorHandle {
    running: Mutex<Option<Running>>,
    wake: Arc<Notify>,
    live: Arc<Mutex<Live>>,
}

struct Running {
    settings: Settings,
    task: JoinHandle<()>,
}

/// What the task was started with; a change restarts it (and reconciles the accounts).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    dsn_file: String,
    ca_file: Option<String>,
    batch: u32,
    poll_ms: u64,
    max_backoff_s: u64,
    /// Accounts with `mirror = false`.
    excluded: BTreeSet<AccountId>,
}

/// What the task is doing, for `lbmail pg status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Live {
    pub connected: bool,
    pub last_batch_items: usize,
    pub last_batch_ms: u64,
    pub last_batch_at: Option<i64>,
    /// Entries per second over the last batch.
    pub last_rate: f64,
}

/// `mirror.status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MirrorStatus {
    /// `[pg] enabled` in the config.
    pub configured: bool,
    pub running: bool,
    pub state: MirrorState,
    /// Seconds since the oldest unsent change.
    pub lag_s: Option<i64>,
    pub live: Live,
    /// Accounts left out with `mirror = false` (anon accounts are always left out and not listed).
    pub excluded: Vec<AccountId>,
    /// Row counts in Postgres, or why they could not be read.
    pub pg: Option<lumbridge_mail_pg::Counts>,
    pub pg_error: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BackfillParams {
    pub account: Option<AccountId>,
    /// Also try entries again that failed too often on their own.
    pub retry_stuck: bool,
}

pub(crate) const METHOD_STATUS: &str = "mirror.status";
pub(crate) const METHOD_BACKFILL: &str = "mirror.backfill";

fn expand_home(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_owned(),
    }
}

impl MirrorHandle {
    fn stop(&self) {
        if let Some(r) = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            r.task.abort();
        }
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .connected = false;
    }

    fn is_running(&self) -> bool {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|r| !r.task.is_finished())
    }
}

/// Makes the mirror match the config: off (the outbox stops recording and is emptied), or on (the
/// first time, or after being off, everything is queued once; then the task runs). Called at start
/// and on every config reload. Never fails the reload: a mirror problem is logged and shown in status.
pub(crate) async fn configure(shared: &Arc<Shared>) {
    if let Err(e) = configure_inner(shared).await {
        eprintln!("lumbridge-maild: Postgres mirror: {e}");
        let msg = e.to_string();
        let _ = shared.db(move |s| s.mirror_error(&msg)).await;
    }
}

async fn configure_inner(shared: &Arc<Shared>) -> Result<()> {
    let cfg = shared
        .config
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let pg: PgSection = cfg.pg.clone();
    if !pg.enabled {
        shared.mirror.stop();
        if shared.db(|s| s.set_mirror_enabled(false)).await? {
            eprintln!("lumbridge-maild: Postgres mirror off (changes are no longer recorded)");
        }
        return Ok(());
    }
    let Some(dsn_file) = pg.dsn_file.as_deref().map(expand_home) else {
        shared.mirror.stop();
        return Err(Error::Invalid(
            "[pg] enabled = true needs dsn_file = \"<a 0600 file with the mail_writer DSN>\""
                .into(),
        ));
    };
    let settings = Settings {
        dsn_file,
        ca_file: pg.ca_file.as_deref().map(expand_home),
        batch: pg.batch.clamp(1, 5000),
        poll_ms: pg.poll_ms.clamp(50, 60_000),
        max_backoff_s: pg.max_backoff_s.max(1),
        excluded: cfg
            .accounts
            .iter()
            .filter(|a| !a.mirror)
            .map(|a| a.account_id())
            .collect(),
    };
    if !shared.db(|s| s.set_mirror_enabled(true)).await? {
        let n = shared.db(|s| s.mirror_backfill(None)).await?;
        eprintln!(
            "lumbridge-maild: Postgres mirror on; {n} change(s) queued to send what the store holds"
        );
    }
    {
        let running = shared
            .mirror
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(r) = running.as_ref()
            && r.settings == settings
            && !r.task.is_finished()
        {
            shared.mirror.wake.notify_one();
            return Ok(());
        }
    }
    shared.mirror.stop();
    let task = tokio::spawn(run(shared.clone(), settings.clone()));
    *shared
        .mirror
        .running
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(Running { settings, task });
    Ok(())
}

/// The mirror's own store connection (a second SQLite connection on the same file).
type MStore = Arc<Mutex<Store>>;

async fn mdb<T: Send + 'static>(
    store: &MStore,
    f: impl FnOnce(&mut Store) -> Result<T> + Send + 'static,
) -> Result<T> {
    let store = store.clone();
    tokio::task::spawn_blocking(move || {
        let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut s)
    })
    .await
    .map_err(|e| Error::Other(format!("mirror store task failed: {e}")))?
}

/// Waits `d`, or less if woken or shutting down (false = shutting down).
async fn pause(shared: &Shared, wake: &Notify, d: Duration) -> bool {
    let mut down = shared.shutdown.subscribe();
    if *down.borrow() {
        return false;
    }
    tokio::select! {
        _ = tokio::time::sleep(d) => true,
        _ = wake.notified() => true,
        _ = down.changed() => false,
    }
}

async fn run(shared: Arc<Shared>, settings: Settings) {
    let path = shared.paths.database();
    let store: MStore = match tokio::task::spawn_blocking(move || Store::open(path)).await {
        Ok(Ok(s)) => Arc::new(Mutex::new(s)),
        Ok(Err(e)) => {
            eprintln!("lumbridge-maild: Postgres mirror: cannot open the store: {e}");
            return;
        }
        Err(e) => {
            eprintln!("lumbridge-maild: Postgres mirror: {e}");
            return;
        }
    };
    let wake = shared.mirror.wake.clone();
    let live = shared.mirror.live.clone();
    let opts = ConnectOptions {
        ca_file: settings.ca_file.clone().map(Into::into),
        application_name: Some("lumbridge-maild mirror".into()),
    };
    let max_backoff = Duration::from_secs(settings.max_backoff_s);
    let mut backoff = Duration::from_secs(1);
    let mut writer: Option<Writer> = None;
    let mut in_pg: HashSet<String> = HashSet::new();
    let mut logged_down = false;
    loop {
        if shared.is_shutting_down() {
            return;
        }
        if writer.as_ref().is_none_or(Writer::is_closed) {
            live.lock().unwrap_or_else(|e| e.into_inner()).connected = false;
            let connected = async {
                let dsn =
                    lumbridge_mail_pg::read_dsn_file(std::path::Path::new(&settings.dsn_file))?;
                let mut w = Writer::connect(&dsn, &opts).await?;
                let pg = reconcile(&store, &mut w, &settings.excluded).await?;
                Ok::<_, Error>((w, pg))
            }
            .await;
            match connected {
                Ok((w, pg)) => {
                    if logged_down {
                        eprintln!("lumbridge-maild: Postgres mirror connected again");
                    }
                    writer = Some(w);
                    in_pg = pg;
                    backoff = Duration::from_secs(1);
                    logged_down = false;
                    live.lock().unwrap_or_else(|e| e.into_inner()).connected = true;
                }
                Err(e) => {
                    if !logged_down {
                        eprintln!(
                            "lumbridge-maild: Postgres mirror: {e} (retrying; mail works as usual)"
                        );
                        logged_down = true;
                    }
                    let msg = e.to_string();
                    let _ = mdb(&store, move |s| s.mirror_error(&msg)).await;
                    if !pause(&shared, &Notify::new(), backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(max_backoff);
                    continue;
                }
            }
        }
        let Some(w) = writer.as_mut() else { continue };
        let batch = settings.batch;
        let items = match mdb(&store, move |s| s.mirror_take(batch)).await {
            Ok(i) => i,
            Err(e) => {
                eprintln!("lumbridge-maild: Postgres mirror: {e}");
                if !pause(&shared, &wake, Duration::from_secs(5)).await {
                    return;
                }
                continue;
            }
        };
        if items.is_empty() {
            if !pause(&shared, &wake, Duration::from_millis(settings.poll_ms)).await {
                return;
            }
            continue;
        }
        let started = Instant::now();
        match send(&store, w, &mut in_pg, &settings.excluded, items).await {
            Ok(n) => {
                let ms = started.elapsed().as_millis() as u64;
                let mut l = live.lock().unwrap_or_else(|e| e.into_inner());
                l.last_batch_items = n;
                l.last_batch_ms = ms;
                l.last_batch_at = Some(now());
                l.last_rate = if ms == 0 {
                    n as f64 * 1000.0
                } else {
                    n as f64 * 1000.0 / ms as f64
                };
            }
            Err(e) => {
                // Postgres went away mid-batch: nothing was marked sent; reconnect and resend.
                let msg = e.to_string();
                let _ = mdb(&store, move |s| s.mirror_error(&msg)).await;
                if !logged_down {
                    eprintln!("lumbridge-maild: Postgres mirror: {e} (retrying)");
                    logged_down = true;
                }
                writer = None;
                if !pause(&shared, &Notify::new(), backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}

/// Which store accounts may be mirrored: not anon, not `mirror = false`. With their kept flag.
fn allowed_accounts(
    s: &Store,
    excluded: &BTreeSet<AccountId>,
) -> Result<HashMap<AccountId, (Account, bool)>> {
    let mut out = HashMap::new();
    for a in s.accounts()? {
        if a.persona == Persona::Anon || excluded.contains(&a.id) {
            continue;
        }
        let kept = s.account_kept(&a.id)?;
        out.insert(a.id.clone(), (a, kept));
    }
    Ok(out)
}

/// At every (re)connect: accounts in Postgres that may no longer be there are deleted (with their
/// mail); allowed accounts missing from Postgres are queued in full. Returns the account ids now there.
async fn reconcile(
    store: &MStore,
    w: &mut Writer,
    excluded: &BTreeSet<AccountId>,
) -> Result<HashSet<String>> {
    let mut in_pg = w.account_ids().await?;
    let ex = excluded.clone();
    let allowed = mdb(store, move |s| allowed_accounts(s, &ex)).await?;
    let gone: Vec<String> = in_pg
        .iter()
        .filter(|id| !allowed.contains_key(&AccountId::new(id.as_str())))
        .cloned()
        .collect();
    if !gone.is_empty() {
        let changes: Vec<Change> = gone
            .iter()
            .map(|id| Change::DeleteAccount { id: id.clone() })
            .collect();
        w.apply(&changes).await?;
        for id in &gone {
            in_pg.remove(id);
        }
        eprintln!(
            "lumbridge-maild: Postgres mirror: removed {} account(s) that may not be mirrored",
            gone.len()
        );
    }
    let missing: Vec<AccountId> = allowed
        .keys()
        .filter(|id| !in_pg.contains(id.as_str()))
        .cloned()
        .collect();
    if !missing.is_empty() {
        mdb(store, move |s| {
            for id in &missing {
                s.mirror_backfill(Some(id))?;
            }
            Ok(())
        })
        .await?;
    }
    Ok(in_pg)
}

/// What one outbox entry turns into.
#[derive(Default)]
struct Plan {
    changes: Vec<Change>,
    /// After it is sent: queue this account in full (it was not in Postgres before).
    backfill: Option<AccountId>,
    /// After it is sent: queue these messages again (their thread turned out sensitive).
    again: Vec<(AccountId, MessageId)>,
}

/// Sends a batch: one transaction; if Postgres refuses it, entry by entry so one bad row does not
/// hold up the rest. Returns how many entries were done.
async fn send(
    store: &MStore,
    w: &mut Writer,
    in_pg: &mut HashSet<String>,
    excluded: &BTreeSet<AccountId>,
    items: Vec<OutboxItem>,
) -> Result<usize> {
    let ex = excluded.clone();
    let pg_now = in_pg.clone();
    let (items, plans) = mdb(store, move |s| {
        let allowed = allowed_accounts(s, &ex)?;
        let plans = items
            .iter()
            .map(|i| plan(s, &allowed, &pg_now, i))
            .collect::<Result<Vec<_>>>()?;
        Ok((items, plans))
    })
    .await?;
    let all: Vec<Change> = plans
        .iter()
        .flat_map(|p| p.changes.iter().cloned())
        .collect();
    let n = items.len();
    match w.apply(&all).await {
        Ok(()) => {
            note_accounts(in_pg, &all);
            finish(store, items, plans).await?;
        }
        Err(e) if lumbridge_mail_pg::is_refusal(&e) => {
            for (item, p) in items.into_iter().zip(plans) {
                match w.apply(&p.changes).await {
                    Ok(()) => {
                        note_accounts(in_pg, &p.changes);
                        finish(store, vec![item], vec![p]).await?;
                    }
                    Err(e) if lumbridge_mail_pg::is_refusal(&e) => {
                        eprintln!(
                            "lumbridge-maild: Postgres mirror: {} {}/{} refused: {e}",
                            item.kind.as_str(),
                            item.account,
                            item.item
                        );
                        let msg = e.to_string();
                        mdb(store, move |s| s.mirror_failed(&item, &msg)).await?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        Err(e) => return Err(e),
    }
    Ok(n)
}

fn note_accounts(in_pg: &mut HashSet<String>, changes: &[Change]) {
    for c in changes {
        match c {
            Change::UpsertAccount(a) => {
                in_pg.insert(a.id.clone());
            }
            Change::DeleteAccount { id } => {
                in_pg.remove(id);
            }
            _ => {}
        }
    }
}

async fn finish(store: &MStore, items: Vec<OutboxItem>, plans: Vec<Plan>) -> Result<()> {
    mdb(store, move |s| {
        s.mirror_done(&items)?;
        for p in plans {
            if let Some(a) = p.backfill {
                s.mirror_backfill(Some(&a))?;
            }
            for (a, m) in p.again {
                s.mirror_enqueue(OutboxKind::Message, &a, m.as_str())?;
            }
        }
        Ok(())
    })
    .await
}

/// Is this message sensitive, and if so its kind and one line. Sender, subject, snippet and (when
/// cached) body text, the same rules and the same line agents see (`crate::redact`); a thread
/// triaged `sensitive` makes every message in it sensitive.
fn judge(
    m: &MessageMeta,
    body: Option<&Body>,
    triage: Option<&Triage>,
) -> Option<(String, String)> {
    let from = m.from.first();
    let name = from.and_then(|a| a.name.as_deref());
    let email = from.map(|a| a.email.as_str()).unwrap_or("");
    let mut kind = sensitive_sender_kind(name, email, &m.subject, &m.snippet);
    let mut line = crate::redact::sensitive_line_for_meta(m);
    if kind.is_none()
        && let Some(b) = body
    {
        let text = b.text.clone().unwrap_or_else(|| {
            lumbridge_mail_render::plain_text(
                b,
                &lumbridge_mail_render::TextOptions {
                    max_chars: 4000,
                    ..Default::default()
                },
            )
        });
        kind = sensitive_sender_kind(name, email, &m.subject, &text);
        line = crate::redact::sensitive_line_with_body(m, b);
    }
    if let (Some(k), Some(l)) = (kind, line) {
        return Some((k.to_owned(), l));
    }
    triage
        .filter(|t| t.category == Category::Sensitive)
        .map(|t| ("Sensitive".to_owned(), t.summary.clone()))
}

fn plan(
    s: &Store,
    allowed: &HashMap<AccountId, (Account, bool)>,
    in_pg: &HashSet<String>,
    item: &OutboxItem,
) -> Result<Plan> {
    let mut p = Plan::default();
    let acct = &item.account;
    let Some((account, kept)) = allowed.get(acct) else {
        // Anon, `mirror = false`, or gone from the store: never sent; deleted if it is in Postgres.
        if item.kind == OutboxKind::Account && in_pg.contains(acct.as_str()) {
            p.changes.push(Change::DeleteAccount { id: acct.0.clone() });
        }
        return Ok(p);
    };
    let Some(row) = AccountRow::from_account(account, *kept) else {
        return Ok(p);
    };
    if item.kind == OutboxKind::Account || !in_pg.contains(acct.as_str()) {
        p.changes.push(Change::UpsertAccount(row));
        if !in_pg.contains(acct.as_str()) && item.kind == OutboxKind::Account {
            p.backfill = Some(acct.clone());
        }
    }
    match item.kind {
        OutboxKind::Account => {}
        OutboxKind::Mailbox => match s
            .mailboxes(Some(acct))?
            .into_iter()
            .find(|b| b.id.as_str() == item.item)
        {
            Some(b) => p
                .changes
                .push(Change::UpsertMailbox(MailboxRow::from_mailbox(&b))),
            None => p.changes.push(Change::DeleteMailbox {
                account: acct.0.clone(),
                id: item.item.clone(),
            }),
        },
        OutboxKind::Message | OutboxKind::Body => {
            let id = MessageId::new(item.item.as_str());
            match s.message(acct, &id)? {
                None => p.changes.push(Change::DeleteMessage {
                    account: acct.0.clone(),
                    id: item.item.clone(),
                }),
                Some(m) => {
                    let body = s.body(acct, &id)?;
                    let triage = s.triage(acct, &m.thread)?;
                    let change = match judge(&m, body.as_ref(), triage.as_ref()) {
                        Some((kind, line)) => Change::message(
                            MessageRow::sensitive(&m, &kind, &line),
                            BodyChange::Drop,
                        ),
                        None => {
                            let b = match (&body, item.kind) {
                                (Some(b), OutboxKind::Body) => BodyChange::Put(Box::new(
                                    BodyRow::from_body(acct.as_str(), id.as_str(), b)
                                        // schema.org data for the parsing worker (crate::pgwork).
                                        .with_structured(
                                            lumbridge_mail_index::structured::from_body(b),
                                        ),
                                )),
                                _ => BodyChange::Keep,
                            };
                            Change::message(MessageRow::from_meta(&m), b)
                        }
                    };
                    p.changes.push(change);
                }
            }
        }
        OutboxKind::Triage => {
            let thread = ThreadId::new(item.item.as_str());
            match s.triage(acct, &thread)? {
                None => p.changes.push(Change::DeleteTriage {
                    account: acct.0.clone(),
                    thread_id: item.item.clone(),
                }),
                Some(t) => {
                    let msgs = s.thread(acct, &thread)?;
                    let mut line = None;
                    for m in &msgs {
                        let body = s.body(acct, &m.id)?;
                        if let Some((_, l)) = judge(m, body.as_ref(), Some(&t)) {
                            line.get_or_insert(l);
                        }
                    }
                    if t.category == Category::Sensitive {
                        // Triage says sensitive where the rules on each message did not: send them again.
                        p.again = msgs.iter().map(|m| (acct.clone(), m.id.clone())).collect();
                    }
                    p.changes.push(Change::UpsertTriage(TriageRow::from_triage(
                        acct.as_str(),
                        &thread,
                        &t,
                        line.as_deref(),
                    )));
                }
            }
        }
    }
    Ok(p)
}

/// `mirror.status`.
pub(crate) async fn status(shared: &Arc<Shared>) -> Result<MirrorStatus> {
    let cfg = shared
        .config
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let state = shared.db(|s| s.mirror_state()).await?;
    let mut st = MirrorStatus {
        configured: cfg.pg.enabled,
        running: shared.mirror.is_running(),
        lag_s: state.oldest_at.map(|t| (now() - t).max(0)),
        state,
        live: shared
            .mirror
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        excluded: cfg
            .accounts
            .iter()
            .filter(|a| !a.mirror)
            .map(|a| a.account_id())
            .collect(),
        pg: None,
        pg_error: None,
    };
    if cfg.pg.enabled
        && let Some(f) = cfg.pg.dsn_file.as_deref().map(expand_home)
    {
        let opts = ConnectOptions {
            ca_file: cfg.pg.ca_file.as_deref().map(expand_home).map(Into::into),
            application_name: Some("lumbridge-maild status".into()),
        };
        let counts = tokio::time::timeout(Duration::from_secs(8), async {
            let dsn = lumbridge_mail_pg::read_dsn_file(std::path::Path::new(&f))?;
            let c = lumbridge_mail_pg::connect::connect(&dsn, &opts).await?;
            lumbridge_mail_pg::counts(&c).await
        })
        .await;
        match counts {
            Ok(Ok(c)) => st.pg = Some(c),
            Ok(Err(e)) => st.pg_error = Some(e.to_string()),
            Err(_) => st.pg_error = Some("timed out reading counts".into()),
        }
    }
    Ok(st)
}

/// `mirror.backfill`: queue what the store holds (one account or all) and wake the mirror.
pub(crate) async fn backfill(shared: &Arc<Shared>, p: BackfillParams) -> Result<Value> {
    if let Some(a) = &p.account {
        let a = a.clone();
        if shared.db(move |s| Ok(s.account(&a)?.is_none())).await? {
            return Err(Error::NotFound(format!(
                "account {}",
                p.account.as_ref().map(|a| a.as_str()).unwrap_or_default()
            )));
        }
    }
    let retry = p.retry_stuck;
    let account = p.account.clone();
    let (queued, retried) = shared
        .db(move |s| {
            let retried = if retry { s.mirror_retry_stuck()? } else { 0 };
            Ok((s.mirror_backfill(account.as_ref())?, retried))
        })
        .await?;
    shared.mirror.wake.notify_one();
    Ok(json!({ "queued": queued, "retried": retried }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(from: (&str, &str), subject: &str, snippet: &str) -> MessageMeta {
        MessageMeta {
            id: MessageId::new("m"),
            account: AccountId::new("a"),
            thread: ThreadId::new("t"),
            mailboxes: vec![],
            from: vec![Address::new(Some(from.0), from.1)],
            to: vec![],
            cc: vec![],
            bcc: vec![],
            subject: subject.into(),
            snippet: snippet.into(),
            date: 0,
            flags: Flags::default(),
            has_attachments: false,
            size: 0,
            rfc_message_id: None,
            in_reply_to: None,
            references: vec![],
            partial: false,
        }
    }

    #[test]
    fn sensitive_mail_is_judged_like_the_agent_view() {
        let (k, l) = judge(
            &meta(
                ("Acme", "no-reply@acme.example"),
                "Your Acme verification code",
                "482913",
            ),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            (k.as_str(), l.as_str()),
            ("A security code", "A security code from Acme")
        );
        // Only the body gives it away.
        let m = meta(("Acme", "hello@acme.example"), "Hello", "see inside");
        assert!(judge(&m, None, None).is_none());
        let body = Body {
            text: Some("Your code is 482913".into()),
            ..Body::default()
        };
        assert_eq!(judge(&m, Some(&body), None).unwrap().0, "A security code");
        // A bank by its domain.
        assert_eq!(
            judge(
                &meta(("Chase", "alerts@chase.com"), "Hello", ""),
                None,
                None
            )
            .unwrap()
            .0,
            "Bank or money mail"
        );
        // Triage said sensitive.
        let t = Triage {
            category: Category::Sensitive,
            priority: 0,
            summary: "A password or sign-in link from Acme".into(),
            needs_reply: false,
            due: None,
            by: "rules".into(),
            at: 0,
            suspicious: false,
        };
        assert_eq!(
            judge(&m, None, Some(&t)).unwrap().1,
            "A password or sign-in link from Acme"
        );
        assert!(
            judge(
                &meta(("Ada", "ada@example.com"), "Lunch?", "noon"),
                None,
                None
            )
            .is_none()
        );
    }
}
