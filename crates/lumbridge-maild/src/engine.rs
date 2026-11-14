//! The engine: per account a sync loop and an action-queue runner, one background triage worker, and
//! the shared state the RPC methods use. The store is synchronous; every store call goes through
//! [`Shared::db`] (`spawn_blocking` on one writer behind a mutex).

use crate::config::{AccountConfig, Config, ProviderConfig};
use crate::demo::{DemoProvider, DemoSet};
use crate::secrets::{self, SecretSource};
use lumbridge_mail_ai::{MailAi, MailMessage, MailThread, Model, OpenAiModel};
use lumbridge_mail_core::rpc::{AccountStatus, Event, ThreadRef};
use lumbridge_mail_core::safety::{self, ModelPlace};
use lumbridge_mail_core::*;
use lumbridge_mail_store::{FailOutcome, RetryPolicy, Store};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, broadcast, watch};
use tokio::task::JoinHandle;

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// What the daemon knows about one account at run time.
pub(crate) struct AccountRt {
    pub account: Account,
    pub config: AccountConfig,
    pub provider: Arc<dyn Provider>,
    pub sync_now: Notify,
    pub queue_wake: Notify,
    pub stop: watch::Sender<bool>,
    pub status: Mutex<RtStatus>,
    /// Set by an `Auth` error: the sync loop waits for `sync.now` (the user acted) before trying again.
    pub auth_blocked: AtomicBool,
    pub tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Calls to the provider in flight that are not the body fill's, and when the last one ended
    /// (`AccountRt::foreground`): the fill waits for both.
    pub fg: std::sync::atomic::AtomicU32,
    pub fg_last: Mutex<Instant>,
    pub fill: crate::fill::FillRt,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RtStatus {
    pub online: bool,
    pub error: Option<Error>,
    /// A first (or reset) sync is still paging.
    pub syncing: bool,
}

impl AccountRt {
    pub fn readonly(&self) -> bool {
        self.config.readonly
    }
}

pub(crate) struct Shared {
    pub paths: lumbridge_mail_core::paths::Paths,
    pub store: Arc<Mutex<Store>>,
    pub config: RwLock<Config>,
    pub accounts: RwLock<BTreeMap<AccountId, Arc<AccountRt>>>,
    pub events: broadcast::Sender<Event>,
    pub ai: RwLock<Option<Arc<MailAi>>>,
    pub person_token: String,
    pub shutdown: watch::Sender<bool>,
    pub triage_wake: Notify,
    pub overrides: HashMap<AccountId, Arc<dyn Provider>>,
    pub model_override: Option<Arc<dyn Model>>,
    /// Threads the triage worker should not look at again before the given time (failures, future dates).
    pub triage_skip: Mutex<HashMap<(AccountId, ThreadId), Instant>>,
    pub version: String,
    /// Where `accounts.add` puts passwords.
    pub secrets: Arc<dyn secrets::SecretStore>,
    /// The gog process budget: one gate per ssh host (and one for local gog), shared by its accounts.
    pub gates: Arc<lumbridge_mail_gog::Gates>,
    /// Shared ssh connections, one per gog host (closed at shutdown).
    pub ssh: Mutex<HashMap<String, lumbridge_mail_gog::SshRunner>>,
    /// `daemon.pause`: no provider is called while true. Persisted as `paths.paused_file()`.
    pub paused: watch::Sender<bool>,
    /// `daemon.quit` was asked: the server shuts down.
    pub quit: Notify,
    /// Background triage body fetches, per account.
    pub body_buckets: Mutex<HashMap<AccountId, Bucket>>,
    /// Tests: gog accounts run through this runner (inside the budget) instead of a real gog.
    pub gog_runner: Option<Arc<dyn lumbridge_mail_gog::Runner>>,
    /// The Postgres mirror (`[pg]`), if configured.
    pub mirror: crate::mirror::MirrorHandle,
    /// The embedding and parsing workers (`[pg.embed]`, `[pg.parse]`).
    pub pgwork: crate::pgwork::Handle,
}

/// A small token bucket: `per_min` refill, up to `cap` saved up.
#[derive(Debug, Clone)]
pub(crate) struct Bucket {
    tokens: f64,
    at: Instant,
    per_min: u32,
}

impl Bucket {
    pub fn new(per_min: u32) -> Self {
        Self { tokens: Self::cap_of(per_min), at: Instant::now(), per_min }
    }
    fn cap_of(per_min: u32) -> f64 {
        f64::from(per_min.clamp(1, 5))
    }
    /// Take one if there is one (never waits).
    pub fn try_take(&mut self) -> bool {
        if self.per_min == 0 {
            return false;
        }
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.at).as_secs_f64() * f64::from(self.per_min) / 60.0).min(Self::cap_of(self.per_min));
        self.at = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// What a provider says while the daemon is paused.
pub(crate) const PAUSED: &str = "paused (lbmail resume starts it again)";

impl Shared {
    /// Runs `f` on the store in a blocking task.
    pub async fn db<T: Send + 'static>(&self, f: impl FnOnce(&mut Store) -> Result<T> + Send + 'static) -> Result<T> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut s)
        })
        .await
        .map_err(|e| Error::Other(format!("store task failed: {e}")))?
    }

    pub fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    pub fn rt(&self, id: &AccountId) -> Result<Arc<AccountRt>> {
        self.accounts.read().unwrap_or_else(|e| e.into_inner()).get(id).cloned().ok_or_else(|| Error::NotFound(format!("account {id}")))
    }

    pub fn rts(&self) -> Vec<Arc<AccountRt>> {
        self.accounts.read().unwrap_or_else(|e| e.into_inner()).values().cloned().collect()
    }

    pub fn ai(&self) -> Option<Arc<MailAi>> {
        self.ai.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn is_shutting_down(&self) -> bool {
        *self.shutdown.borrow()
    }

    pub fn is_paused(&self) -> bool {
        *self.paused.borrow()
    }

    /// Pause or resume every provider call; persisted so a restart keeps it.
    pub async fn set_paused(&self, on: bool) -> Result<()> {
        let file = self.paths.paused_file();
        if on {
            crate::fsutil::write_private(&file, b"paused by lbmail pause / daemon.pause\n")?;
        } else if let Err(e) = std::fs::remove_file(&file)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(Error::Other(format!("{}: {e}", file.display())));
        }
        self.paused.send_replace(on);
        eprintln!("lumbridge-maild: {}", if on { "paused: no provider calls until resumed" } else { "resumed" });
        for rt in self.rts() {
            if !on {
                rt.sync_now.notify_one();
                rt.queue_wake.notify_one();
            }
            self.emit_status(&rt).await;
        }
        if !on {
            self.triage_wake.notify_one();
        }
        Ok(())
    }

    /// Whether background triage may fetch one more body for `account` now.
    pub fn take_body_token(&self, rt: &AccountRt) -> bool {
        let per_min = match &rt.config.provider {
            ProviderConfig::Gog { options, .. } => options.triage_bodies_per_min,
            ProviderConfig::Zoho { options, .. } => options.triage_bodies_per_min,
            _ => None,
        }
        .unwrap_or_else(|| self.config.read().unwrap_or_else(|e| e.into_inner()).daemon.triage_bodies_per_min);
        let mut b = self.body_buckets.lock().unwrap_or_else(|e| e.into_inner());
        let bucket = b.entry(rt.account.id.clone()).or_insert_with(|| Bucket::new(per_min));
        if bucket.per_min != per_min {
            *bucket = Bucket::new(per_min);
        }
        bucket.try_take()
    }

    /// Per budget scope, what gog is doing (deduplicated across accounts sharing a host), then each
    /// Zoho connection (shared by the accounts on one sign-in).
    pub fn loads(&self) -> Vec<ProviderLoad> {
        let mut l: Vec<ProviderLoad> = self.gates.all().iter().map(|g| g.load()).collect();
        l.extend(lumbridge_mail_zoho::loads());
        l
    }

    pub async fn status_of(&self, rt: &AccountRt) -> AccountStatus {
        let id = rt.account.id.clone();
        let (last_sync, queued) = self
            .db(move |s| Ok((s.last_sync(&id).unwrap_or(None), s.queued_count(&id).unwrap_or(0))))
            .await
            .unwrap_or((None, 0));
        let st = rt.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
        AccountStatus { account: rt.account.id.clone(), online: st.online, last_sync, queued, error: st.error, paused: self.is_paused(), body_fill: None }
    }

    async fn set_status(&self, rt: &AccountRt, f: impl FnOnce(&mut RtStatus)) {
        let changed = {
            let mut st = rt.status.lock().unwrap_or_else(|e| e.into_inner());
            let before = (st.online, st.error.clone());
            f(&mut st);
            before != (st.online, st.error.clone())
        };
        if changed {
            let status = self.status_of(rt).await;
            self.emit(Event::Status { status });
        }
    }

    async fn emit_status(&self, rt: &AccountRt) {
        let status = self.status_of(rt).await;
        self.emit(Event::Status { status });
    }
}

