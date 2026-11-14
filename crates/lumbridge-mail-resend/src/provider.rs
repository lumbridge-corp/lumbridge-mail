//! `ResendProvider`: the core `Provider` for one Resend account. See the crate README for the endpoints
//! each method calls and what stays local.

use crate::api::{self, ApiKeyInfo, DomainInfo, List, ReceivedEmail, SentEmail};
use crate::client::{Client, Method};
use crate::config::ResendConfig;
use crate::convert::{self, Kind};
use crate::cursor::{Cursor, Run, Source};
use crate::send;
use lumbridge_mail_core::*;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long the domain list (for the From check) is trusted.
const DOMAINS_TTL: Duration = Duration::from_secs(600);

pub struct ResendProvider {
    cfg: ResendConfig,
    client: Client,
    /// The key can only send (`restricted_api_key` on a list): sync reports nothing and stops asking.
    send_only: AtomicBool,
    /// Domains as last listed (None inside: the key may not list them).
    domains: tokio::sync::Mutex<Option<(Instant, Option<Vec<DomainInfo>>)>>,
    /// Scheduled emails this process canceled or changed: re-read at the next sync. A cache, never needed
    /// for correctness.
    recheck: Mutex<HashSet<String>>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn parse<T: DeserializeOwned>(what: &str, v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(|_| Error::Other(format!("Resend: unexpected {what}")))
}

/// One list page of either source, reduced to what paging needs.
struct Item<T> {
    id: String,
    t: i64,
    value: T,
}

impl ResendProvider {
    pub fn new(cfg: ResendConfig) -> Result<Self> {
        let client = Client::new(&cfg)?;
        Ok(Self {
            cfg,
            client,
            send_only: AtomicBool::new(false),
            domains: tokio::sync::Mutex::new(None),
            recheck: Mutex::new(HashSet::new()),
        })
    }

    pub fn config(&self) -> &ResendConfig {
        &self.cfg
    }

    /// The key turned out to be a sending-only key (nothing to list).
    pub fn is_send_only(&self) -> bool {
        self.send_only.load(Ordering::SeqCst)
    }

    /// `GET /domains`: every domain of the team with its verification status.
    pub async fn domains(&self) -> Result<Vec<DomainInfo>> {
        let l: List<DomainInfo> = parse("domain list", self.client.get("/domains").await?)?;
        Ok(l.data)
    }

    /// `GET /api-keys`: names and dates of the team's keys (Resend never returns a key's value).
    pub async fn api_keys(&self) -> Result<Vec<ApiKeyInfo>> {
        let l: List<ApiKeyInfo> = parse("API key list", self.client.get("/api-keys").await?)?;
        Ok(l.data)
    }

    fn page_path(&self, base: &str, after: Option<&str>) -> Result<String> {
        let mut p = format!("{base}?limit={}", self.cfg.page_size.clamp(1, 100));
        if let Some(a) = after {
            if !convert::valid_resend_id(a) {
                return Err(Error::ResyncNeeded("bad paging id in the cursor".into()));
            }
            p.push_str("&after=");
            p.push_str(a);
        }
        Ok(p)
    }

    async fn page<T: DeserializeOwned>(&self, base: &str, after: Option<&str>, created: impl Fn(&T) -> (String, Option<String>)) -> Result<(Vec<Item<T>>, bool)> {
        let path = self.page_path(base, after)?;
        let l: List<T> = parse("list", self.client.get(&path).await?)?;
        let items = l
            .data
            .into_iter()
            .filter_map(|v| {
                let (id, at) = created(&v);
                let t = at.as_deref().and_then(convert::parse_time_ms)?;
                convert::valid_resend_id(&id).then_some(Item { id, t, value: v })
            })
            .collect();
        Ok((items, l.has_more))
    }

    /// Advances one source's run by one page. Returns the new items (newest first) and, for a first
    /// page, every item on it (for the delivery-state window).
    fn step<T>(&self, src: &mut Source, items: Vec<Item<T>>, has_more: bool, full: bool) -> (Vec<Item<T>>, Vec<Item<T>>) {
        let run = src.run.take().unwrap_or_default();
        let first = run.after.is_none();
        if first && let Some(top) = items.iter().map(|i| i.t).max() {
            let ids: Vec<String> = items.iter().filter(|i| i.t == top).map(|i| i.id.clone()).collect();
            src.advance(top, &ids);
        }
        let reached_floor = items.iter().any(|i| !run.is_new(i.t, &i.id));
        let limit = if full { self.cfg.max_full_pages } else { self.cfg.max_new_pages };
        let last = items.last().map(|i| i.id.clone());
        let (new, old): (Vec<Item<T>>, Vec<Item<T>>) = items.into_iter().partition(|i| run.is_new(i.t, &i.id));
        if !reached_floor && has_more && run.pages + 1 < limit.max(1)
            && let Some(last) = last
        {
            src.run = Some(Run { after: Some(last), pages: run.pages + 1, ..run });
        }
        (new, if first { old } else { Vec::new() })
    }

