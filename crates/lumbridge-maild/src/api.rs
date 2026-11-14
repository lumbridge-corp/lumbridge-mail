//! The RPC methods (`core::rpc::method`), one function each, on behalf of one connection.

use crate::engine::{Shared, load_thread, triage_thread};
use crate::redact;
use lumbridge_mail_ai::{ReplyRequest, Search, SearchHit};
use lumbridge_mail_core::rpc::*;
use lumbridge_mail_core::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// What the daemon knows about the other end of one connection.
#[derive(Default)]
pub struct Conn {
    /// Presented the person token in `hello`.
    pub person: AtomicBool,
}

impl Conn {
    pub fn is_person(&self) -> bool {
        self.person.load(Ordering::SeqCst)
    }
}

#[derive(Deserialize, Default)]
struct AccountParam {
    #[serde(default)]
    account: Option<AccountId>,
}

#[derive(Deserialize)]
struct RequiredAccount {
    account: AccountId,
}

#[derive(Deserialize)]
struct AskParams {
    question: String,
    #[serde(default)]
    accounts: Option<Vec<AccountId>>,
}

#[derive(Deserialize)]
struct DiscoverParams {
    email: String,
}

#[derive(Serialize)]
struct PathResult {
    path: String,
}

fn params<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
    // Missing params are `{}` (JSON-RPC allows leaving them out).
    let v = if v.is_null() { json!({}) } else { v };
    serde_json::from_value(v).map_err(|e| Error::Invalid(format!("params: {e}")))
}

fn to_value<T: Serialize>(v: T) -> Result<Value> {
    serde_json::to_value(v).map_err(|e| Error::Other(format!("result: {e}")))
}