// ---- building providers and the AI ------------------------------------------------------------

/// A provider that cannot be built (no password, bad settings): every call says why, as `Auth` or
/// `Invalid`, so the account shows the problem in `status` instead of disappearing.
struct Broken {
    account: AccountId,
    error: Error,
}

#[async_trait::async_trait]
impl Provider for Broken {
    fn account(&self) -> &AccountId {
        &self.account
    }
    async fn sync(&self, _: Option<&SyncCursor>) -> Result<Delta> {
        Err(self.error.clone())
    }
    async fn fetch_body(&self, _: &MessageId) -> Result<Body> {
        Err(self.error.clone())
    }
    async fn fetch_raw(&self, _: &MessageId) -> Result<Vec<u8>> {
        Err(self.error.clone())
    }
    async fn fetch_attachment(&self, _: &MessageId, _: &AttachmentId) -> Result<Vec<u8>> {
        Err(self.error.clone())
    }
    async fn apply(&self, _: &Action) -> Result<ApplyOutcome> {
        Err(self.error.clone())
    }
    async fn wait_for_change(&self, t: u64) -> Result<()> {
        tokio::time::sleep(Duration::from_secs(t)).await;
        Ok(())
    }
}

/// Every provider call goes through this: while the daemon is paused it fails at once as `Offline`, and
/// a sync in flight is let go (its gog process, if any, finishes on its own inside the budget).
struct Guarded {
    inner: Arc<dyn Provider>,
    paused: watch::Receiver<bool>,
}

impl Guarded {
    fn check(&self) -> Result<()> {
        if *self.paused.borrow() { Err(Error::Offline(PAUSED.into())) } else { Ok(()) }
    }
}

async fn until_paused(mut rx: watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return std::future::pending().await;
        }
    }
}

#[async_trait::async_trait]
impl Provider for Guarded {
    fn account(&self) -> &AccountId {
        self.inner.account()
    }
    async fn sync(&self, c: Option<&SyncCursor>) -> Result<Delta> {
        self.check()?;
        tokio::select! {
            r = self.inner.sync(c) => r,
            _ = until_paused(self.paused.clone()) => Err(Error::Offline(PAUSED.into())),
        }
    }
    async fn fetch_body(&self, id: &MessageId) -> Result<Body> {
        self.check()?;
        self.inner.fetch_body(id).await
    }
    async fn fetch_body_bundle(&self, id: &MessageId, thread: Option<&ThreadId>) -> Result<BodyBundle> {
        self.check()?;
        self.inner.fetch_body_bundle(id, thread).await
    }
    async fn fetch_raw(&self, id: &MessageId) -> Result<Vec<u8>> {
        self.check()?;
        self.inner.fetch_raw(id).await
    }
    async fn fetch_attachment(&self, m: &MessageId, a: &AttachmentId) -> Result<Vec<u8>> {
        self.check()?;
        self.inner.fetch_attachment(m, a).await
    }
    async fn apply(&self, a: &Action) -> Result<ApplyOutcome> {
        self.check()?;
        self.inner.apply(a).await
    }
    async fn wait_for_change(&self, t: u64) -> Result<()> {
        self.inner.wait_for_change(t).await
    }
    async fn request_backfill(&self, since: Option<i64>) -> Result<()> {
        self.inner.request_backfill(since).await
    }
    fn load(&self) -> Option<ProviderLoad> {
        self.inner.load()
    }
    fn capabilities(&self) -> lumbridge_mail_core::Capabilities {
        self.inner.capabilities()
    }
}

impl Shared {
    /// The provider for one account, inside the pause gate.
    pub(crate) async fn provider_for(&self, cfg: &AccountConfig) -> Arc<dyn Provider> {
        let inner = match self.overrides.get(&cfg.account_id()) {
            Some(p) => p.clone(),
            None => self.build_provider(cfg).await,
        };
        Arc::new(Guarded { inner, paused: self.paused.subscribe() })
    }

    async fn build_provider(&self, cfg: &AccountConfig) -> Arc<dyn Provider> {
        let gog_cfg = self.config.read().unwrap_or_else(|e| e.into_inner()).gog.clone();
        build_provider(cfg, &BuildEnv { gates: &self.gates, gog: &gog_cfg, paths: &self.paths, ssh: &self.ssh, test_runner: self.gog_runner.clone() }).await
    }
}

