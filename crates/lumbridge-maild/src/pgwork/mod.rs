//! The workers that make the Postgres mirror searchable by meaning and parsed into facts
//! (`[pg.embed]`, `[pg.parse]`, both off by default), and the read side the daemon serves
//! (`search.semantic`, `pg.parsed`, `pg.work.status`, `pg.requeue`).
//!
//! They drain `mail.jobs`, which the mirror's triggers fill whenever a body arrives or changes, on
//! their own Postgres connection as `mail_writer` (the DSN of `[pg] dsn_file`). A separate worker
//! role would not separate anything: the daemon holds both. Each worker is one task; jobs are
//! claimed in batches with `FOR UPDATE SKIP LOCKED` and a lease, so a second daemon (or a restart)
//! never doubles work.
//!
//! The privacy rules, checked for every job before anything is read or sent (and tested in
//! `tests/pg_work.rs`):
//! - sensitive mail is never embedded or parsed (it has no body in Postgres; the worker refuses
//!   it too, and the schema refuses its rows);
//! - accounts with AI `off` are neither embedded nor parsed (not even by rules); anon accounts are
//!   not in Postgres, and are refused if one ever were;
//! - `local_only` accounts only go to endpoints configured `place = "local"`
//!   (`safety::ai_allowed`); rules run locally and need no endpoint;
//! - mail that talks to an assistant is not given to a model (rules only), and model answers are
//!   validated before anything is stored (`lumbridge_mail_index::fact`);
//! - logs carry ids and counts, never mail content; job errors carry status codes only.

pub(crate) mod cli;
mod embed;
mod parse;
pub mod search;

use crate::engine::{Shared, now};
use lumbridge_mail_core::safety::{ModelPlace, ai_allowed};
use lumbridge_mail_core::*;
use lumbridge_mail_index::EmbedEndpoint;
use lumbridge_mail_pg::ConnectOptions;
use lumbridge_mail_pg::work::{JobKind, ParsedQuery, ParsedRow, QueueState, Requeue, Work};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub(crate) const METHOD_STATUS: &str = "pg.work.status";
pub(crate) const METHOD_REQUEUE: &str = "pg.requeue";
pub(crate) const METHOD_PARSED: &str = "pg.parsed";
pub(crate) const METHOD_SEMANTIC: &str = "search.semantic";

/// `[pg.embed]`: the embedding worker.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct EmbedSection {
    pub enabled: bool,
    /// The endpoint: `base_url`, `model`, `place`, `headers`, `api_key_env`, `timeout_s`,
    /// `input_type`, `query_prefix`, `passage_prefix`, `dimensions`. Also used to embed search
    /// queries (`lbmail search --semantic`), whether or not the worker is enabled.
    #[serde(flatten)]
    pub endpoint: EmbedEndpoint,
    /// Jobs claimed per round.
    pub batch_jobs: u32,
    /// Texts per request to the endpoint (a message's chunks are never split across requests).
    pub batch_inputs: u32,
    /// Requests per minute at most, spread evenly (0 = no limit).
    pub max_per_minute: u32,
    /// Requests in flight at once. 1: the embedding server is a shared GPU box.
    pub concurrency: u32,
    /// Characters per chunk (head included), overlap between chunks, chunks per message.
    pub chunk_chars: u32,
    pub chunk_overlap: u32,
    pub max_chunks: u32,
    #[serde(flatten)]
    pub retry: RetrySettings,
}

impl Default for EmbedSection {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: EmbedEndpoint::default(),
            batch_jobs: 32,
            batch_inputs: 16,
            max_per_minute: 120,
            concurrency: 1,
            chunk_chars: 3000,
            chunk_overlap: 300,
            max_chunks: 4,
            retry: RetrySettings::default(),
        }
    }
}

/// `[pg.parse]`: the parsing worker.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ParseSection {
    pub enabled: bool,
    /// Use the `[ai]` chat endpoint for the kinds the rules cannot read (false: rules only).
    pub model: bool,
    /// Jobs claimed per round.
    pub batch_jobs: u32,
    /// Model calls per minute at most, spread evenly (0 = no limit), and in flight at once.
    pub max_per_minute: u32,
    pub concurrency: u32,
    /// Characters of body a model sees.
    pub max_chars: u32,
    /// Model answers rated below this (percent) are not stored.
    pub min_confidence_pct: u8,
    /// A message whose thread is not triaged yet waits this long for triage (which picks the
    /// extractor) before it is parsed without it.
    pub triage_wait_s: u64,
    #[serde(flatten)]
    pub retry: RetrySettings,
}