    async fn received_detail(&self, id: &str) -> Result<ReceivedEmail> {
        parse("received email", self.client.get(&format!("/emails/receiving/{id}?html_format=cid")).await?)
    }

    fn mine_received(&self, e: &ReceivedEmail) -> bool {
        e.to.iter().chain(&e.cc).chain(&e.bcc).chain(&e.received_for).filter_map(|a| convert::parse_address(a)).any(|a| self.cfg.is_mine(&a.email))
    }

    fn mine_sent(&self, e: &SentEmail) -> bool {
        e.from.as_deref().and_then(convert::parse_address).is_some_and(|a| self.cfg.is_mine(&a.email))
    }

    async fn sync_sent(&self, c: &mut Cursor, delta: &mut Delta) -> Result<()> {
        let after = c.sent.run.as_ref().and_then(|r| r.after.clone());
        let first = after.is_none();
        let (items, more) = self.page::<SentEmail>("/emails", after.as_deref(), |e| (e.id.clone(), e.created_at.clone())).await?;
        let (new, rest) = self.step(&mut c.sent, items, more, c.full);
        for it in &new {
            if self.mine_sent(&it.value) {
                self.report_sent(c, delta, &it.value);
            }
        }
        if first {
            // The delivery-state window: the newest page. Rows whose state changed are sent again.
            let mut states = std::collections::BTreeMap::new();
            for it in new.iter().chain(&rest) {
                if !self.mine_sent(&it.value) {
                    continue;
                }
                let ev = it.value.last_event.clone().unwrap_or_default();
                if rest.iter().any(|r| r.id == it.id) && c.states.get(&it.id) != Some(&ev) {
                    self.report_sent(c, delta, &it.value);
                }
                states.insert(it.id.clone(), ev);
            }
            c.states = states;
        }
        Ok(())
    }

    fn report_sent(&self, c: &mut Cursor, delta: &mut Delta, e: &SentEmail) {
        let meta = convert::sent_meta(&self.cfg.account, e);
        match convert::delivery_state(e) {
            Some(DeliveryState::Scheduled) => {
                let at = e.scheduled_at.as_deref().and_then(convert::parse_time_ms).unwrap_or(0);
                c.scheduled.insert(e.id.clone(), at);
            }
            _ => {
                c.scheduled.remove(&e.id);
            }
        }
        delta.upserted.retain(|m| m.id != meta.id);
        delta.upserted.push(meta);
    }