pub(crate) struct BuildEnv<'a> {
    pub gates: &'a lumbridge_mail_gog::Gates,
    pub gog: &'a crate::config::GogSection,
    pub paths: &'a lumbridge_mail_core::paths::Paths,
    pub ssh: &'a Mutex<HashMap<String, lumbridge_mail_gog::SshRunner>>,
    pub test_runner: Option<Arc<dyn lumbridge_mail_gog::Runner>>,
}

pub(crate) async fn build_provider(cfg: &AccountConfig, env: &BuildEnv<'_>) -> Arc<dyn Provider> {
    let account = cfg.account();
    match &cfg.provider {
        ProviderConfig::Demo { set } => {
            let set = set.as_deref().and_then(DemoSet::parse).unwrap_or_else(|| DemoSet::for_persona(&account.persona));
            Arc::new(DemoProvider::new(account.id.clone(), set, Address::new(Some(&account.name), &account.email)))
        }
        ProviderConfig::Gog { binary, ssh_host, env_file, options } => {
            use lumbridge_mail_gog::runner::Niceness;
            use lumbridge_mail_gog::{AccountLimits, GatedRunner, GogConfig, GogProvider, LocalRunner, Mux, Runner, SshRunner};
            let g = env.gog;
            let niceness = Niceness { nice: Some(g.nice), ionice_idle: g.ionice };
            let (scope, inner): (String, Arc<dyn Runner>) = match ssh_host {
                Some(host) => {
                    let mut r = SshRunner::new(host.clone(), binary.clone()).with_niceness(niceness);
                    if let Some(e) = env_file {
                        r = r.with_env_file(e.clone());
                    }
                    let mut known = env.ssh.lock().unwrap_or_else(|e| e.into_inner());
                    if g.ssh_multiplex {
                        // One master per host, whichever account starts it: accounts on a host share
                        // the Mux (its lock keeps two masters from racing for one socket).
                        let persist = Duration::from_secs(g.ssh_persist_s.max(1));
                        let mux = match known.get(host).and_then(|k| k.mux.clone()).filter(|m| m.persist == persist) {
                            Some(m) => m,
                            None => {
                                let mut m = Mux::new(env.paths.ssh_control_dir());
                                m.persist = persist;
                                Arc::new(m)
                            }
                        };
                        r = r.with_mux(mux);
                    }
                    known.insert(host.clone(), r.clone());
                    drop(known);
                    (format!("ssh:{host}"), Arc::new(r))
                }
                None => {
                    let mut r = LocalRunner::new(expand_home(binary)).with_niceness(niceness);
                    if let Some(e) = env_file {
                        r = r.with_env_file(expand_home(e));
                    }
                    ("local".to_owned(), Arc::new(r))
                }
            };
            let inner = env.test_runner.clone().unwrap_or(inner);
            let gate = env.gates.gate(&scope, g.host_limits(ssh_host.is_some()));
            let limits = AccountLimits { max_in_flight: options.max_in_flight.unwrap_or(g.account_max_in_flight).max(1), retries: g.retries };
            let runner = Arc::new(GatedRunner::new(inner, gate, limits));
            let mut c = GogConfig::new(account.id.clone(), account.email.clone());
            c.name = Some(account.name.clone());
            c.timeout = Duration::from_secs(g.timeout_s.max(5));
            if let Some(v) = options.window_days {
                c.window_days = v;
            }
            if let Some(v) = options.window_max {
                c.window_max = v;
            }
            if let Some(v) = options.page_size {
                c.page_size = v.clamp(1, 500);
            }
            if let Some(v) = options.search_bodies {
                c.search_bodies = v;
            }
            if let Some(v) = options.backfill {
                c.backfill_auto = v;
            }
            if let Some(v) = options.backfill_every_s {
                c.backfill_every = Duration::from_secs(v);
            }
            Arc::new(GogProvider::new(c, runner))
        }
        ProviderConfig::Imap {
            imap_host,
            imap_port,
            imap_security,
            smtp_host,
            smtp_port,
            smtp_security,
            username,
            auth,
            password_env,
            password_command,
            accept_invalid_certs,
            server_saves_sent,
        } => {
            use lumbridge_mail_imap::{Credentials, Identity, ImapAccount, ImapProvider, Security, Server};
            let source = SecretSource { env: password_env.clone(), command: password_command.clone() };
            let id = cfg.id.clone();
            let secret = tokio::task::spawn_blocking(move || secrets::imap_secret(&id, &source))
                .await
                .unwrap_or_else(|e| Err(Error::Other(format!("secret task: {e}"))));
            let secret = match secret {
                Ok(s) => s,
                Err(error) => return Arc::new(Broken { account: account.id.clone(), error }),
            };
            let user = username.clone().unwrap_or_else(|| account.email.clone());
            let creds = match auth {
                crate::config::AuthKind::Password => Credentials::Password { username: user, password: secret },
                crate::config::AuthKind::Oauth2 => Credentials::OAuth2 { username: user, token: secret },
            };
            let port = |p: Option<u16>, s: Security, imap: bool| {
                p.unwrap_or(match (imap, s) {
                    (true, Security::Tls) => 993,
                    (true, _) => 143,
                    (false, Security::Tls) => 465,
                    (false, _) => 587,
                })
            };
            let mut imap = Server::new(imap_host.clone(), port(*imap_port, *imap_security, true), *imap_security);
            imap.accept_invalid_certs = *accept_invalid_certs;
            let smtp = smtp_host.as_ref().map(|h| {
                let mut s = Server::new(h.clone(), port(*smtp_port, *smtp_security, false), *smtp_security);
                s.accept_invalid_certs = *accept_invalid_certs;
                s
            });
            let mut a = ImapAccount::new(
                account.id.clone(),
                Identity { email: account.email.clone(), name: account.name.clone() },
                imap,
                smtp,
                creds,
            );
            a.options.server_saves_sent = *server_saves_sent;
            Arc::new(ImapProvider::new(a))
        }
        ProviderConfig::Resend { .. } => match build_resend(cfg).await {
            Ok(p) => Arc::new(p),
            Err(error) => Arc::new(Broken { account: account.id.clone(), error }),
        },
        ProviderConfig::Zoho { .. } => match build_zoho(cfg) {
            Ok(p) => Arc::new(p),
            Err(error) => Arc::new(Broken { account: account.id.clone(), error }),
        },
    }
}