impl Default for ParseSection {
    fn default() -> Self {
        Self {
            enabled: false,
            model: true,
            batch_jobs: 32,
            max_per_minute: 30,
            concurrency: 1,
            max_chars: 6000,
            min_confidence_pct: 50,
            triage_wait_s: 600,
            retry: RetrySettings::default(),
        }
    }
}

/// How failed jobs are retried, and how the queue is polled.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RetrySettings {
    /// A job that failed this many times is parked (`lbmail pg requeue --parked` tries again).
    pub max_attempts: u32,
    /// First retry after this long, doubling up to `max_retry_s`.
    pub retry_s: u64,
    pub max_retry_s: u64,
    /// A claimed job comes back after this long if its worker went away.
    pub lease_s: u64,
    /// How often an idle worker looks for jobs.
    pub poll_ms: u64,
}

impl Default for RetrySettings {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            retry_s: 60,
            max_retry_s: 3600,
            lease_s: 600,
            poll_ms: 2000,
        }
    }
}

impl RetrySettings {
    fn backoff(&self, attempts: i32) -> Duration {
        let n = attempts.clamp(1, 20) as u32 - 1;
        let s = self.retry_s.max(1).saturating_mul(1u64 << n.min(16));
        Duration::from_secs(s.min(self.max_retry_s.max(1)))
    }
}

/// What one worker is doing, for `lbmail pg status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerLive {
    pub enabled: bool,
    pub running: bool,
    pub connected: bool,
    /// The endpoint label (`model @ base_url`), never a key.
    pub endpoint: Option<String>,
    /// Why it is not working, if it is not (no mail content).
    pub last_error: Option<String>,
    pub last_error_at: Option<i64>,
    pub last_batch_at: Option<i64>,
    pub last_batch_jobs: usize,
    pub last_batch_calls: usize,
    pub last_batch_ms: u64,
    /// Since the daemon started.
    pub jobs: u64,
    pub calls: u64,
    pub items: u64,
    /// Model answers that failed validation (parse).
    pub rejected: u64,
}

/// `pg.work.status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkStatus {
    pub embed: WorkerLive,
    pub parse: WorkerLive,
    /// The embedding model searches and the worker use.
    pub embed_model: Option<String>,
    pub queues: Option<Vec<QueueState>>,
    pub queues_error: Option<String>,
}

struct Running<S> {
    settings: S,
    task: JoinHandle<()>,
}

/// The running workers, kept in `Shared`.
#[derive(Default)]
pub(crate) struct Handle {
    embed: Mutex<Option<Running<embed::Settings>>>,
    parse: Mutex<Option<Running<parse::Settings>>>,
    embed_live: Arc<Mutex<WorkerLive>>,
    parse_live: Arc<Mutex<WorkerLive>>,
    wake: Arc<Notify>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn stop<S>(slot: &Mutex<Option<Running<S>>>, live: &Mutex<WorkerLive>) {
    if let Some(r) = lock(slot).take() {
        r.task.abort();
    }
    let mut l = lock(live);
    l.running = false;
    l.connected = false;
}

fn expand_home(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_owned(),
    }
}

/// Where the workers connect: the mirror's DSN file and CA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Db {
    pub dsn_file: String,
    pub ca_file: Option<String>,
}

impl Db {
    fn from_config(cfg: &crate::config::Config) -> Option<Db> {
        cfg.pg.dsn_file.as_deref().map(|f| Db {
            dsn_file: expand_home(f),
            ca_file: cfg.pg.ca_file.as_deref().map(expand_home),
        })
    }

    pub(crate) async fn connect(
        &self,
        app: &str,
    ) -> Result<lumbridge_mail_pg::tokio_postgres::Client> {
        let dsn = lumbridge_mail_pg::read_dsn_file(std::path::Path::new(&self.dsn_file))?;
        let opts = ConnectOptions {
            ca_file: self.ca_file.clone().map(Into::into),
            application_name: Some(app.into()),
        };
        lumbridge_mail_pg::connect::connect(&dsn, &opts).await
    }

    async fn work(&self, app: &str) -> Result<Work> {
        Work::from_client(self.connect(app).await?).await
    }
}