    /// Scheduled emails that are due (or that this process canceled) are read again.
    async fn check_scheduled(&self, c: &mut Cursor, delta: &mut Delta) -> Result<()> {
        let now = now_ms();
        let recheck: HashSet<String> = std::mem::take(&mut *self.recheck.lock().unwrap_or_else(|e| e.into_inner()));
        let due: Vec<String> = c
            .scheduled
            .iter()
            .filter(|(id, at)| **at <= now || recheck.contains(*id))
            .map(|(id, _)| id.clone())
            .take(self.cfg.max_scheduled_checks)
            .collect();
        for id in due {
            match self.client.get(&format!("/emails/{id}")).await {
                Ok(v) => {
                    let e: SentEmail = parse("email", v)?;
                    self.report_sent(c, delta, &e);
                }
                Err(Error::NotFound(_)) => {
                    c.scheduled.remove(&id);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn sync_received(&self, c: &mut Cursor, delta: &mut Delta) -> Result<()> {
        let after = c.recv.run.as_ref().and_then(|r| r.after.clone());
        let (items, more) = self.page::<ReceivedEmail>("/emails/receiving", after.as_deref(), |e| (e.id.clone(), e.created_at.clone())).await?;
        let (new, _) = self.step(&mut c.recv, items, more, c.full);
        let mut budget = self.cfg.max_details_per_sync;
        for it in new {
            if !self.mine_received(&it.value) {
                continue;
            }
            let e = if budget > 0 {
                budget -= 1;
                match self.received_detail(&it.id).await {
                    Ok(full) => full,
                    // Over quota when received, or gone: the list row is all there is.
                    Err(Error::NotFound(_) | Error::Unsupported(_)) => it.value,
                    Err(e) => return Err(e),
                }
            } else {
                it.value
            };
            delta.upserted.push(convert::received_meta(&self.cfg.account, &e));
        }
        Ok(())
    }

    async fn sync_inner(&self, cursor: Option<&SyncCursor>) -> Result<Delta> {
        let mut c = match Cursor::read(cursor)? {
            Some(c) => c,
            None => Cursor::full_sync(self.cfg.receiving),
        };
        if !self.cfg.receiving {
            c.recv.run = None;
        }
        let continuing = c.running();
        if !continuing {
            c.sent.start();
            if self.cfg.receiving {
                c.recv.start();
            }
        }
        let mut delta = Delta { mailboxes: Some(convert::mailboxes(&self.cfg.account, self.cfg.receiving)), reset: c.full, ..Delta::default() };
        if c.sent.run.is_some() {
            self.sync_sent(&mut c, &mut delta).await?;
        }
        if c.recv.run.is_some() {
            self.sync_received(&mut c, &mut delta).await?;
        }
        if !continuing && !c.full {
            self.check_scheduled(&mut c, &mut delta).await?;
        }
        delta.more = c.running();
        if !delta.more {
            c.full = false;
        }
        delta.cursor = Some(c.encode());
        Ok(delta)
    }

    /// Resend's domains for the From check; None when this key may not list them.
    async fn sending_domains(&self) -> Result<Option<Vec<DomainInfo>>> {
        let mut cache = self.domains.lock().await;
        if let Some((at, d)) = cache.as_ref()
            && at.elapsed() < DOMAINS_TTL
        {
            return Ok(d.clone());
        }
        let d = match self.domains().await {
            Ok(d) => Some(d),
            Err(Error::Unsupported(_)) => None,
            Err(e) => return Err(e),
        };
        *cache = Some((Instant::now(), d.clone()));
        Ok(d)
    }

    async fn check_from(&self, from: &Address) -> Result<()> {
        if !self.cfg.is_mine(&from.email) {
            return Err(Error::Refused(format!("{} is not one of this account's addresses", from.email)));
        }
        let Some(domains) = self.sending_domains().await? else { return Ok(()) };
        let domain = from.domain().to_owned();
        match domains.iter().find(|d| d.name.eq_ignore_ascii_case(&domain)) {
            Some(d) if d.can_send() => Ok(()),
            Some(d) => Err(Error::Refused(format!("{domain} is not ready to send at Resend (status {})", d.status))),
            None => Err(Error::Refused(format!("{domain} is not a domain of this Resend account"))),
        }
    }

    async fn send(&self, draft: &Draft) -> Result<ApplyOutcome> {
        let key = send::idempotency_key(self.cfg.account.as_str(), &draft.local_id)?;
        let from = Address::new(self.cfg.name.as_deref(), &self.cfg.email);
        self.check_from(&from).await?;
        let mut draft = draft.clone();
        if draft.in_reply_to.is_none()
            && let Some(parent) = &draft.reply_to
            && let Ok((Kind::Received, id)) = convert::split_id(parent)
            && let Ok(p) = self.received_detail(id).await
        {
            let meta = convert::received_meta(&self.cfg.account, &p);
            draft.in_reply_to = meta.rfc_message_id.clone();
            if draft.references.is_empty() {
                draft.references = meta.references.clone();
            }
        }
        let mut files = Vec::with_capacity(draft.attachments.len());
        for a in &draft.attachments {
            let bytes = tokio::fs::read(&a.path).await.map_err(|e| Error::Invalid(format!("attachment {}: {}", a.filename, e.kind())))?;
            files.push(send::Loaded { filename: a.filename.clone(), mime: a.mime.clone(), bytes });
        }
        let body = send::payload(&draft, &from, self.cfg.reply_to.as_deref(), &files)?;
        let created: api::Created = parse("send answer", self.client.call(Method::Post, "/emails", Some(&body), Some(&key)).await?)?;
        if !convert::valid_resend_id(&created.id) {
            return Ok(ApplyOutcome::default());
        }
        Ok(ApplyOutcome::with_id(convert::message_id(Kind::Sent, &created.id).0))
    }

    /// Trash: a scheduled email is canceled (`POST /emails/{id}/cancel`); everything else is local.
    async fn trash(&self, ids: &[MessageId]) -> Result<()> {
        for id in ids {
            let Ok((Kind::Sent, rid)) = convert::split_id(id) else { continue };
            let e: SentEmail = match self.client.get(&format!("/emails/{rid}")).await {
                Ok(v) => parse("email", v)?,
                Err(Error::NotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            if convert::delivery_state(&e) == Some(DeliveryState::Scheduled) {
                self.client.call(Method::Post, &format!("/emails/{rid}/cancel"), None, None).await?;
                self.recheck.lock().unwrap_or_else(|e| e.into_inner()).insert(rid.to_owned());
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Provider for ResendProvider {
    fn account(&self) -> &AccountId {
        &self.cfg.account
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            send: true,
            drafts: Support::Local,
            seen: Support::Local,
            flagged: Support::Local,
            archive: Support::Local,
            trash: Support::Local,
            moves: Support::Local,
            raw: self.cfg.receiving,
            note: Some(
                "Resend keeps no read state, stars, folders or drafts: those stay on this device. \
                 Trash on a scheduled email cancels it at Resend."
                    .into(),
            ),
        }
    }

    async fn sync(&self, cursor: Option<&SyncCursor>) -> Result<Delta> {
        if self.is_send_only() {
            return Ok(Delta {
                mailboxes: Some(convert::mailboxes(&self.cfg.account, false)),
                cursor: cursor.cloned(),
                ..Delta::default()
            });
        }
        match self.sync_inner(cursor).await {
            Err(Error::Unsupported(_)) => {
                // A sending-only key: nothing can be listed. Say so once in the status, then stay quiet.
                self.send_only.store(true, Ordering::SeqCst);
                Ok(Delta {
                    mailboxes: Some(convert::mailboxes(&self.cfg.account, false)),
                    cursor: cursor.cloned(),
                    reset: cursor.is_none(),
                    ..Delta::default()
                })
            }
            other => other,
        }
    }

    async fn fetch_body(&self, id: &MessageId) -> Result<Body> {
        match convert::split_id(id)? {
            (Kind::Received, rid) => Ok(convert::received_body(&self.received_detail(rid).await?)),
            (Kind::Sent, rid) => {
                let e: SentEmail = parse("email", self.client.get(&format!("/emails/{rid}")).await?)?;
                let attachments = match self.client.get(&format!("/emails/{rid}/attachments")).await {
                    Ok(v) => parse::<List<api::AttachmentInfo>>("attachment list", v).map(|l| l.data).unwrap_or_default(),
                    Err(e @ (Error::Auth(_) | Error::Offline(_))) => return Err(e),
                    Err(_) => Vec::new(),
                };
                Ok(convert::sent_body(&e, &attachments))
            }
        }
    }

    async fn fetch_raw(&self, id: &MessageId) -> Result<Vec<u8>> {
        match convert::split_id(id)? {
            (Kind::Received, rid) => {
                let e = self.received_detail(rid).await?;
                let url = e.raw.and_then(|r| r.download_url).ok_or_else(|| Error::Unsupported("Resend has no raw copy of this email".into()))?;
                self.client.download(&url, self.cfg.max_download_bytes).await
            }
            (Kind::Sent, _) => Err(Error::Unsupported("Resend does not keep the raw message of sent mail".into())),
        }
    }

    async fn fetch_attachment(&self, message: &MessageId, attachment: &AttachmentId) -> Result<Vec<u8>> {
        let (kind, rid) = convert::split_id(message)?;
        let aid = attachment.as_str();
        if !convert::valid_resend_id(aid) {
            return Err(Error::NotFound(format!("attachment {aid}")));
        }
        let path = match kind {
            Kind::Received => format!("/emails/receiving/{rid}/attachments/{aid}"),
            Kind::Sent => format!("/emails/{rid}/attachments/{aid}"),
        };
        let a: api::AttachmentInfo = parse("attachment", self.client.get(&path).await?)?;
        if a.size > self.cfg.max_download_bytes {
            return Err(Error::Refused(format!("larger than the {} MB download limit", self.cfg.max_download_bytes >> 20)));
        }
        let url = a.download_url.ok_or_else(|| Error::Other("Resend gave no download link for the attachment".into()))?;
        self.client.download(&url, self.cfg.max_download_bytes).await
    }

    async fn apply(&self, action: &Action) -> Result<ApplyOutcome> {
        match action {
            // Resend keeps none of these: they live in the local store only (capabilities say Local).
            Action::SetSeen { .. } | Action::SetFlagged { .. } | Action::Archive { .. } => Ok(ApplyOutcome::default()),
            Action::SaveDraft { .. } | Action::DeleteDraft { .. } => Ok(ApplyOutcome::default()),
            Action::Move { to, .. } => {
                if matches!(to.as_str(), convert::INBOX | convert::ARCHIVE | convert::TRASH) {
                    Ok(ApplyOutcome::default())
                } else {
                    Err(Error::Unsupported(format!("{to} is a view of Resend's state; mail cannot be moved there")))
                }
            }
            Action::Trash { ids } => {
                self.trash(ids).await?;
                Ok(ApplyOutcome::default())
            }
            Action::Send { draft } => self.send(draft).await,
        }
    }

    async fn wait_for_change(&self, timeout_s: u64) -> Result<()> {
        tokio::time::sleep(Duration::from_secs(timeout_s).min(self.cfg.poll_interval)).await;
        Ok(())
    }
}