fn same_secret(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub async fn dispatch(shared: &Arc<Shared>, conn: &Conn, method_name: &str, p: Value) -> Result<Value> {
    use lumbridge_mail_core::rpc::method as m;
    match method_name {
        m::HELLO => hello(shared, conn, params(p)?),
        m::ACCOUNTS => to_value(accounts(shared)),
        m::MAILBOXES => {
            let a: AccountParam = params(p)?;
            if let Some(id) = &a.account {
                shared.rt(id)?;
            }
            to_value(shared.db(move |s| s.mailboxes(a.account.as_ref())).await?)
        }
        m::THREADS => to_value(threads(shared, conn, params(p)?).await?),
        m::THREAD => to_value(thread(shared, conn, params(p)?).await?),
        m::BODY => to_value(body(shared, conn, params(p)?).await?),
        m::ATTACHMENT => to_value(attachment(shared, conn, params(p)?).await?),
        m::ATTACHMENT_READ => to_value(attachment_read(shared, conn, params(p)?).await?),
        m::ACT => to_value(json!({ "queued": act(shared, conn, params(p)?).await? })),
        m::SYNC => {
            let a: AccountParam = params(p)?;
            sync_now(shared, a.account.as_ref())?;
            Ok(json!({}))
        }
        m::STATUS => {
            let mut out = Vec::new();
            for rt in shared.rts() {
                let mut st = shared.status_of(&rt).await;
                st.body_fill = crate::fill::status(shared, &rt).await;
                out.push(st);
            }
            to_value(out)
        }
        m::SYNC_BODIES => to_value(json!({ "accounts": crate::fill::ask(shared, params(p)?).await? })),
        m::COUNTS => {
            let r: CountsRequest = params(p)?;
            to_value(shared.db(move |s| s.category_counts(r.accounts.as_deref(), r.role)).await?)
        }
        m::AI_TRIAGE => {
            let r: ThreadRef = params(p)?;
            let t = triage_thread(shared, &r, false).await?;
            to_value(if conn.is_person() { t } else { agent_triage(shared, &r, t).await })
        }
        m::AI_SUMMARISE => to_value(json!({ "summary": summarise(shared, params(p)?).await? })),
        m::AI_DRAFT => to_value(draft(shared, params(p)?).await?),
        m::AI_ASK => {
            let a: AskParams = params(p)?;
            to_value(ask(shared, a).await?)
        }
        m::AI_EXTRACT => to_value(extract(shared, params(p)?).await?),
        m::ACCOUNTS_ADD => to_value(add_account(shared, conn, params(p)?).await?),
        m::ACCOUNTS_DISCOVER => {
            let d: DiscoverParams = params(p)?;
            to_value(crate::setup::discover(&d.email)?)
        }
        m::ACCOUNTS_CAPABILITIES => {
            let a: AccountParam = params(p)?;
            let rts = match &a.account {
                Some(id) => vec![shared.rt(id)?],
                None => shared.rts(),
            };
            to_value(rts.iter().map(|rt| AccountCapabilities { account: rt.account.id.clone(), capabilities: capabilities(rt) }).collect::<Vec<_>>())
        }
        m::RESEND_DOMAINS => {
            let a: RequiredAccount = params(p)?;
            to_value(resend_for(shared, &a.account).await?.domains().await?)
        }
        m::RESEND_API_KEYS => {
            let a: RequiredAccount = params(p)?;
            to_value(resend_for(shared, &a.account).await?.api_keys().await?)
        }
        m::CONFIG_RELOAD => {
            let cfg = crate::config::Config::load(&shared.paths.config_file())?;
            let n = shared.apply_config(cfg).await?;
            Ok(json!({ "accounts": n }))
        }
        m::SYNC_BACKFILL => {
            let b: BackfillParams = params(p)?;
            to_value(json!({ "accounts": backfill(shared, b).await? }))
        }
        m::DAEMON_INFO => to_value(info(shared).await?),
        m::DAEMON_PAUSE => {
            shared.set_paused(true).await?;
            to_value(info(shared).await?)
        }
        m::DAEMON_RESUME => {
            shared.set_paused(false).await?;
            to_value(info(shared).await?)
        }
        m::DAEMON_QUIT => {
            // The marker first: whoever sees the socket go away must find it.
            crate::fsutil::write_private(&shared.paths.stopped_marker(), b"stopped on purpose (lbmail quit / daemon.quit); clients do not start the daemon while this file exists\n")?;
            eprintln!("lumbridge-maild: quit asked ({}); stopping", if conn.is_person() { "person" } else { "agent" });
            shared.quit.notify_one();
            Ok(json!({}))
        }
        m::ACCOUNTS_PURGE => to_value(purge(shared, conn, params(p)?).await?),
        // The Postgres mirror (not in core's method list yet: see the PR's asks of core).
        crate::mirror::METHOD_STATUS => to_value(crate::mirror::status(shared).await?),
        crate::mirror::METHOD_BACKFILL => crate::mirror::backfill(shared, params(p)?).await,
        // Embeddings, parsed facts, semantic search (crate::pgwork).
        crate::pgwork::METHOD_STATUS => to_value(crate::pgwork::status(shared).await?),
        crate::pgwork::METHOD_REQUEUE => crate::pgwork::requeue(shared, params(p)?).await,
        crate::pgwork::METHOD_PARSED => to_value(crate::pgwork::parsed(shared, params(p)?).await?),
        crate::pgwork::METHOD_SEMANTIC => to_value(semantic(shared, conn, params(p)?).await?),
        other => Err(Error::Unsupported(format!("no method {other:?}"))),
    }
}

/// What an account can do. A read-only account can do nothing on the server, whatever its provider.
fn capabilities(rt: &crate::engine::AccountRt) -> Capabilities {
    let c = rt.provider.capabilities();
    if !rt.readonly() {
        return c;
    }
    let no = Support::Unsupported;
    Capabilities { send: false, drafts: no, seen: no, flagged: no, archive: no, trash: no, moves: no, ..c }
}

/// A Resend client for `resend.*` methods, built from the account's config (its own rate limiter).
async fn resend_for(shared: &Arc<Shared>, id: &AccountId) -> Result<lumbridge_mail_resend::ResendProvider> {
    let rt = shared.rt(id)?;
    crate::engine::build_resend(&rt.config).await
}

fn hello(shared: &Arc<Shared>, conn: &Conn, p: HelloParams) -> Result<Value> {
    if let Some(t) = &p.person_token {
        if !same_secret(t.trim(), &shared.person_token) {
            return Err(Error::Refused("that is not this daemon's person token".into()));
        }
        conn.person.store(true, Ordering::SeqCst);
    }
    to_value(Hello { protocol: PROTOCOL_VERSION, version: shared.version.clone(), person: conn.is_person() })
}

/// `accounts.add`: a person adds an account. The password goes to the keyring first (so the account
/// never starts without it), then the entry to config.toml, then the daemon reloads.
async fn add_account(shared: &Arc<Shared>, conn: &Conn, n: NewAccount) -> Result<Account> {
    if !conn.is_person() {
        return Err(Error::Refused("adding an account needs a person (the app or lbmail at a terminal)".into()));
    }
    let file = shared.paths.config_file();
    let current = crate::config::Config::load(&file)?;
    let taken: Vec<String> = current.accounts.iter().map(|a| a.id.clone()).collect();
    let cfg = crate::setup::account_config(&n, &taken)?;
    let password = n.password.filter(|p| !p.is_empty());
    let resend = matches!(cfg.provider, crate::config::ProviderConfig::Resend { .. });
    if password.is_some() && !resend && !matches!(cfg.provider, crate::config::ProviderConfig::Imap { .. }) {
        return Err(Error::Invalid("only IMAP and Resend accounts take a password or API key (gog keeps its own sign-in)".into()));
    }
    let store = shared.secrets.clone();
    let (id, c2) = (cfg.id.clone(), cfg.clone());
    tokio::task::spawn_blocking(move || -> Result<()> {
        if let Some(pw) = &password {
            if resend { store.set_resend(&id, pw)? } else { store.set(&id, pw)? }
        }
        if let Err(e) = crate::config::edit::add_account(&file, &c2) {
            if password.is_some() {
                let _ = if resend { store.delete_resend(&id) } else { store.delete(&id) };
            }
            return Err(e);
        }
        Ok(())
    })
    .await
    .map_err(|e| Error::Other(format!("account task: {e}")))??;
    let reloaded = crate::config::Config::load(&shared.paths.config_file())?;
    shared.apply_config(reloaded).await?;
    Ok(cfg.account())
}

pub fn accounts(shared: &Arc<Shared>) -> Vec<Account> {
    shared.rts().into_iter().map(|rt| rt.account.clone()).collect()
}

fn sync_now(shared: &Arc<Shared>, account: Option<&AccountId>) -> Result<()> {
    let rts = match account {
        Some(id) => vec![shared.rt(id)?],
        None => shared.rts(),
    };
    for rt in rts {
        rt.sync_now.notify_one();
        rt.queue_wake.notify_one();
    }
    Ok(())
}

async fn threads(shared: &Arc<Shared>, conn: &Conn, q: Query) -> Result<ThreadPage> {
    let mut page = shared.db(move |s| s.list_threads(&q)).await?;
    if !conn.is_person() {
        page.threads = page.threads.into_iter().map(redact::summary_for_agent).collect();
    }
    Ok(page)
}

/// `search.semantic`: Postgres (vectors fused with full text) when it can, else the local store's
/// full-text search, saying why.
async fn semantic(shared: &Arc<Shared>, conn: &Conn, p: crate::pgwork::search::SemanticParams) -> Result<crate::pgwork::search::SemanticResult> {
    match crate::pgwork::search::semantic(shared, &p).await? {
        Ok(mut r) => {
            let page = semantic_threads(shared, &mut r.hits).await?;
            r.threads = Some(if conn.is_person() {
                page
            } else {
                ThreadPage { threads: page.threads.into_iter().map(redact::summary_for_agent).collect(), next: None }
            });
            Ok(r)
        }
        Err(note) => {
            let q = Query { accounts: p.accounts.clone(), text: Some(p.q.clone()), limit: p.limit.unwrap_or(20), ..Query::default() };
            let page = threads(shared, conn, q).await?;
            Ok(crate::pgwork::search::SemanticResult { mode: "local".into(), vector: false, note: Some(note), hits: Vec::new(), threads: Some(page) })
        }
    }
}

/// The local store's threads for Postgres hits, best hit first, each thread once. A hit is placed by
/// its message, not by the thread id Postgres has: the store may have grouped it into another thread
/// since (`Store::rethread`, or headers learnt from a body) and the mirror not caught up yet; the hit's
/// `thread_id` is corrected to the store's. Hits whose message is no longer stored are left out of
/// `threads` (they stay in `hits`).
async fn semantic_threads(shared: &Arc<Shared>, hits: &mut [lumbridge_mail_pg::work::SearchRow]) -> Result<ThreadPage> {
    let wanted: Vec<(AccountId, MessageId)> = hits.iter().map(|h| (AccountId::new(&h.account), MessageId::new(&h.message_id))).collect();
    let (threads, placed) = shared
        .db(move |s| {
            let mut seen = std::collections::HashSet::new();
            let mut threads = Vec::new();
            let mut placed = Vec::with_capacity(wanted.len());
            for (a, m) in &wanted {
                let Some(meta) = s.message(a, m)? else {
                    placed.push(None);
                    continue;
                };
                placed.push(Some(meta.thread.clone()));
                if seen.insert((a.clone(), meta.thread.clone()))
                    && let Some(t) = s.thread_summary(a, &meta.thread)?
                {
                    threads.push(t);
                }
            }
            Ok((threads, placed))
        })
        .await?;
    for (h, t) in hits.iter_mut().zip(placed) {
        if let Some(t) = t {
            h.thread_id = t.0;
        }
    }
    Ok(ThreadPage { threads, next: None })
}

async fn thread(shared: &Arc<Shared>, conn: &Conn, r: ThreadRef) -> Result<Vec<MessageMeta>> {
    shared.rt(&r.account)?;
    let msgs = shared.db(move |s| s.thread(&r.account, &r.thread)).await?;
    Ok(if conn.is_person() { msgs } else { msgs.into_iter().map(redact::meta_for_agent).collect() })
}

async fn agent_triage(shared: &Arc<Shared>, r: &ThreadRef, mut t: Triage) -> Triage {
    if t.category != Category::Sensitive
        && let Ok(Some(s)) = {
            let (a, th) = (r.account.clone(), r.thread.clone());
            shared.db(move |s| s.thread_summary(&a, &th)).await
        }
        && let Some(line) = redact::sensitive_line_for_summary(&s)
    {
        t.summary = line;
    }
    t
}

/// The cached body, or the provider's (then cached). One provider call may bring more (gog: the whole
/// thread): the other bodies are cached too (when not cached yet) and the full records stored, so
/// opening the rest of the thread, or triaging it, costs nothing more.
pub async fn fetch_body(shared: &Arc<Shared>, account: &AccountId, message: &MessageId) -> Result<Body> {
    fetch_body_as(shared, account, message, true).await
}

/// [`fetch_body`] without marking the account busy: the body fill's own fetches.
pub(crate) async fn fetch_body_quietly(shared: &Arc<Shared>, account: &AccountId, message: &MessageId) -> Result<Body> {
    fetch_body_as(shared, account, message, false).await
}

async fn fetch_body_as(shared: &Arc<Shared>, account: &AccountId, message: &MessageId, foreground: bool) -> Result<Body> {
    let (a, m) = (account.clone(), message.clone());
    if let Some(b) = shared.db(move |s| s.body(&a, &m)).await? {
        return Ok(b);
    }
    let rt = shared.rt(account)?;
    let (a, m) = (account.clone(), message.clone());
    let Some(meta) = shared.db(move |s| s.message(&a, &m)).await? else {
        return Err(Error::NotFound(format!("message {message}")));
    };
    let bundle = {
        // Someone is waiting for this one: the background body fill keeps off the provider meanwhile.
        let _busy = foreground.then(|| rt.foreground());
        rt.provider.fetch_body_bundle(message, Some(&meta.thread)).await?
    };
    let body = bundle.body.clone();
    let a = account.clone();
    let m = message.clone();
    let by_store = crate::engine::threads_by_store(&rt);
    let changed = shared
        .db(move |s| {
            let mut metas: Vec<MessageMeta> = bundle.metas.into_iter().filter(|x| x.account == a).collect();
            if by_store {
                s.assign_threads(&a, &mut metas)?;
            }
            let applied = if metas.is_empty() { None } else { Some(s.merge_metas(&a, &metas)?) };
            // The body's Message-ID / In-Reply-To / References place a message the listing could not.
            let mut moved = if by_store { s.learn_thread_headers(&a, &m, &bundle.body.headers)? } else { Vec::new() };
            s.put_body(&a, &m, &bundle.body)?;
            for (id, b) in &bundle.others {
                // Only messages we store, and never over a body already cached.
                if s.message(&a, id)?.is_some() && s.body(&a, id)?.is_none() {
                    s.put_body(&a, id, b)?;
                }
            }
            let mut changed = applied.map(|x| x.changed_threads).unwrap_or_default();
            changed.append(&mut moved);
            changed.sort_by(|x, y| x.0.cmp(&y.0));
            changed.dedup();
            Ok(changed)
        })
        .await?;
    if !changed.is_empty() {
        shared.emit(Event::Changed { account: account.clone(), threads: changed });
    }
    Ok(body)
}

/// A body for background use: cached, or fetched if this account's triage body budget has room now.
async fn cheap_body(shared: &Arc<Shared>, account: &AccountId, message: &MessageId) -> Option<Body> {
    let (a, m) = (account.clone(), message.clone());
    if let Ok(Some(b)) = shared.db(move |s| s.body(&a, &m)).await {
        return Some(b);
    }
    let rt = shared.rt(account).ok()?;
    if shared.is_paused() || !shared.take_body_token(&rt) {
        return None;
    }
    fetch_body(shared, account, message).await.ok()
}

async fn info(shared: &Arc<Shared>) -> Result<DaemonInfo> {
    let kept = shared.db(|s| s.kept_accounts()).await?;
    Ok(DaemonInfo { version: shared.version.clone(), paused: shared.is_paused(), load: shared.loads(), kept_accounts: kept })
}

async fn backfill(shared: &Arc<Shared>, b: BackfillParams) -> Result<u32> {
    let rts = match &b.account {
        Some(id) => vec![shared.rt(id)?],
        None => shared.rts(),
    };
    let mut n = 0;
    for rt in rts {
        match rt.provider.request_backfill(b.since).await {
            Ok(()) => {
                n += 1;
                rt.sync_now.notify_one();
            }
            Err(e) if b.account.is_some() => return Err(e),
            Err(_) => {}
        }
    }
    Ok(n)
}

#[derive(Deserialize)]
struct PurgeParams {
    account: AccountId,
}

async fn purge(shared: &Arc<Shared>, conn: &Conn, p: PurgeParams) -> Result<Value> {
    if !conn.is_person() {
        return Err(Error::Refused("purging an account's mail needs a person (lbmail at a terminal)".into()));
    }
    if shared.rt(&p.account).is_ok() {
        return Err(Error::Invalid(format!("account {} is in the config: remove it first (lbmail account remove, or edit config.toml and reload)", p.account)));
    }
    let id = p.account.clone();
    let purged = shared
        .db(move |s| {
            if !s.kept_accounts()?.contains(&id) {
                return Ok(false);
            }
            s.remove_account(&id)
        })
        .await?;
    if !purged {
        return Err(Error::NotFound(format!("no kept mail for account {}", p.account)));
    }
    Ok(json!({ "purged": true }))
}

async fn refuse_if_sensitive_for_agent(shared: &Arc<Shared>, conn: &Conn, account: &AccountId, message: &MessageId, body: Option<&Body>) -> Result<()> {
    if conn.is_person() {
        return Ok(());
    }
    let (a, m) = (account.clone(), message.clone());
    let Some(meta) = shared.db(move |s| s.message(&a, &m)).await? else { return Ok(()) };
    let sensitive = match body {
        Some(b) => redact::sensitive_line_with_body(&meta, b),
        None => redact::sensitive_line_for_meta(&meta),
    };
    match sensitive {
        Some(line) => Err(Error::Refused(format!("{line}: sensitive mail is shown only to a person"))),
        None => Ok(()),
    }
}

async fn body(shared: &Arc<Shared>, conn: &Conn, r: MessageRef) -> Result<Body> {
    refuse_if_sensitive_for_agent(shared, conn, &r.account, &r.message, None).await?;
    let b = fetch_body(shared, &r.account, &r.message).await?;
    refuse_if_sensitive_for_agent(shared, conn, &r.account, &r.message, Some(&b)).await?;
    Ok(b)
}

async fn attachment(shared: &Arc<Shared>, conn: &Conn, r: AttachmentRef) -> Result<PathResult> {
    Ok(PathResult { path: cached_attachment(shared, conn, &r).await?.0 })
}

/// The attachment in the daemon's cache (fetched from the provider the first time), and what the
/// message says about it.
async fn cached_attachment(shared: &Arc<Shared>, conn: &Conn, r: &AttachmentRef) -> Result<(String, Attachment)> {
    let rt = shared.rt(&r.account)?;
    let b = fetch_body(shared, &r.account, &r.message).await?;
    refuse_if_sensitive_for_agent(shared, conn, &r.account, &r.message, Some(&b)).await?;
    let info = b.attachments.iter().find(|x| x.id == r.attachment).cloned().ok_or_else(|| Error::NotFound(format!("attachment {}", r.attachment)))?;
    let (a, m, at) = (r.account.clone(), r.message.clone(), r.attachment.clone());
    if let Some(p) = shared.db(move |s| s.attachment_path(&a, &m, &at)).await?
        && std::path::Path::new(&p).is_file()
    {
        return Ok((p, info));
    }
    let bytes = {
        let _busy = rt.foreground();
        rt.provider.fetch_attachment(&r.message, &r.attachment).await?
    };
    let dir = shared.paths.attachments_dir().join(crate::fsutil::hashed(&[r.account.as_str(), r.message.as_str()]));
    let name = info.filename.clone();
    let path = tokio::task::spawn_blocking(move || -> Result<String> {
        crate::fsutil::private_dir(&dir)?;
        let path = crate::fsutil::unique_path(&dir, &crate::fsutil::safe_file_name(&name));
        crate::fsutil::write_private(&path, &bytes)?;
        Ok(path.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| Error::Other(format!("attachment task: {e}")))??;
    let (a, m, at, p) = (r.account.clone(), r.message.clone(), r.attachment.clone(), path.clone());
    shared.db(move |s| s.set_attachment_path(&a, &m, &at, &p)).await?;
    Ok((path, info))
}

/// `message.attachment.read`: one piece of a cached attachment, for a client on another machine. The
/// daemon reads at most `ATTACHMENT_CHUNK_MAX` bytes per call, so neither side holds the whole file in
/// a message.
async fn attachment_read(shared: &Arc<Shared>, conn: &Conn, r: AttachmentRead) -> Result<AttachmentChunk> {
    let aref = AttachmentRef { account: r.account.clone(), message: r.message.clone(), attachment: r.attachment.clone() };
    let (path, info) = cached_attachment(shared, conn, &aref).await?;
    let max = match r.max {
        0 => ATTACHMENT_CHUNK_MAX,
        n => n.min(ATTACHMENT_CHUNK_MAX),
    };
    let offset = r.offset;
    let (size, bytes) = tokio::task::spawn_blocking(move || -> Result<(u64, Vec<u8>)> {
        use std::io::{Read, Seek, SeekFrom};
        let io = |e: std::io::Error| Error::Other(format!("attachment cache: {e}"));
        let mut f = std::fs::File::open(&path).map_err(io)?;
        let size = f.metadata().map_err(io)?.len();
        if offset > size {
            return Err(Error::Invalid(format!("offset {offset} is past the end ({size} bytes)")));
        }
        f.seek(SeekFrom::Start(offset)).map_err(io)?;
        let mut buf = Vec::with_capacity((max as u64).min(size - offset) as usize);
        f.take(u64::from(max)).read_to_end(&mut buf).map_err(io)?;
        Ok((size, buf))
    })
    .await
    .map_err(|e| Error::Other(format!("attachment task: {e}")))??;
    use base64::Engine as _;
    let eof = offset + bytes.len() as u64 >= size;
    Ok(AttachmentChunk {
        filename: info.filename,
        mime: info.mime,
        size,
        offset,
        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
        eof,
    })
}

async fn act(shared: &Arc<Shared>, conn: &Conn, a: Act) -> Result<i64> {
    let rt = shared.rt(&a.account)?;
    if rt.readonly() {
        return Err(Error::Refused(format!("account {} is read-only: nothing is changed on the server", a.account)));
    }
    // The caller field is self-declared; only a connection that showed the person token may use it.
    let caller = if conn.is_person() { a.caller } else { Caller::Agent };
    if matches!(a.action, Action::Send { .. }) && caller != Caller::Person {
        return Err(Error::Refused(safety::SEND_NEEDS_A_PERSON.into()));
    }
    // Agents read, ask and draft; changing mail is a person's unless `[agents] actions = true`.
    let agents_may_act = shared.config.read().unwrap_or_else(|e| e.into_inner()).agents.actions;
    if caller != Caller::Person && !agents_may_act && !matches!(a.action, Action::SaveDraft { .. }) {
        return Err(Error::Refused(format!(
            "{} needs a person: agents may read, ask and draft ([agents] actions = false in the daemon's config)",
            action_name(&a.action)
        )));
    }
    // What the account cannot do at all is refused here, before it changes anything locally.
    if rt.provider.capabilities().support_for(&a.action) == Support::Unsupported {
        return Err(Error::Unsupported(format!("account {} cannot do this ({})", a.account, action_name(&a.action))));
    }
    let account = a.account.clone();
    let q = shared.db(move |s| s.enqueue(&account, a.action, caller)).await?;
    if !q.changed_threads.is_empty() {
        shared.emit(Event::Changed { account: rt.account.id.clone(), threads: q.changed_threads });
    }
    rt.queue_wake.notify_one();
    let status = shared.status_of(&rt).await;
    shared.emit(Event::Status { status });
    Ok(q.id)
}

use lumbridge_mail_core::safety;

fn action_name(a: &Action) -> &'static str {
    match a {
        Action::SetSeen { .. } => "mark read",
        Action::SetFlagged { .. } => "star",
        Action::Move { .. } => "move",
        Action::Archive { .. } => "archive",
        Action::Trash { .. } => "trash",
        Action::SaveDraft { .. } => "save a draft",
        Action::DeleteDraft { .. } => "delete a draft",
        Action::Send { .. } => "send",
    }
}

fn need_ai(shared: &Arc<Shared>) -> Result<Arc<lumbridge_mail_ai::MailAi>> {
    shared.ai().ok_or_else(|| Error::Unsupported("AI is off in the config".into()))
}

async fn summarise(shared: &Arc<Shared>, r: ThreadRef) -> Result<String> {
    let rt = shared.rt(&r.account)?;
    let thread = load_thread(shared, &r, true).await?;
    if let Some(line) = lumbridge_mail_ai::sensitive_line(&thread) {
        return Ok(line);
    }
    let ai = need_ai(shared)?;
    ai.for_account(&rt.account)?.summarise(&thread).await
}

async fn extract(shared: &Arc<Shared>, r: ThreadRef) -> Result<Extracted> {
    let rt = shared.rt(&r.account)?;
    let thread = load_thread(shared, &r, true).await?;
    let ai = need_ai(shared)?;
    ai.for_account(&rt.account)?.extract(&thread).await
}

async fn draft(shared: &Arc<Shared>, r: DraftRequest) -> Result<Draft> {
    let rt = shared.rt(&r.account)?;
    let ai = need_ai(shared)?;
    let acct_ai = ai.for_account(&rt.account)?;
    match &r.thread {
        Some(t) => {
            let tref = ThreadRef { account: r.account.clone(), thread: t.clone() };
            // Sensitive mail never goes to a model, whoever asks: said before any body is fetched, in
            // its one line only (never its content).
            let stored = load_thread(shared, &tref, false).await?;
            if let Some(line) = lumbridge_mail_ai::sensitive_line(&stored) {
                return Err(Error::Refused(format!("{line}: sensitive mail is never given to a model, so there is no AI draft for it")));
            }
            let thread = load_thread(shared, &tref, true).await?;
            if let Some(line) = lumbridge_mail_ai::sensitive_line(&thread) {
                return Err(Error::Refused(format!("{line}: sensitive mail is never given to a model, so there is no AI draft for it")));
            }
            let req = ReplyRequest { instruction: r.instruction.clone(), voice: Vec::new(), reply_all: r.reply_all, extra_cc: r.extra_cc.clone() };
            acct_ai.draft_reply(&thread, &req).await
        }
        None => acct_ai.draft_new(r.extra_cc.clone(), "", &r.instruction, &[]).await,
    }
}

struct StoreSearch {
    shared: Arc<Shared>,
}

#[async_trait::async_trait]
impl Search for StoreSearch {
    async fn search(&self, query: &str, accounts: &[AccountId], limit: u32) -> Result<Vec<SearchHit>> {
        let q = Query { accounts: Some(accounts.to_vec()), text: Some(query.to_owned()), limit: limit.max(1), ..Query::default() };
        let page = self.shared.db(move |s| s.list_threads(&q)).await?;
        let mut hits = Vec::new();
        for t in page.threads {
            let (a, th) = (t.account.clone(), t.id.clone());
            let msgs = self.shared.db(move |s| s.thread(&a, &th)).await?;
            let Some(latest) = msgs.last() else { continue };
            // Cached bodies, a few fetched within the budget; else the snippet.
            let text = match cheap_body(&self.shared, &t.account, &latest.id).await {
                Some(b) => lumbridge_mail_render::plain_text(&b, &lumbridge_mail_render::TextOptions::for_ai()),
                None => latest.snippet.clone(),
            };
            hits.push(SearchHit {
                thread: ThreadRef { account: t.account.clone(), thread: t.id.clone() },
                subject: t.subject.clone(),
                from: latest.from.first().cloned().unwrap_or_else(|| Address::new(None, "")),
                date: latest.date,
                text,
            });
        }
        Ok(hits)
    }
}

async fn ask(shared: &Arc<Shared>, a: AskParams) -> Result<Answer> {
    let ai = need_ai(shared)?;
    let accounts: Vec<Account> = match &a.accounts {
        Some(ids) => ids.iter().map(|id| shared.rt(id).map(|rt| rt.account.clone())).collect::<Result<_>>()?,
        // The anon wall: a question about "my mail" never reads anon accounts unless they are named.
        None => accounts(shared).into_iter().filter(|x| x.persona != Persona::Anon).collect(),
    };
    let search = StoreSearch { shared: shared.clone() };
    ai.ask(&a.question, &accounts, &search).await
}