/// Makes the workers match the config (start, restart on a change, stop). Called after the mirror is
/// configured, at start and on every reload. Never fails the reload.
pub(crate) async fn configure(shared: &Arc<Shared>) {
    let cfg = shared
        .config
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let h = &shared.pgwork;
    let db = Db::from_config(&cfg).filter(|_| cfg.pg.enabled);
    {
        let mut l = lock(&h.embed_live);
        l.enabled = cfg.pg.enabled && cfg.pg.embed.enabled;
        l.endpoint = cfg
            .pg
            .embed
            .endpoint
            .is_configured()
            .then(|| endpoint_label(&cfg.pg.embed.endpoint));
    }
    {
        let mut l = lock(&h.parse_live);
        l.enabled = cfg.pg.enabled && cfg.pg.parse.enabled;
        l.endpoint = (cfg.pg.parse.model && cfg.ai.enabled).then(|| cfg.ai.endpoint.label.clone());
    }
    match (&db, cfg.pg.embed.enabled) {
        (Some(db), true) => {
            let s = embed::Settings {
                db: db.clone(),
                section: cfg.pg.embed.clone(),
            };
            restart(&h.embed, &h.embed_live, s, |s| {
                tokio::spawn(embed::run(shared.clone(), s))
            });
        }
        _ => stop(&h.embed, &h.embed_live),
    }
    match (&db, cfg.pg.parse.enabled) {
        (Some(db), true) => {
            let s = parse::Settings {
                db: db.clone(),
                section: cfg.pg.parse.clone(),
                ai_enabled: cfg.ai.enabled,
                ai_endpoint: format!("{:?}", cfg.ai.endpoint),
            };
            restart(&h.parse, &h.parse_live, s, |s| {
                tokio::spawn(parse::run(shared.clone(), s))
            });
        }
        _ => stop(&h.parse, &h.parse_live),
    }
    h.wake.notify_waiters();
}

fn restart<S: Clone + PartialEq>(
    slot: &Mutex<Option<Running<S>>>,
    live: &Mutex<WorkerLive>,
    settings: S,
    spawn: impl FnOnce(S) -> JoinHandle<()>,
) {
    let mut g = lock(slot);
    if let Some(r) = g.as_ref()
        && r.settings == settings
        && !r.task.is_finished()
    {
        return;
    }
    if let Some(r) = g.take() {
        r.task.abort();
    }
    lock(live).running = true;
    *g = Some(Running {
        task: spawn(settings.clone()),
        settings,
    });
}

pub(crate) fn endpoint_label(e: &EmbedEndpoint) -> String {
    format!("{} @ {}", e.model, e.base_url)
}

/// Waits `d`, or less if woken or shutting down (false = shutting down).
async fn pause(shared: &Shared, d: Duration) -> bool {
    let mut down = shared.shutdown.subscribe();
    if *down.borrow() {
        return false;
    }
    tokio::select! {
        _ = tokio::time::sleep(d) => true,
        _ = shared.pgwork.wake.notified() => true,
        _ = down.changed() => false,
    }
}

/// Why a job may not go further, if it may not. `place` is the endpoint the content would go to;
/// None for local code (the rules).
pub(crate) fn refusal(
    account: Option<&Account>,
    pg_persona: &str,
    pg_ai_policy: &str,
    sensitive: Option<&str>,
    place: Option<ModelPlace>,
) -> Option<&'static str> {
    if sensitive.is_some() {
        return Some("sensitive mail");
    }
    let Some(a) = account else {
        return Some("the account is not in the daemon");
    };
    if a.persona == Persona::Anon || pg_persona == "anon" {
        return Some("anon account");
    }
    if a.ai == AiPolicy::Off || pg_ai_policy == "off" {
        return Some("the account's AI policy is off");
    }
    if let Some(place) = place {
        if !ai_allowed(a, place) {
            return Some("the account is local-only and the endpoint is hosted");
        }
        if place == ModelPlace::Hosted && pg_ai_policy == "local_only" {
            return Some("the account is local-only and the endpoint is hosted");
        }
    }
    None
}

/// The store's account rows, read once per batch.
async fn accounts_of(
    shared: &Arc<Shared>,
    ids: Vec<String>,
) -> Result<std::collections::HashMap<String, Option<Account>>> {
    shared
        .db(move |s| {
            let mut out = std::collections::HashMap::new();
            for id in ids {
                let a = s.account(&AccountId::new(id.as_str()))?;
                out.insert(id, a);
            }
            Ok(out)
        })
        .await
}