/// A Zoho provider from its config entry. Accounts with the same connection keys share one connection
/// (token and budget) inside `lumbridge-mail-zoho`.
pub(crate) fn build_zoho(cfg: &AccountConfig) -> Result<lumbridge_mail_zoho::ZohoProvider> {
    use lumbridge_mail_zoho::{Connection, FlagKind, ZohoConfig, ZohoProvider};
    let ProviderConfig::Zoho { mcp_url, token_url, client_id, token_file, zoho_account_id, options: o } = &cfg.provider else {
        return Err(Error::Unsupported(format!("{} is not a Zoho account", cfg.id)));
    };
    let account = cfg.account();
    let conn = Connection {
        mcp_url: mcp_url.trim().to_owned(),
        token_url: token_url.trim().to_owned(),
        client_id: client_id.trim().to_owned(),
        token_file: expand_home(token_file.trim()).into(),
    };
    let mut z = ZohoConfig::new(account.id.clone(), account.email.clone(), conn);
    z.name = Some(account.name.clone()).filter(|n| !n.trim().is_empty() && *n != account.email);
    z.zoho_account_id = zoho_account_id.clone();
    if let Some(v) = o.window_days {
        z.window_days = v;
    }
    if let Some(v) = o.window_max {
        z.window_max = v;
    }
    if let Some(v) = o.backfill {
        z.backfill_auto = v;
    }
    if let Some(v) = o.backfill_every_s {
        z.backfill_every = Duration::from_secs(v);
    }
    if let Some(v) = o.page_size {
        z.page_size = v.clamp(1, 200);
    }
    if let Some(v) = o.poll_size {
        z.poll_size = v.clamp(1, 200);
    }
    if let Some(v) = o.poll_interval_s {
        z.poll_interval = Duration::from_secs(v.max(5));
    }
    if let Some(v) = o.max_in_flight {
        z.limits.max_in_flight = v.max(1);
    }
    if let Some(v) = o.max_calls_per_min {
        z.limits.max_calls_per_min = v.max(1);
    }
    if let Some(f) = o.flag.as_deref().and_then(FlagKind::parse) {
        z.flag = f;
    }
    if let Some(v) = o.sync_archive {
        z.sync_archive = v;
    }
    if let Some(v) = &o.list_fields {
        z.list_fields = v.clone();
    }
    ZohoProvider::new(z)
}

/// A Resend provider from its config entry, with the API key from the env, the command or the keyring.
/// Also used on demand by `resend.domains` / `resend.api_keys`.
pub(crate) async fn build_resend(cfg: &AccountConfig) -> Result<lumbridge_mail_resend::ResendProvider> {
    use lumbridge_mail_resend::{AddressPattern, ApiKey, ResendConfig, ResendProvider};
    let ProviderConfig::Resend { base_url, addresses, receiving, reply_to, api_key_env, api_key_command, requests_per_second } = &cfg.provider
    else {
        return Err(Error::Unsupported(format!("{} is not a Resend account", cfg.id)));
    };
    let source = SecretSource { env: api_key_env.clone(), command: api_key_command.clone() };
    let id = cfg.id.clone();
    let key = tokio::task::spawn_blocking(move || secrets::resend_secret(&id, &source))
        .await
        .unwrap_or_else(|e| Err(Error::Other(format!("secret task: {e}"))))?;
    let account = cfg.account();
    let mut r = ResendConfig::new(account.id.clone(), account.email.clone(), ApiKey::new(key));
    r.name = Some(account.name.clone()).filter(|n| !n.trim().is_empty() && *n != account.email);
    if let Some(b) = base_url {
        r.base_url = b.clone();
    }
    r.addresses = addresses
        .iter()
        .map(|a| AddressPattern::parse(a).ok_or_else(|| Error::Invalid(format!("{}: {a:?} is not an address or @domain", cfg.id))))
        .collect::<Result<_>>()?;
    r.receiving = *receiving;
    r.reply_to = reply_to.clone();
    if let Some(n) = requests_per_second {
        r.requests_per_second = f64::from((*n).max(1));
    }
    ResendProvider::new(r)
}

fn expand_home(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_owned(),
    }
}

pub(crate) fn build_ai(cfg: &Config, model_override: Option<Arc<dyn Model>>) -> Option<Arc<MailAi>> {
    if let Some(m) = model_override {
        return Some(Arc::new(MailAi::from_arc(m, cfg.ai.settings.clone())));
    }
    if !cfg.ai.enabled {
        return None;
    }
    let mut endpoint = cfg.ai.endpoint.clone().with_env();
    if let Some(var) = &cfg.ai.api_key_env {
        endpoint.api_key = std::env::var(var).ok().filter(|k| !k.is_empty());
    }
    match OpenAiModel::new(endpoint) {
        Ok(m) => Some(Arc::new(MailAi::new(m, cfg.ai.settings.clone()))),
        Err(e) => {
            eprintln!("lumbridge-maild: AI endpoint not usable ({}); triage runs on rules only", e);
            None
        }
    }
}

// ---- accounts: start, stop, reload --------------------------------------------------------------

impl Shared {
    /// Make the running accounts match the config: start new ones, restart changed ones, stop removed
    /// ones (their stored mail is kept, hidden, until `accounts.purge`; the server is not touched). A
    /// restarted account's gog processes in flight are not killed: they finish inside the budget.
    pub async fn apply_config(self: &Arc<Self>, cfg: Config) -> Result<usize> {
        let wanted: BTreeMap<AccountId, AccountConfig> = cfg.accounts.iter().map(|a| (a.account_id(), a.clone())).collect();
        let current: Vec<Arc<AccountRt>> = self.rts();
        for rt in &current {
            let keep = wanted.get(&rt.account.id).is_some_and(|w| *w == rt.config);
            if !keep {
                self.stop_account(rt).await;
                self.accounts.write().unwrap_or_else(|e| e.into_inner()).remove(&rt.account.id);
            }
        }
        // Accounts in the store that the config no longer has: stopped, their mail kept (and hidden)
        // until `accounts.purge`, so adding one back costs no resync.
        let kept: Vec<AccountId> = self.db(|s| s.kept_accounts()).await?;
        let known: Vec<AccountId> = self.db(|s| Ok(s.accounts()?.into_iter().map(|a| a.id).collect())).await?;
        for id in known {
            if !wanted.contains_key(&id) && !kept.contains(&id) {
                eprintln!("lumbridge-maild: account {id} is no longer in the config; its mail is kept (lbmail account purge {id} deletes it)");
                self.db(move |s| s.keep_account(&id).map(|_| ())).await?;
            }
        }
        {
            let ai = build_ai(&cfg, self.model_override.clone());
            *self.ai.write().unwrap_or_else(|e| e.into_inner()) = ai;
        }
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = cfg.clone();
        for (id, ac) in wanted {
            if self.accounts.read().unwrap_or_else(|e| e.into_inner()).contains_key(&id) {
                continue;
            }
            let account = ac.account();
            let a2 = account.clone();
            self.db(move |s| s.upsert_account(&a2)).await?;
            let provider = self.provider_for(&ac).await;
            let (stop, _) = watch::channel(false);
            let rt = Arc::new(AccountRt {
                account,
                config: ac,
                provider,
                sync_now: Notify::new(),
                queue_wake: Notify::new(),
                stop,
                status: Mutex::new(RtStatus::default()),
                auth_blocked: AtomicBool::new(false),
                tasks: Mutex::new(Vec::new()),
                fg: Default::default(),
                fg_last: Mutex::new(Instant::now()),
                fill: Default::default(),
            });
            self.accounts.write().unwrap_or_else(|e| e.into_inner()).insert(id, rt.clone());
            let sync = tokio::spawn(sync_loop(self.clone(), rt.clone()));
            let queue = tokio::spawn(queue_loop(self.clone(), rt.clone()));
            let fill = tokio::spawn(crate::fill::fill_loop(self.clone(), rt.clone()));
            rt.tasks.lock().unwrap_or_else(|e| e.into_inner()).extend([sync, queue, fill]);
        }
        self.triage_wake.notify_one();
        crate::mirror::configure(self).await;
        crate::pgwork::configure(self).await;
        Ok(self.accounts.read().unwrap_or_else(|e| e.into_inner()).len())
    }