fn note_error(live: &Mutex<WorkerLive>, e: &str) {
    let mut l = lock(live);
    l.last_error = Some(e.chars().take(300).collect());
    l.last_error_at = Some(now());
}

/// `pg.work.status`.
pub(crate) async fn status(shared: &Arc<Shared>) -> Result<WorkStatus> {
    let cfg = shared
        .config
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let h = &shared.pgwork;
    let mut st = WorkStatus {
        embed: lock(&h.embed_live).clone(),
        parse: lock(&h.parse_live).clone(),
        embed_model: cfg
            .pg
            .embed
            .endpoint
            .is_configured()
            .then(|| cfg.pg.embed.endpoint.model.clone()),
        queues: None,
        queues_error: None,
    };
    st.embed.running = lock(&h.embed)
        .as_ref()
        .is_some_and(|r| !r.task.is_finished());
    st.parse.running = lock(&h.parse)
        .as_ref()
        .is_some_and(|r| !r.task.is_finished());
    if let Some(db) = Db::from_config(&cfg).filter(|_| cfg.pg.enabled) {
        let q = tokio::time::timeout(Duration::from_secs(8), async {
            let c = db.connect("lumbridge-maild status").await?;
            if lumbridge_mail_pg::migrate::current_version(&c)
                .await?
                .unwrap_or(0)
                < 2
            {
                return Err(Error::Refused(
                    "the mail schema is older than version 2: run `lbmail pg migrate`".into(),
                ));
            }
            lumbridge_mail_pg::work::queue_state(&c).await
        })
        .await;
        match q {
            Ok(Ok(q)) => st.queues = Some(q),
            Ok(Err(e)) => st.queues_error = Some(e.to_string()),
            Err(_) => st.queues_error = Some("timed out reading the queues".into()),
        }
    }
    Ok(st)
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RequeueParams {
    pub kind: JobKind,
    #[serde(default)]
    pub account: Option<AccountId>,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub parked_only: bool,
}

/// `pg.requeue`: queue embedding (what lacks the current model's embeddings, or everything with
/// `all`) or parsing (everything) again, or give parked jobs another chance.
pub(crate) async fn requeue(shared: &Arc<Shared>, p: RequeueParams) -> Result<Value> {
    let cfg = shared
        .config
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let db = Db::from_config(&cfg)
        .filter(|_| cfg.pg.enabled)
        .ok_or_else(|| {
            Error::Invalid("the Postgres mirror is off ([pg] enabled = false)".into())
        })?;
    let model = cfg
        .pg
        .embed
        .endpoint
        .is_configured()
        .then(|| cfg.pg.embed.endpoint.model.clone());
    if p.kind == JobKind::Embed && model.is_none() && !p.all && !p.parked_only {
        return Err(Error::Invalid(
            "[pg.embed] has no model: give --all, or configure base_url and model".into(),
        ));
    }
    let work = db.work("lumbridge-maild requeue").await?;
    let n = work
        .requeue(&Requeue {
            kind: p.kind,
            account: p.account.map(|a| a.0),
            model,
            all: p.all || p.kind == JobKind::Parse,
            parked_only: p.parked_only,
        })
        .await?;
    shared.pgwork.wake.notify_waiters();
    Ok(json!({ "queued": n }))
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ParsedParams {
    pub kind: Option<String>,
    pub since: Option<i64>,
    pub accounts: Option<Vec<AccountId>>,
    pub limit: Option<i64>,
}

/// `pg.parsed`: facts, newest mail first.
pub(crate) async fn parsed(shared: &Arc<Shared>, p: ParsedParams) -> Result<Vec<ParsedRow>> {
    if let Some(k) = &p.kind
        && lumbridge_mail_index::FactKind::parse(k).is_none()
    {
        return Err(Error::Invalid(format!(
            "kind {k:?}: one of {}",
            lumbridge_mail_index::FactKind::ALL
                .map(|k| k.as_str())
                .join(", ")
        )));
    }
    let cfg = shared
        .config
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let db = Db::from_config(&cfg)
        .filter(|_| cfg.pg.enabled)
        .ok_or_else(|| {
            Error::Invalid("the Postgres mirror is off ([pg] enabled = false)".into())
        })?;
    let c = tokio::time::timeout(Duration::from_secs(8), db.connect("lumbridge-maild parsed"))
        .await
        .map_err(|_| Error::Offline("postgres: timed out connecting".into()))??;
    lumbridge_mail_pg::work::parsed(
        &c,
        &ParsedQuery {
            kind: p.kind,
            since: p.since,
            accounts: p.accounts.map(|v| v.into_iter().map(|a| a.0).collect()),
            limit: p.limit.unwrap_or(100),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(persona: Persona, ai: AiPolicy) -> Account {
        Account {
            id: AccountId::new("a"),
            persona,
            label: "A".into(),
            email: "sam@example.com".into(),
            name: "Sam".into(),
            provider: ProviderKind::Demo,
            color: None,
            ai,
            readonly: false,
        }
    }

    #[test]
    fn the_privacy_rules_refuse_before_anything_is_sent() {
        let any = account(Persona::Personal, AiPolicy::Any);
        let local = account(Persona::Work, AiPolicy::LocalOnly);
        let off = account(Persona::Personal, AiPolicy::Off);
        let anon = account(Persona::Anon, AiPolicy::Any);
        let l = Some(ModelPlace::Local);
        let h = Some(ModelPlace::Hosted);
        assert_eq!(refusal(Some(&any), "personal", "any", None, h), None);
        assert_eq!(
            refusal(Some(&any), "personal", "any", Some("A security code"), l),
            Some("sensitive mail")
        );
        assert_eq!(
            refusal(Some(&off), "personal", "off", None, None),
            Some("the account's AI policy is off"),
            "not even rules"
        );
        assert_eq!(refusal(Some(&local), "work", "local_only", None, l), None);
        assert!(refusal(Some(&local), "work", "local_only", None, h).is_some());
        assert_eq!(
            refusal(Some(&local), "work", "local_only", None, None),
            None,
            "rules are local"
        );
        assert_eq!(
            refusal(Some(&anon), "anon", "any", None, l),
            Some("anon account")
        );
        assert_eq!(
            refusal(None, "personal", "any", None, l),
            Some("the account is not in the daemon")
        );
        // Postgres' row is a second opinion: a policy turned off there is honoured too.
        assert!(refusal(Some(&any), "personal", "off", None, l).is_some());
        assert!(refusal(Some(&any), "personal", "local_only", None, h).is_some());
    }

    #[test]
    fn the_workers_are_off_unless_configured_and_parse_from_toml() {
        let c = crate::config::Config::parse("").unwrap();
        assert!(!c.pg.embed.enabled && !c.pg.parse.enabled);
        assert!(!c.pg.embed.endpoint.is_configured());
        assert_eq!((c.pg.embed.concurrency, c.pg.parse.concurrency), (1, 1));
        let c = crate::config::Config::parse(
            "[pg]\nenabled = true\ndsn_file = \"~/pg.dsn\"\n\n[pg.embed]\nenabled = true\nbase_url = \"http://embed.example:8000/v1\"\nmodel = \"m\"\nplace = \"hosted\"\nheaders = [[\"x-no-trace\", \"1\"]]\ninput_type = true\nmax_per_minute = 10\nmax_attempts = 7\n\n[pg.parse]\nenabled = true\nmodel = false\ntriage_wait_s = 5\n",
        )
        .unwrap();
        let e = &c.pg.embed;
        assert!(e.enabled && e.endpoint.is_configured() && e.endpoint.input_type);
        assert_eq!(e.endpoint.place, ModelPlace::Hosted);
        assert_eq!(
            e.endpoint.headers,
            vec![("x-no-trace".to_string(), "1".to_string())]
        );
        assert_eq!(
            (e.max_per_minute, e.retry.max_attempts, e.batch_inputs),
            (10, 7, 16)
        );
        assert!(c.pg.parse.enabled && !c.pg.parse.model);
        assert_eq!(c.pg.parse.triage_wait_s, 5);
        assert!(crate::config::Config::parse("[pg.embed]\nplace = \"cloud\"\n").is_err());
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        let r = RetrySettings {
            retry_s: 60,
            max_retry_s: 3600,
            ..RetrySettings::default()
        };
        assert_eq!(r.backoff(1), Duration::from_secs(60));
        assert_eq!(r.backoff(2), Duration::from_secs(120));
        assert_eq!(r.backoff(4), Duration::from_secs(480));
        assert_eq!(r.backoff(30), Duration::from_secs(3600));
    }
}