    /// Stops an account's loops: tells them, waits a little for an action being sent, then aborts.
    pub async fn stop_account(&self, rt: &AccountRt) {
        // send_replace: a loop that has not started yet must still see it (send fails with no receivers).
        rt.stop.send_replace(true);
        rt.sync_now.notify_waiters();
        rt.queue_wake.notify_waiters();
        let tasks: Vec<JoinHandle<()>> = std::mem::take(&mut *rt.tasks.lock().unwrap_or_else(|e| e.into_inner()));
        for t in tasks {
            let abort = t.abort_handle();
            if tokio::time::timeout(Duration::from_secs(10), t).await.is_err() {
                abort.abort();
            }
        }
    }

    pub fn retry_policy(&self) -> RetryPolicy {
        let c = self.config.read().unwrap_or_else(|e| e.into_inner());
        RetryPolicy { first_delay_s: c.daemon.first_retry_s.max(1), max_delay_s: c.daemon.max_retry_s.max(1), max_attempts: c.daemon.max_attempts.max(1) }
    }

    fn sync_interval(&self) -> u64 {
        self.config.read().unwrap_or_else(|e| e.into_inner()).daemon.sync_interval_s.max(1)
    }

    fn max_backoff(&self) -> u64 {
        self.config.read().unwrap_or_else(|e| e.into_inner()).daemon.max_backoff_s.max(1)
    }
}

/// While the daemon is paused: wait for resume (true) or stop (false).
async fn wait_unpaused(shared: &Shared, stop: &mut watch::Receiver<bool>) -> bool {
    let mut p = shared.paused.subscribe();
    loop {
        if !*p.borrow_and_update() {
            return true;
        }
        tokio::select! {
            r = p.changed() => if r.is_err() { return false },
            _ = stop.changed() => return false,
        }
    }
}

/// Waits until `d` passes, the account is told to sync, or it is stopped. True when stopped.
async fn pause(rt: &AccountRt, stop: &mut watch::Receiver<bool>, d: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = rt.sync_now.notified() => false,
        _ = stop.changed() => true,
    }
}

// ---- the sync loop --------------------------------------------------------------------------------

/// Accounts whose provider hands out no thread ids (Zoho's listings): the store groups their mail
/// (`Store::assign_threads`, `Store::learn_thread_headers`).
pub(crate) fn threads_by_store(rt: &AccountRt) -> bool {
    rt.account.provider == ProviderKind::Zoho
}

/// The version of the store's threading for such accounts. When it is newer than the one an account's
/// mail was last grouped with, the account's stored mail is grouped again once (`Store::rethread`): no
/// provider call, no resync. Done is noted as `data/threads-v<N>/<hash of the account id>`.
pub const THREADING_VERSION: u32 = 1;

/// The file that says `account` was grouped with [`THREADING_VERSION`].
pub fn threading_marker(paths: &lumbridge_mail_core::paths::Paths, account: &AccountId) -> std::path::PathBuf {
    paths.data_dir.join(format!("threads-v{THREADING_VERSION}")).join(crate::fsutil::hashed(&[account.as_str()]))
}

async fn rethread_once(shared: &Arc<Shared>, rt: &Arc<AccountRt>) {
    let id = rt.account.id.clone();
    let marker = threading_marker(&shared.paths, &id);
    if marker.exists() {
        return;
    }
    let a = id.clone();
    match shared.db(move |s| s.rethread(&a)).await {
        Ok(changed) => {
            if !changed.is_empty() {
                eprintln!("lumbridge-maild: {id}: grouped stored mail into threads ({} threads changed)", changed.len());
                shared.emit(Event::Changed { account: id.clone(), threads: changed });
            }
            let noted = marker
                .parent()
                .map_or(Ok(()), crate::fsutil::private_dir)
                .and_then(|_| crate::fsutil::write_private(&marker, id.as_str().as_bytes()));
            if let Err(e) = noted {
                eprintln!("lumbridge-maild: {id}: could not note the threading upgrade: {e}");
            }
        }
        Err(e) => eprintln!("lumbridge-maild: {id}: grouping stored mail into threads failed: {e}"),
    }
}

async fn sync_loop(shared: Arc<Shared>, rt: Arc<AccountRt>) {
    let mut stop = rt.stop.subscribe();
    if threads_by_store(&rt) {
        rethread_once(&shared, &rt).await;
    }
    let mut backoff = 2u64;
    let mut pages = 0u32;
    loop {
        if *stop.borrow() || shared.is_shutting_down() {
            return;
        }
        if shared.is_paused() {
            shared.emit_status(&rt).await;
            if !wait_unpaused(&shared, &mut stop).await {
                return;
            }
            continue;
        }
        if rt.auth_blocked.load(Ordering::SeqCst) {
            // Signing in again is the user's job; `sync.now` means they did something.
            tokio::select! {
                _ = rt.sync_now.notified() => rt.auth_blocked.store(false, Ordering::SeqCst),
                _ = stop.changed() => return,
            }
        }
        let id = rt.account.id.clone();
        let cursor = match shared.db(move |s| s.sync_cursor(&id)).await {
            Ok(c) => c,
            Err(e) => {
                shared.set_status(&rt, |s| s.error = Some(e)).await;
                if pause(&rt, &mut stop, Duration::from_secs(30)).await {
                    return;
                }
                continue;
            }
        };
        let had_cursor = cursor.is_some();
        let result = {
            let _busy = rt.foreground();
            tokio::select! {
                r = rt.provider.sync(cursor.as_ref()) => r,
                _ = stop.changed() => return,
            }
        };
        match result {
            Ok(delta) => {
                backoff = 2;
                let more = delta.more;
                // A first or full sync, or older mail being filled in: nothing to announce.
                let quiet = delta.reset || !had_cursor || delta.backfill;
                match apply_delta(&shared, &rt, delta, quiet).await {
                    Ok(()) => {
                        shared.set_status(&rt, |s| {
                            s.online = true;
                            s.error = None;
                            s.syncing = more;
                        })
                        .await;
                    }
                    Err(e) => {
                        shared.set_status(&rt, |s| s.error = Some(e)).await;
                        if pause(&rt, &mut stop, Duration::from_secs(30)).await {
                            return;
                        }
                        continue;
                    }
                }
                if more {
                    pages += 1;
                    // Let readers in between pages of a long first sync.
                    if pages.is_multiple_of(20) {
                        tokio::task::yield_now().await;
                    }
                    continue;
                }
                pages = 0;
                shared.triage_wake.notify_one();
                let wait = shared.sync_interval();
                let changed = tokio::select! {
                    r = rt.provider.wait_for_change(wait) => r,
                    _ = rt.sync_now.notified() => Ok(()),
                    _ = stop.changed() => return,
                };
                if let Err(e) = changed
                    && e.is_retryable()
                {
                    shared.set_status(&rt, |s| { s.online = false; s.error = Some(e) }).await;
                    if pause(&rt, &mut stop, Duration::from_secs(backoff)).await {
                        return;
                    }
                    backoff = (backoff * 2).min(shared.max_backoff());
                }
            }
            Err(Error::ResyncNeeded(_)) => {
                let id = rt.account.id.clone();
                if let Err(e) = shared.db(move |s| s.reset_sync_cursor(&id)).await {
                    shared.set_status(&rt, |s| s.error = Some(e)).await;
                    if pause(&rt, &mut stop, Duration::from_secs(30)).await {
                        return;
                    }
                }
            }
            Err(e @ Error::Offline(_)) => {
                shared.set_status(&rt, |s| { s.online = false; s.error = Some(e) }).await;
                if pause(&rt, &mut stop, Duration::from_secs(backoff)).await {
                    return;
                }
                backoff = (backoff * 2).min(shared.max_backoff());
            }
            Err(e @ Error::Auth(_)) => {
                rt.auth_blocked.store(true, Ordering::SeqCst);
                shared.set_status(&rt, |s| { s.online = false; s.error = Some(e) }).await;
            }
            Err(e) => {
                shared.set_status(&rt, |s| s.error = Some(e)).await;
                if pause(&rt, &mut stop, Duration::from_secs(shared.sync_interval().min(60))).await {
                    return;
                }
            }
        }
    }
}

/// Applies a delta and announces it. `quiet`: a first or full sync, so no `NewMail` per message.
async fn apply_delta(shared: &Arc<Shared>, rt: &Arc<AccountRt>, mut delta: Delta, quiet: bool) -> Result<()> {
    let id = rt.account.id.clone();
    if threads_by_store(rt) {
        let a = id.clone();
        delta = shared
            .db(move |s| {
                s.assign_threads(&a, &mut delta.upserted)?;
                Ok(delta)
            })
            .await?;
    }
    let metas: HashMap<MessageId, MessageMeta> = delta.upserted.iter().map(|m| (m.id.clone(), m.clone())).collect();
    let applied = shared.db(move |s| s.apply_delta(&id, &delta)).await?;
    if !applied.changed_threads.is_empty() {
        shared.emit(Event::Changed { account: rt.account.id.clone(), threads: applied.changed_threads.clone() });
    }
    if !quiet {
        for mid in &applied.new_messages {
            let Some(m) = metas.get(mid) else { continue };
            let in_inbox = m.mailboxes.iter().any(|b| matches!(b.as_str(), "INBOX" | "inbox")) || m.mailboxes.is_empty();
            let mine = m.from.first().is_some_and(|a| a.email.eq_ignore_ascii_case(&rt.account.email));
            if m.flags.seen || mine || !in_inbox {
                continue;
            }
            let from = m.from.first().map(lumbridge_mail_ai::text::who).unwrap_or_else(|| "someone".into());
            // Sensitive mail is never quoted in a notification: only its one line.
            let subject = crate::redact::sensitive_line_for_meta(m).unwrap_or_else(|| m.subject.clone());
            shared.emit(Event::NewMail { account: rt.account.id.clone(), thread: m.thread.clone(), from, subject });
        }
    }
    if !applied.new_messages.is_empty() {
        shared.triage_wake.notify_one();
    }
    Ok(())
}

// ---- the action queue runner --------------------------------------------------------------------

async fn queue_loop(shared: Arc<Shared>, rt: Arc<AccountRt>) {
    let mut stop = rt.stop.subscribe();
    loop {
        if *stop.borrow() || shared.is_shutting_down() {
            return;
        }
        if shared.is_paused() && !wait_unpaused(&shared, &mut stop).await {
            return;
        }
        let id = rt.account.id.clone();
        let t = now();
        let job = match shared.db(move |s| s.next_action_for(&id, t)).await {
            Ok(j) => j,
            Err(_) => {
                tokio::select! { _ = tokio::time::sleep(Duration::from_secs(5)) => {}, _ = stop.changed() => return }
                continue;
            }
        };
        let Some(job) = job else {
            let id = rt.account.id.clone();
            let due = shared.db(move |s| s.next_action_due(Some(&id))).await.ok().flatten();
            let wait = due.map(|d| (d - now()).clamp(0, 3600) as u64).unwrap_or(3600);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(wait * 1000 + 50)) => {},
                _ = rt.queue_wake.notified() => {},
                _ = stop.changed() => return,
            }
            continue;
        };
        let result = if rt.readonly() {
            // Queued before the account was made read-only: it fails (and is rolled back), never sent.
            Err(Error::Refused(format!("account {} is read-only", rt.account.id)))
        } else {
            // An action being sent is finished even during shutdown (bounded by stop_account's grace).
            let _busy = rt.foreground();
            rt.provider.apply(&job.action).await
        };
        let qid = job.id;
        match result {
            Ok(outcome) => {
                let _ = shared.db(move |s| s.action_done_with(qid, &outcome)).await;
                shared.emit_status(&rt).await;
                // Reconcile with what the server now says.
                rt.sync_now.notify_one();
            }
            Err(e) => {
                let policy = shared.retry_policy();
                let e2 = e.clone();
                let out = shared
                    .db(move |s| {
                        s.set_retry_policy(policy);
                        s.action_failed(qid, &e2, now())
                    })
                    .await;
                match out {
                    Ok(FailOutcome::Retry { .. }) => {
                        if e.is_retryable() {
                            shared.set_status(&rt, |s| { s.online = false; s.error = Some(e) }).await;
                        }
                    }
                    Ok(FailOutcome::GaveUp { changed_threads }) => {
                        shared.emit(Event::ActionFailed { account: rt.account.id.clone(), action: job.action.clone(), error: e.clone() });
                        if !changed_threads.is_empty() {
                            shared.emit(Event::Changed { account: rt.account.id.clone(), threads: changed_threads });
                        }
                        if matches!(e, Error::Auth(_)) {
                            rt.auth_blocked.store(true, Ordering::SeqCst);
                            shared.set_status(&rt, |s| s.error = Some(e)).await;
                        } else {
                            shared.emit_status(&rt).await;
                        }
                    }
                    Err(_) => {}
                }
            }
        }
    }
}

// ---- the budget log line ------------------------------------------------------------------------

/// Once a minute while a provider is busy: gog's process starts per host, Zoho's calls per sign-in, so a
/// live run can be watched
/// (`journalctl --user -u lumbridge-maild -f`, or the app's daemon log).
pub(crate) async fn budget_log_loop(shared: Arc<Shared>) {
    let mut down = shared.shutdown.subscribe();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(60)) => {},
            _ = down.changed() => return,
        }
        if !shared.config.read().unwrap_or_else(|e| e.into_inner()).daemon.log_budget {
            continue;
        }
        for l in shared.loads() {
            if l.starts_last_min == 0 && l.in_flight == 0 && l.breaker_open_until.is_none() {
                continue;
            }
            eprintln!("lumbridge-maild: {}", describe_load(&l));
        }
    }
}

/// One line about one budget scope, in its provider's words (also used by `lbmail status`).
pub fn describe_load(l: &ProviderLoad) -> String {
    l.describe(now())
}

// ---- background triage --------------------------------------------------------------------------

pub(crate) async fn triage_loop(shared: Arc<Shared>) {
    let mut down = shared.shutdown.subscribe();
    loop {
        tokio::select! {
            _ = shared.triage_wake.notified() => {},
            _ = tokio::time::sleep(Duration::from_secs(60)) => {},
            _ = down.changed() => return,
        }
        if shared.is_shutting_down() {
            return;
        }
        let enabled = shared.config.read().unwrap_or_else(|e| e.into_inner()).daemon.triage;
        if !enabled || shared.is_paused() {
            continue;
        }
        loop {
            let refs = match shared.db(|s| s.threads_needing_triage(200)).await {
                Ok(r) => r,
                Err(_) => break,
            };
            let todo: Vec<ThreadRef> = {
                let mut skip = shared.triage_skip.lock().unwrap_or_else(|e| e.into_inner());
                let t = Instant::now();
                skip.retain(|_, until| *until > t);
                refs.into_iter().filter(|r| !skip.contains_key(&(r.account.clone(), r.thread.clone()))).take(32).collect()
            };
            if todo.is_empty() {
                break;
            }
            let concurrency = shared.ai().map(|a| a.config().concurrency.max(1)).unwrap_or(4);
            use futures::StreamExt;
            // The body budget goes to the newest threads first, whichever triage gets there first.
            let turns = Turns::new(todo.len());
            let results: Vec<(ThreadRef, Result<Triage>)> = futures::stream::iter(todo.into_iter().enumerate().map(|(i, r)| {
                let shared = shared.clone();
                let turn = Turn { turns: turns.clone(), idx: i };
                async move {
                    let out = triage_thread_in_turn(&shared, &r, false, Some(&turn)).await;
                    (r, out)
                }
            }))
            .buffer_unordered(concurrency)
            .collect()
            .await;
            let t = Instant::now();
            for (r, out) in results {
                let wait = match &out {
                    // Future-dated mail or a triage older than the thread: look again later, not now.
                    Ok(_) => Duration::from_secs(600),
                    Err(Error::Offline(_)) => Duration::from_secs(120),
                    Err(_) => Duration::from_secs(3600),
                };
                shared.triage_skip.lock().unwrap_or_else(|e| e.into_inner()).insert((r.account, r.thread), t + wait);
            }
            if shared.is_shutting_down() {
                return;
            }
            if shared.is_paused() {
                break;
            }
        }
    }
}

/// The thread as the AI layer takes it: metadata plus cached bodies; with `fetch`, the latest
/// message's body is fetched (and cached) when missing.
pub(crate) async fn load_thread(shared: &Arc<Shared>, r: &ThreadRef, fetch: bool) -> Result<MailThread> {
    let (a, t) = (r.account.clone(), r.thread.clone());
    let metas = shared.db(move |s| s.thread(&a, &t)).await?;
    if metas.is_empty() {
        return Err(Error::NotFound(format!("thread {}", r.thread)));
    }
    let mut messages = Vec::with_capacity(metas.len());
    let last = metas.len() - 1;
    for (i, m) in metas.into_iter().enumerate() {
        let (a, id) = (r.account.clone(), m.id.clone());
        let mut body = shared.db(move |s| s.body(&a, &id)).await?;
        if body.is_none() && fetch && i == last {
            body = crate::api::fetch_body(shared, &r.account, &m.id).await.ok();
        }
        messages.push(MailMessage::new(m, body));
    }
    MailThread::new(messages)
}

/// Gmail's own categories, as labels on the message: free, and enough to keep bulk mail away from
/// body fetches and models. `Updates` is left to the model (bills, shipping), but without a body fetch.
fn category_by_labels(thread: &MailThread, me: &str) -> Option<Category> {
    let user_wrote = thread.messages().iter().any(|m| m.sender().is_some_and(|a| a.email.eq_ignore_ascii_case(me)));
    if user_wrote {
        return None;
    }
    let boxes = &thread.latest().meta.mailboxes;
    let has = |l: &str| boxes.iter().any(|b| b.as_str() == l);
    if has("CATEGORY_PROMOTIONS") {
        Some(Category::Promotion)
    } else if has("CATEGORY_SOCIAL") {
        Some(Category::Notification)
    } else if has("CATEGORY_FORUMS") {
        Some(Category::Newsletter)
    } else {
        None
    }
}

fn rules_triage(thread: &MailThread, me: &str, t: i64) -> Option<Triage> {
    if let Some(tr) = lumbridge_mail_ai::triage_by_rules(thread, me, t) {
        return Some(tr);
    }
    let c = category_by_labels(thread, me)?;
    let who = thread.latest().sender().map(lumbridge_mail_ai::text::who).unwrap_or_else(|| "someone".into());
    let label = match c {
        Category::Promotion => "Promotion",
        Category::Newsletter => "Newsletter",
        _ => "Notification",
    };
    Some(Triage {
        category: c,
        priority: lumbridge_mail_ai::rules::rule_priority(c),
        summary: lumbridge_mail_ai::text::one_line(&format!("{label} from {who}"), 120),
        needs_reply: false,
        due: None,
        by: lumbridge_mail_ai::rules::BY_RULES.into(),
        at: t,
        suspicious: false,
    })
}

/// Rules first, on what is stored (metadata, cached bodies); a model only when the account's policy
/// allows it at the model's place and the rules did not decide (sensitive mail never gets past the
/// rules). A body is fetched only on the way to a model, only when this account's triage body budget
/// (`triage_bodies_per_min`) has room (or `force_model`: a person asked), and never for mail Gmail
/// filed as Updates. Stores the result and announces it.
pub(crate) async fn triage_thread(shared: &Arc<Shared>, r: &ThreadRef, force_model: bool) -> Result<Triage> {
    triage_thread_in_turn(shared, r, force_model, None).await
}

/// Hands out a batch's body budget in the batch's order (newest thread first), whatever order the
/// concurrent triages reach it in: a triage asks for a body token only once every earlier one in the
/// batch has asked or will not (`Turn` passes when dropped).
pub(crate) struct Turns {
    /// Which turns have passed, and the first that has not.
    state: Mutex<(usize, Vec<bool>)>,
    moved: Notify,
}

impl Turns {
    pub(crate) fn new(n: usize) -> Arc<Self> {
        Arc::new(Self { state: Mutex::new((0, vec![false; n])), moved: Notify::new() })
    }
}

pub(crate) struct Turn {
    turns: Arc<Turns>,
    idx: usize,
}

impl Turn {
    /// Until every earlier turn has passed.
    async fn wait(&self) {
        loop {
            let moved = self.turns.moved.notified();
            if self.turns.state.lock().unwrap_or_else(|e| e.into_inner()).0 >= self.idx {
                return;
            }
            moved.await;
        }
    }

    fn pass(&self) {
        {
            let mut st = self.turns.state.lock().unwrap_or_else(|e| e.into_inner());
            let (first, passed) = &mut *st;
            if passed[self.idx] {
                return;
            }
            passed[self.idx] = true;
            while *first < passed.len() && passed[*first] {
                *first += 1;
            }
        }
        self.turns.moved.notify_waiters();
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.pass();
    }
}

async fn triage_thread_in_turn(shared: &Arc<Shared>, r: &ThreadRef, force_model: bool, turn: Option<&Turn>) -> Result<Triage> {
    let rt = shared.rt(&r.account)?;
    let fetch = shared.config.read().unwrap_or_else(|e| e.into_inner()).daemon.fetch_bodies_for_triage;
    let mut thread = load_thread(shared, r, false).await?;
    let t = now();
    let me = rt.account.email.clone();
    let triage = match rules_triage(&thread, &me, t) {
        Some(tr) => tr,
        None => {
            let ai = shared.ai();
            match ai.as_ref().filter(|ai| safety::ai_allowed(&rt.account, ai.place())) {
                Some(ai) => {
                    let updates = thread.latest().meta.mailboxes.iter().any(|b| b.as_str() == "CATEGORY_UPDATES");
                    let missing = thread.latest().body.is_none();
                    let mut decided = None;
                    let wants = missing && fetch && !updates && !shared.is_paused();
                    if wants && !force_model && let Some(t) = turn {
                        t.wait().await;
                    }
                    let granted = wants && (force_model || shared.take_body_token(&rt));
                    if let Some(t) = turn {
                        t.pass();
                    }
                    if granted {
                        let latest = thread.latest().meta.id.clone();
                        if crate::api::fetch_body(shared, &r.account, &latest).await.is_ok() {
                            thread = load_thread(shared, r, false).await?;
                            // With the body the header rules (List-Id, ...) may decide after all.
                            decided = rules_triage(&thread, &me, t);
                        }
                    }
                    match decided {
                        Some(tr) => tr,
                        None => ai.for_account(&rt.account)?.triage(&thread).await?,
                    }
                }
                None if force_model => {
                    return Err(Error::Refused(match ai {
                        None => "AI is off in the config".into(),
                        Some(ai) => format!("this account's AI policy does not allow a {} model", place_name(ai.place())),
                    }));
                }
                None => plain_triage(&thread, t),
            }
        }
    };
    let (a, th, tr) = (r.account.clone(), r.thread.clone(), triage.clone());
    shared.db(move |s| s.put_triage(&a, &th, &tr)).await?;
    shared.emit(Event::Changed { account: r.account.clone(), threads: vec![r.thread.clone()] });
    Ok(triage)
}

fn place_name(p: ModelPlace) -> &'static str {
    match p {
        ModelPlace::Local => "local",
        ModelPlace::Hosted => "hosted",
    }
}

/// For accounts whose mail may not reach any model: a neutral row the rules could not improve on.
fn plain_triage(thread: &MailThread, t: i64) -> Triage {
    let who = thread.latest().sender().map(lumbridge_mail_ai::text::who).unwrap_or_else(|| "someone".into());
    Triage {
        category: Category::Other,
        priority: 40,
        summary: format!("Mail from {who}"),
        needs_reply: false,
        due: None,
        by: lumbridge_mail_ai::rules::BY_RULES.into(),
        at: t,
        suspicious: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The race behind the flaky `triage_fetches_bodies_only_for_the_model_and_within_its_budget`:
    /// concurrent triages reached the body budget in whatever order their store reads finished, so an
    /// older thread could take a token before a newer one. With turns they ask in the batch's order,
    /// and a turn that never asks (rules decided, an error) does not hold the others up.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_body_budget_is_asked_for_in_the_batch_order() {
        for _ in 0..50 {
            let turns = Turns::new(5);
            let order = Arc::new(Mutex::new(Vec::new()));
            let mut tasks = Vec::new();
            // Started newest last, and the later ones quicker: without turns the order would be reversed.
            for i in (0..5).rev() {
                let (turns, order) = (turns.clone(), order.clone());
                tasks.push(tokio::spawn(async move {
                    let turn = Turn { turns, idx: i };
                    tokio::time::sleep(Duration::from_millis(5 * (5 - i as u64))).await;
                    if i == 2 {
                        return; // decided by the rules: never asks, passes when dropped
                    }
                    turn.wait().await;
                    order.lock().unwrap().push(i);
                    turn.pass();
                }));
            }
            for t in tasks {
                t.await.unwrap();
            }
            assert_eq!(*order.lock().unwrap(), vec![0, 1, 3, 4]);
        }
    }
}
