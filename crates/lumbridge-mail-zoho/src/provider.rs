//! `ZohoProvider`: the core `Provider` for one Zoho address. See the crate README for the tool calls
//! each method makes and for the cursor.

use crate::config::ZohoConfig;
use crate::cursor::{ARCHIVE, Backfill, Box_, Cursor, FolderState, Full, Run, Seen};
use crate::mcp::{Retry, ZohoConn};
use crate::parse::{self, Folder, Row};
use lumbridge_mail_core::*;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where `get_mail_html`'s answer keeps the HTML.
const HTML_KEYS: [&str; 4] = ["content", "htmlContent", "html", "data"];
/// Where `get_mail_original`'s answer keeps the message.
const RAW_KEYS: [&str; 5] = ["originalContent", "content", "data", "message", "original"];

/// The row in a `get_mail` answer: the answer itself, a list's first entry, or an object under a key.
fn detail_row(v: &Value) -> Option<Row> {
    parse::row(v)
        .or_else(|| {
            parse::list_of(v, &["data", "messages"]).and_then(|l| l.first().and_then(parse::row))
        })
        .or_else(|| v.as_object().and_then(|m| m.values().find_map(parse::row)))
}

/// Zoho takes at most this many message ids per change.
const IDS_PER_CALL: usize = 250;
/// Backfill starts this many messages above where the full sync stopped in a folder (new mail since
/// then pushes the old mail down the list; overlap is skipped by date).
const BACKFILL_OVERLAP: u32 = 20;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct ZohoProvider {
    cfg: ZohoConfig,
    conn: Arc<ZohoConn>,
    zid: tokio::sync::OnceCell<String>,
    /// The Sent folder's id, once a folder listing has shown it (for the duplicate-send check).
    sent_folder: Mutex<Option<String>>,
    /// Drafts this process has sent (a retry of one of these is not sent again).
    sent: Mutex<HashSet<String>>,
    backfill_req: Mutex<Option<Option<i64>>>,
    last_backfill: Mutex<Option<tokio::time::Instant>>,
    /// Call shapes (tool + the names of its arguments) Zoho refused for good this session (a 400 naming a
    /// parameter it does not take): not sent again until the daemon restarts; the reason is kept.
    refused: Mutex<std::collections::HashMap<String, String>>,
}

/// The parameter a refusal names (`location X` in `mcp::classify`'s words).
fn refused_location(why: &str) -> Option<String> {
    let rest = why.split("location ").nth(1)?;
    let p: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (!p.is_empty()).then_some(p)
}

/// A call's shape: the tool and the names (not values) of its query and body arguments.
fn shape(name: &str, query: Option<&Value>, body: Option<&Value>) -> String {
    let keys = |v: Option<&Value>| {
        let mut k: Vec<&str> = v
            .and_then(Value::as_object)
            .map(|m| m.keys().map(String::as_str).collect())
            .unwrap_or_default();
        k.sort_unstable();
        k.join(",")
    };
    format!("{name}?{}|{}", keys(query), keys(body))
}

fn order(boxes: &[Box_], archive: bool) -> Vec<String> {
    let rank = |b: &Box_| match b.role() {
        MailboxRole::Inbox => 0,
        MailboxRole::Sent => 1,
        MailboxRole::Drafts => 2,
        _ => 3,
    };
    let mut v: Vec<&Box_> = boxes.iter().collect();
    v.sort_by_key(|b| rank(b));
    let mut out: Vec<String> = v.into_iter().map(|b| b.id.clone()).collect();
    if archive {
        out.push(ARCHIVE.into());
    }
    out
}

fn ids_of(ids: &[MessageId]) -> Result<Vec<String>> {
    ids.iter()
        .map(|i| {
            if parse::valid_id(i.as_str()) {
                Ok(i.0.clone())
            } else {
                Err(Error::NotFound(format!("message {i}")))
            }
        })
        .collect()
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn emails(a: &[Address]) -> String {
    a.iter()
        .map(|a| a.email.as_str())
        .filter(|e| !e.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

impl ZohoProvider {
    pub fn new(cfg: ZohoConfig) -> Result<Self> {
        if cfg
            .zoho_account_id
            .as_deref()
            .is_some_and(|z| !parse::valid_id(z))
        {
            return Err(Error::Invalid(
                "zoho_account_id must be Zoho's numeric account id".into(),
            ));
        }
        let conn = ZohoConn::shared(
            &cfg.connection,
            &cfg.limits,
            cfg.timeout,
            cfg.max_response_bytes,
        )?;
        Ok(Self {
            cfg,
            conn,
            zid: tokio::sync::OnceCell::new(),
            sent_folder: Mutex::new(None),
            sent: Mutex::new(HashSet::new()),
            backfill_req: Mutex::new(None),
            last_backfill: Mutex::new(None),
            refused: Mutex::new(std::collections::HashMap::new()),
        })
    }

    pub fn config(&self) -> &ZohoConfig {
        &self.cfg
    }

    /// The connection (shared with every account on the same sign-in).
    pub fn connection(&self) -> &Arc<ZohoConn> {
        &self.conn
    }

    /// Zoho's account id for this address: from the config, or the `list_accounts` entry with it.
    pub async fn zoho_account_id(&self) -> Result<String> {
        self.zid
            .get_or_try_init(|| async {
                if let Some(z) = &self.cfg.zoho_account_id {
                    return Ok(z.clone());
                }
                let accounts = self.conn.accounts().await?;
                accounts
                    .iter()
                    .find(|a| parse::account_addresses(a).contains(&self.cfg.email))
                    .and_then(|a| parse::text(a.get("accountId")))
                    .filter(|z| parse::valid_id(z))
                    .ok_or_else(|| {
                        Error::NotFound(format!(
                            "{} is not one of the mail accounts of this Zoho sign-in ({} found)",
                            self.cfg.email,
                            accounts.len()
                        ))
                    })
            })
            .await
            .cloned()
    }

    async fn tool(
        &self,
        name: &str,
        path: Value,
        query: Option<Value>,
        body: Option<Value>,
        retry: Retry,
    ) -> Result<Value> {
        if let Some(why) = self.refused_reason(name, query.as_ref(), body.as_ref()) {
            return Err(Error::Unsupported(format!(
                "{why} (not asked again until the daemon restarts)"
            )));
        }
        let zid = self.zoho_account_id().await?;
        let mut pv = Map::new();
        pv.insert("accountId".into(), Value::String(zid));
        if let Value::Object(extra) = path {
            pv.extend(extra);
        }
        let mut args = Map::new();
        args.insert("path_variables".into(), Value::Object(pv));
        if let Some(q) = query {
            args.insert("query_params".into(), q);
        }
        if let Some(b) = body {
            args.insert("body".into(), b);
        }
        let sent_keys: Vec<String> = [&args.get("query_params"), &args.get("body")]
            .iter()
            .filter_map(|v| v.and_then(Value::as_object))
            .flat_map(|m| m.keys().cloned())
            .collect();
        let key = shape(
            name,
            args.get("query_params"),
            args.get("body"),
        );
        let out = self.conn.call(name, Value::Object(args), retry).await;
        if let Err(Error::Unsupported(why)) = &out {
            eprintln!(
                "lumbridge-mail-zoho: {}: {why}; not asked again this session",
                self.cfg.account
            );
            // Zoho named the parameter: every call of this tool that carries it is refused; else
            // this shape of call.
            let key = match refused_location(why) {
                Some(p) if sent_keys.contains(&p) => format!("{name}#{p}"),
                _ => key,
            };
            self.refused
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, why.clone());
        }
        out
    }

    fn refused_reason(&self, name: &str, query: Option<&Value>, body: Option<&Value>) -> Option<String> {
        let refused = self.refused.lock().unwrap_or_else(|e| e.into_inner());
        if refused.is_empty() {
            return None;
        }
        if let Some(why) = refused.get(&shape(name, query, body)) {
            return Some(why.clone());
        }
        [query, body]
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
            .flat_map(|m| m.keys())
            .find_map(|k| refused.get(&format!("{name}#{k}")).cloned())
    }

    /// Whether Zoho refused calls like this one for good this session (this shape, or a parameter in it).
    pub fn refused(&self, name: &str, query: Option<&Value>) -> bool {
        self.refused_reason(name, query, None).is_some()
    }

    async fn list_folders(&self) -> Result<Vec<Folder>> {
        let v = self
            .tool("ZohoMail_list_folders", json!({}), None, None, Retry::Safe)
            .await?;
        let f = parse::folders(&v)?;
        if let Some(s) = f.iter().find(|f| f.role() == MailboxRole::Sent) {
            *self.sent_folder.lock().unwrap_or_else(|e| e.into_inner()) = Some(s.id.clone());
        }
        Ok(f)
    }

    /// One `list_mails` page of `folder` (or of archived mail), newest first: from Zoho's
    /// `pageToken` when the last page gave one, else from the 1-based `start`.
    async fn list_page(
        &self,
        folder: &str,
        start: u32,
        token: Option<&str>,
        limit: u32,
    ) -> Result<parse::Page> {
        // Live Zoho refuses `includeArchived` (400 EXTRA_PARAM_FOUND), so the archive is off unless asked
        // for; a cursor saved while it was on still names it, and gets an empty page here.
        if folder == ARCHIVE && !self.cfg.sync_archive {
            return Ok(parse::Page::default());
        }
        let mut q = json!({
            "limit": limit.clamp(1, 200),
            "sortBy": "time",
            "sortOrder": false,
        });
        match token {
            Some(t) => q["pageToken"] = json!(t),
            None => q["start"] = json!(start.max(1)),
        }
        if folder == ARCHIVE {
            q["includeArchived"] = json!(true);
        } else if parse::valid_id(folder) {
            q["folderId"] = json!(folder);
        } else {
            return Err(Error::ResyncNeeded(
                "a folder id in the cursor is not Zoho's".into(),
            ));
        }
        // `fields` is optional: if Zoho refused it once, list without it.
        if !self.cfg.list_fields.trim().is_empty() {
            let mut with = q.clone();
            with["fields"] = json!(self.cfg.list_fields.trim());
            if self
                .refused_reason("ZohoMail_list_mails", Some(&with), None)
                .is_none()
            {
                q = with;
            }
        }
        let fields = q.get("fields").is_some();
        match self
            .tool("ZohoMail_list_mails", json!({}), Some(q.clone()), None, Retry::Safe)
            .await
        {
            Ok(v) => parse::page(&v),
            // The archive view is a feature Zoho may not offer: without it the rest still syncs.
            Err(Error::Unsupported(_)) if folder == ARCHIVE => Ok(parse::Page::default()),
            Err(Error::Unsupported(_)) if fields => {
                if let Some(m) = q.as_object_mut() {
                    m.remove("fields");
                }
                parse::page(
                    &self
                        .tool("ZohoMail_list_mails", json!({}), Some(q), None, Retry::Safe)
                        .await?,
                )
            }
            Err(e) => Err(e),
        }
    }

    /// The first page of `folder`.
    async fn list(&self, folder: &str, start: u32, limit: u32) -> Result<Vec<Row>> {
        Ok(self.list_page(folder, start, None, limit).await?.rows)
    }

    /// `get_mail` for one message (its folder and state now).
    async fn details(&self, id: &str) -> Result<Row> {
        let v = self
            .tool(
                "ZohoMail_get_mail",
                json!({ "messageId": id }),
                (!self.cfg.list_fields.trim().is_empty())
                    .then(|| json!({ "fields": self.cfg.list_fields.trim() })),
                None,
                Retry::Safe,
            )
            .await?;
        detail_row(&v).ok_or_else(|| parse::shape_error("get_mail", &v))
    }

    fn meta(&self, c: &Cursor, row: &Row, folder: &str) -> MessageMeta {
        row.meta(
            &self.cfg.account,
            folder,
            c.role_of(folder) == MailboxRole::Drafts,
        )
    }

    fn mailboxes(&self, folders: &[Folder]) -> Vec<Mailbox> {
        let mut out: Vec<Mailbox> = folders
            .iter()
            .filter(|f| f.synced())
            .map(|f| Mailbox {
                id: MailboxId::new(&f.id),
                account: self.cfg.account.clone(),
                name: f.display(),
                role: f.role(),
                unread: 0,
                total: 0,
            })
            .collect();
        if self.cfg.sync_archive {
            out.push(Mailbox {
                id: MailboxId::new(ARCHIVE),
                account: self.cfg.account.clone(),
                name: "Archive".into(),
                role: MailboxRole::Archive,
                unread: 0,
                total: 0,
            });
        }
        out
    }

    /// Lists the folders into the cursor (and the delta). Folder states of folders gone are dropped.
    async fn refresh_folders(&self, c: &mut Cursor, delta: &mut Delta) -> Result<()> {
        let folders = self.list_folders().await?;
        c.boxes = folders
            .iter()
            .filter(|f| f.synced())
            .map(|f| Box_ {
                id: f.id.clone(),
                role: f.role().as_str().into(),
            })
            .collect();
        let keep: HashSet<&str> = c
            .boxes
            .iter()
            .map(|b| b.id.as_str())
            .chain(std::iter::once(ARCHIVE))
            .collect();
        c.folders.retain(|k, _| keep.contains(k.as_str()));
        delta.mailboxes = Some(self.mailboxes(&folders));
        Ok(())
    }

    fn polled_order(&self, c: &Cursor) -> Vec<String> {
        order(&c.boxes, self.cfg.sync_archive)
    }

    /// One page of the full sync.
    async fn full_step(&self, c: &mut Cursor, delta: &mut Delta) -> Result<()> {
        if c.full.is_none() {
            self.refresh_folders(c, delta).await?;
            c.folders.clear();
            let day_floor = (self.cfg.window_days > 0)
                .then(|| now() - i64::from(self.cfg.window_days) * 86_400);
            c.full = Some(Full {
                run: Run::new(self.polled_order(c), 1),
                day_floor,
                floor: day_floor,
            });
            c.burst = None;
            c.deep = None;
            c.backfill = None;
            c.backfill_next = false;
        }
        let mut full = c.full.take().unwrap_or_default();
        if let Some(folder) = full.run.folder().map(str::to_owned) {
            let max = self.cfg.window_max;
            let want = if max > 0 {
                self.cfg.page_size.min(max - full.run.count.min(max)).max(1)
            } else {
                self.cfg.page_size
            };
            let pg = self
                .list_page(&folder, full.run.start, full.run.token.as_deref(), want)
                .await?;
            full.run.token = pg.next.clone();
            let listed = pg.listed as u32;
            let rows = pg.rows;
            let mut stop = listed < want || rows.first().map(|r| &r.id) == full.run.last.as_ref();
            full.run.last = rows.first().map(|r| r.id.clone());
            full.run.start += listed;
            full.run.pages += 1;
            let mut oldest = None;
            let drafts = c.role_of(&folder) == MailboxRole::Drafts;
            let state = c.folders.entry(folder.clone()).or_default();
            for r in &rows {
                if r.date != 0 && full.day_floor.is_some_and(|f| r.date < f) {
                    stop = true;
                    continue;
                }
                if (state.seen.len() as u32) < self.cfg.poll_size {
                    state.seen.insert(
                        r.id.clone(),
                        Seen {
                            b: r.bits(),
                            t: r.date,
                        },
                    );
                }
                full.run.count += 1;
                oldest = Some(r.date);
                delta
                    .upserted
                    .push(r.meta(&self.cfg.account, &folder, drafts));
            }
            if !stop && max > 0 && full.run.count >= max {
                // Cut short by the count: only mail newer than what was listed here is covered.
                if let Some(o) = oldest {
                    full.floor = Some(full.floor.map_or(o + 1, |f| f.max(o + 1)));
                }
                stop = true;
            }
            if stop {
                c.folders.entry(folder).or_default().listed = full.run.count;
                full.run.next_folder(1);
            }
        }
        delta.reset = true;
        delta.window_floor = full.floor;
        if full.run.done() {
            c.floor = full.floor;
            if self.cfg.backfill_auto && c.floor.is_some() {
                c.backfill = Some(self.new_backfill(c, None));
            }
            delta.more = false;
        } else {
            c.full = Some(full);
            delta.more = true;
        }
        Ok(())
    }

    fn backfill_start(&self, c: &Cursor, folder: &str) -> u32 {
        c.folders
            .get(folder)
            .map_or(0, |s| s.listed)
            .saturating_sub(BACKFILL_OVERLAP)
            + 1
    }

    fn new_backfill(&self, c: &Cursor, since: Option<i64>) -> Backfill {
        let order = self.polled_order(c);
        let first = order
            .first()
            .map(|f| self.backfill_start(c, f))
            .unwrap_or(1);
        Backfill {
            run: Run::new(order, first),
            since,
            before: c.floor.unwrap_or(i64::MAX),
        }
    }

    /// One page of older mail.
    async fn backfill_step(&self, c: &mut Cursor, delta: &mut Delta) -> Result<()> {
        *self.last_backfill.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(tokio::time::Instant::now());
        let Some(mut b) = c.backfill.take() else {
            return Ok(());
        };
        delta.backfill = true;
        if let Some(folder) = b.run.folder().map(str::to_owned) {
            let want = self.cfg.page_size;
            let pg = self
                .list_page(&folder, b.run.start, b.run.token.as_deref(), want)
                .await?;
            b.run.token = pg.next.clone();
            let listed = pg.listed as u32;
            let rows = pg.rows;
            let mut stop = listed < want || rows.first().map(|r| &r.id) == b.run.last.as_ref();
            b.run.last = rows.first().map(|r| r.id.clone());
            b.run.start += listed;
            for r in &rows {
                if r.date >= b.before && r.date != 0 {
                    continue;
                }
                if b.since.is_some_and(|s| r.date < s) {
                    stop = true;
                    continue;
                }
                delta.upserted.push(self.meta(c, r, &folder));
            }
            if stop {
                b.run.next_folder(1);
                if let Some(next) = b.run.folder().map(str::to_owned) {
                    b.run.start = self.backfill_start(c, &next);
                }
            }
        }
        if b.run.done() {
            c.floor = b.since;
        } else {
            c.backfill = Some(b);
        }
        Ok(())
    }

    /// Extra pages of a burst of new mail in one folder.
    async fn burst_step(&self, c: &mut Cursor, delta: &mut Delta) -> Result<()> {
        let Some(mut run) = c.burst.take() else {
            return Ok(());
        };
        let Some(folder) = run.folder().map(str::to_owned) else {
            return Ok(());
        };
        let want = self.cfg.poll_size.max(1);
        let pg = self
            .list_page(&folder, run.start, run.token.as_deref(), want)
            .await?;
        run.token = pg.next.clone();
        let listed = pg.listed as u32;
        let rows = pg.rows;
        let mut stop = listed < want || rows.first().map(|r| &r.id) == run.last.as_ref();
        run.last = rows.first().map(|r| r.id.clone());
        run.start += listed;
        run.pages += 1;
        for r in &rows {
            if run.stop_t.is_some_and(|t| r.date <= t) {
                stop = true;
                continue;
            }
            delta.upserted.push(self.meta(c, r, &folder));
        }
        if !stop && run.pages < self.cfg.max_new_pages {
            c.burst = Some(run);
        }
        Ok(())
    }

    /// One page of a deep pass: older mail's read state and flags, changed elsewhere.
    async fn deep_step(&self, c: &mut Cursor, delta: &mut Delta) -> Result<()> {
        let Some(mut run) = c.deep.take() else {
            return Ok(());
        };
        if let Some(folder) = run.folder().map(str::to_owned) {
            let want = self.cfg.page_size;
            let pg = self
                .list_page(&folder, run.start, run.token.as_deref(), want)
                .await?;
            run.token = pg.next.clone();
            let listed = pg.listed as u32;
            let rows = pg.rows;
            let stop = listed < want || rows.first().map(|r| &r.id) == run.last.as_ref();
            run.last = rows.first().map(|r| r.id.clone());
            run.start += listed;
            run.pages += 1;
            let state = c.folders.entry(folder.clone()).or_default();
            for r in &rows {
                if let Some(s) = state.seen.get_mut(&r.id) {
                    s.b = r.bits();
                }
            }
            for r in &rows {
                delta.upserted.push(self.meta(c, r, &folder));
            }
            if stop || run.pages >= self.cfg.deep_pages.max(1) {
                run.next_folder(1);
            }
        }
        if !run.done() {
            c.deep = Some(run);
        }
        Ok(())
    }

    /// An incremental round: the newest page of each polled folder against what the cursor saw.
    async fn round(&self, c: &mut Cursor, delta: &mut Delta) -> Result<()> {
        c.round += 1;
        let cold = c
            .round
            .is_multiple_of(u64::from(self.cfg.cold_every.max(1)));
        if cold || c.boxes.is_empty() {
            self.refresh_folders(c, delta).await?;
        }
        let polled: Vec<String> = self
            .polled_order(c)
            .into_iter()
            .filter(|f| cold || c.boxes.iter().any(|b| b.id == *f && b.hot()))
            .collect();
        let want = self.cfg.poll_size.max(1);
        let mut found: HashSet<String> = HashSet::new();
        let mut vanished: Vec<String> = Vec::new();
        for folder in &polled {
            let rows = self.list(folder, 1, want).await?;
            let drafts = c.role_of(folder) == MailboxRole::Drafts;
            let state: &mut FolderState = c.folders.entry(folder.clone()).or_default();
            let known_any = rows.iter().any(|r| state.seen.contains_key(&r.id));
            for r in &rows {
                found.insert(r.id.clone());
                if state.seen.get(&r.id).is_none_or(|s| s.b != r.bits()) {
                    delta
                        .upserted
                        .push(r.meta(&self.cfg.account, folder, drafts));
                }
            }
            let full_page = rows.len() as u32 >= want;
            let oldest = rows.last().map(|r| r.date).unwrap_or(i64::MIN);
            let on_page: HashSet<&str> = rows.iter().map(|r| r.id.as_str()).collect();
            for (id, s) in &state.seen {
                if !on_page.contains(id.as_str()) && (!full_page || s.t > oldest) {
                    vanished.push(id.clone());
                }
            }
            if full_page
                && !known_any
                && !state.seen.is_empty()
                && self.cfg.max_new_pages > 0
                && c.burst.is_none()
            {
                let top = state.seen.values().map(|s| s.t).max();
                c.burst = Some(Run {
                    start: want + 1,
                    stop_t: top,
                    ..Run::new(vec![folder.clone()], want + 1)
                });
            }
            state.seen = rows
                .iter()
                .map(|r| {
                    (
                        r.id.clone(),
                        Seen {
                            b: r.bits(),
                            t: r.date,
                        },
                    )
                })
                .collect();
        }
        // Gone from a page and not seen elsewhere this round: moved to a folder not polled now, archived,
        // or deleted. Look a few up.
        vanished.retain(|id| !found.contains(id));
        vanished.sort();
        vanished.dedup();
        for id in vanished.into_iter().take(self.cfg.max_lookups) {
            match self.details(&id).await {
                Ok(row) => {
                    let folder = row.folder.clone().unwrap_or_else(|| ARCHIVE.into());
                    delta.upserted.push(self.meta(c, &row, &folder));
                }
                Err(Error::NotFound(_)) => delta.removed.push(MessageId::new(id)),
                Err(e) => return Err(e),
            }
        }
        if self.cfg.deep_every > 0
            && c.round.is_multiple_of(u64::from(self.cfg.deep_every))
            && c.deep.is_none()
        {
            c.deep = Some(Run::new(self.polled_order(c), 1));
        }
        Ok(())
    }

    fn backfill_due(&self) -> bool {
        self.last_backfill
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none_or(|t| t.elapsed() >= self.cfg.backfill_every)
    }

    async fn sync_inner(&self, cursor: Option<&SyncCursor>) -> Result<Delta> {
        let mut c = Cursor::read(cursor)?.unwrap_or_default();
        let mut delta = Delta::default();
        if cursor.is_none() || c.full.is_some() {
            self.full_step(&mut c, &mut delta).await?;
            delta.cursor = Some(c.encode());
            return Ok(delta);
        }
        if let Some(since) = self
            .backfill_req
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            && c.floor.is_some_and(|f| since.is_none_or(|s| s < f))
        {
            c.backfill = Some(self.new_backfill(&c, since));
        }
        if c.backfill_next {
            c.backfill_next = false;
            self.backfill_step(&mut c, &mut delta).await?;
        } else if c.burst.is_some() {
            self.burst_step(&mut c, &mut delta).await?;
        } else if c.deep.is_some() {
            self.deep_step(&mut c, &mut delta).await?;
        } else {
            self.round(&mut c, &mut delta).await?;
        }
        if !delta.backfill
            && c.burst.is_none()
            && c.deep.is_none()
            && c.backfill.is_some()
            && self.backfill_due()
        {
            c.backfill_next = true;
        }
        delta.more = c.burst.is_some() || c.deep.is_some() || c.backfill_next;
        delta.cursor = Some(c.encode());
        Ok(delta)
    }

    async fn attachment_list(&self, id: &str) -> Result<Vec<Attachment>> {
        match self
            .tool(
                "ZohoMail_list_mail_attachments",
                json!({ "messageId": id }),
                Some(json!({ "includeInline": true })),
                None,
                Retry::Safe,
            )
            .await
        {
            Ok(v) => Ok(parse::attachments(&v)),
            Err(Error::NotFound(_)) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    async fn raw(&self, id: &str) -> Result<Vec<u8>> {
        let v = self
            .tool(
                "ZohoMail_get_mail_original",
                json!({ "messageId": id }),
                None,
                None,
                Retry::Safe,
            )
            .await?;
        parse::document(&v, &RAW_KEYS)
            .map(String::into_bytes)
            .ok_or_else(|| parse::shape_error("get_mail_original", &v))
    }

    async fn change(&self, tool: &str, ids: &[MessageId], extra: Value) -> Result<()> {
        let ids = ids_of(ids)?;
        for chunk in ids.chunks(IDS_PER_CALL) {
            let mut body = json!({ "messageIds": chunk });
            if let (Value::Object(b), Value::Object(e)) = (&mut body, &extra) {
                b.extend(e.clone());
            }
            self.tool(tool, json!({}), None, Some(body), Retry::Safe)
                .await?;
        }
        Ok(())
    }

    async fn sent_folder(&self) -> Result<Option<String>> {
        if let Some(s) = self
            .sent_folder
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Ok(Some(s));
        }
        self.list_folders().await?;
        Ok(self
            .sent_folder
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone())
    }

    /// Has this draft already gone out? (A send that timed out may still have been delivered.)
    async fn already_sent(&self, draft: &Draft) -> Result<bool> {
        if self
            .sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&draft.local_id)
        {
            return Ok(true);
        }
        if draft.subject.trim().is_empty() {
            return Ok(false);
        }
        let Some(folder) = self.sent_folder().await? else {
            return Ok(false);
        };
        let rows = self.list(&folder, 1, 25).await?;
        let cutoff = now() - self.cfg.dedupe_window_s;
        let mut want_to: Vec<String> = draft.to.iter().map(|a| a.email.clone()).collect();
        want_to.sort();
        let want_text: String = norm(&draft.text).chars().take(40).collect();
        Ok(rows.iter().any(|r| {
            let mut to: Vec<String> = r.to.iter().map(|a| a.email.clone()).collect();
            to.sort();
            let text_ok = r.summary.is_empty()
                || want_text.is_empty()
                || norm(&r.summary).starts_with(&want_text);
            r.date >= cutoff && norm(&r.subject) == norm(&draft.subject) && to == want_to && text_ok
        }))
    }

    async fn send(&self, draft: &Draft) -> Result<ApplyOutcome> {
        if draft.to.is_empty() && draft.cc.is_empty() && draft.bcc.is_empty() {
            return Err(Error::Invalid("no recipients".into()));
        }
        if !draft.attachments.is_empty() {
            return Err(Error::Unsupported(
                "Zoho's mail tools cannot upload attachments; send this one from Zoho Mail".into(),
            ));
        }
        if self.already_sent(draft).await? {
            return Ok(ApplyOutcome::default());
        }
        let (content, kind) = match &draft.html {
            Some(h) if !h.trim().is_empty() => (h.clone(), "html"),
            _ => (draft.text.clone(), "plaintext"),
        };
        let app_id = {
            use sha2::Digest;
            let h = sha2::Sha256::digest(
                format!("{}\n{}", self.cfg.account, draft.local_id).as_bytes(),
            );
            h.iter()
                .take(12)
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        let mut body = json!({
            "fromAddress": self.cfg.email,
            "subject": draft.subject,
            "content": content,
            "contentType": kind,
            "applicationMessageId": app_id,
        });
        for (k, list) in [
            ("toAddress", &draft.to),
            ("ccAddress", &draft.cc),
            ("bccAddress", &draft.bcc),
        ] {
            if !list.is_empty() {
                body[k] = json!(emails(list));
            }
        }
        let parent = draft
            .reply_to
            .as_ref()
            .filter(|p| parse::valid_id(p.as_str()));
        let answer = match parent {
            Some(p) => {
                body["includeOriginalContent"] = json!(false);
                self.tool(
                    "ZohoMail_reply_to_mail",
                    json!({ "messageId": p.as_str() }),
                    None,
                    Some(body),
                    Retry::Never,
                )
                .await?
            }
            None => {
                if let Some(irt) = &draft.in_reply_to {
                    body["inReplyTo"] = json!(format!("<{}>", irt.trim_matches(['<', '>'])));
                }
                if !draft.references.is_empty() {
                    let refs: Vec<String> = draft
                        .references
                        .iter()
                        .map(|r| format!("<{}>", r.trim_matches(['<', '>'])))
                        .collect();
                    body["referencesHeader"] = json!(refs.join(" "));
                }
                self.tool(
                    "ZohoMail_send_mail",
                    json!({}),
                    None,
                    Some(body),
                    Retry::Never,
                )
                .await?
            }
        };
        self.sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(draft.local_id.clone());
        Ok(
            match parse::text(answer.get("messageId")).filter(|i| parse::valid_id(i)) {
                Some(id) => ApplyOutcome::with_id(id),
                None => ApplyOutcome::default(),
            },
        )
    }
}

/// One line of `lbmail zoho probe`: what one read-only call gave back, as shape only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeLine {
    pub tool: &'static str,
    /// The call went through (an answer that is not an error).
    pub ok: bool,
    /// Zoho's innermost `status.code`, when there was one.
    pub status: Option<i64>,
    /// Key names, JSON types and list lengths ([`parse::probe_shape`]); never values.
    pub shape: Option<String>,
    /// Whether our parser took the answer: `Ok(what it found, as counts)` or `Err(why, shape only)`.
    pub parser: Option<std::result::Result<String, String>>,
    /// The server's error text (or ours), when the call failed.
    pub error: Option<String>,
    /// `list_folders` only: the `folderType` values seen.
    pub folder_types: Vec<String>,
}

impl std::fmt::Display for ProbeLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = self
            .status
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".into());
        let parser = match &self.parser {
            Some(Ok(w)) => format!("accepted ({w})"),
            Some(Err(w)) => format!("REJECTED ({w})"),
            None => "-".into(),
        };
        writeln!(
            f,
            "{:<32} {:<5} status {status:<4} parser {parser}",
            self.tool,
            if self.ok { "ok" } else { "ERROR" }
        )?;
        if let Some(e) = &self.error {
            writeln!(f, "    error: {e}")?;
        }
        if !self.folder_types.is_empty() {
            writeln!(f, "    folderType values: {}", self.folder_types.join(", "))?;
        }
        if let Some(s) = &self.shape {
            writeln!(f, "    shape: {s}")?;
        }
        Ok(())
    }
}

/// The deepest `status.code` in Zoho's wrappers.
fn deepest_code(v: &Value) -> Option<i64> {
    let mut code = None;
    let mut cur = Some(v);
    for _ in 0..6 {
        let Some(c) = cur else { break };
        if let Some(x) = parse::int(c.get("status").and_then(|s| s.get("code"))) {
            code = Some(x);
        }
        cur = c.get("data");
    }
    code
}

/// Peels `{status, data}` wrappers the way the connection does (for the probe's parser check).
fn peel(v: &Value) -> Value {
    let mut cur = v.clone();
    for _ in 0..6 {
        let Value::Object(m) = &cur else { break };
        let wrapper = m.contains_key("data")
            && (m.contains_key("status")
                || m.contains_key("requestId")
                || m.contains_key("headers")
                || m.len() == 1);
        if !wrapper {
            break;
        }
        cur = m.get("data").cloned().unwrap_or(Value::Null);
    }
    cur
}

impl ZohoProvider {
    async fn probe_call(
        &self,
        tool: &'static str,
        args: Value,
        out: &mut Vec<ProbeLine>,
        check: impl FnOnce(&Value) -> std::result::Result<String, String>,
    ) -> Option<Value> {
        let mut line = ProbeLine {
            tool,
            ok: false,
            status: None,
            shape: None,
            parser: None,
            error: None,
            folder_types: Vec::new(),
        };
        let r = self.conn.call_raw(tool, args).await;
        let v = match r {
            Ok(v) => v,
            Err(e) => {
                line.error = Some(e.to_string());
                out.push(line);
                return None;
            }
        };
        line.ok = true;
        line.status = deepest_code(&v);
        line.shape = Some(parse::probe_shape(&v));
        let inner = peel(&v);
        if tool == "ZohoMail_list_folders"
            && let Ok(f) = parse::folders(&inner)
        {
            let mut t: Vec<String> = f
                .iter()
                .map(|f| f.kind.clone())
                .filter(|k| k.len() <= 20 && k.chars().all(|c| c.is_ascii_alphabetic()))
                .collect();
            t.sort();
            t.dedup();
            line.folder_types = t;
        }
        line.parser = Some(check(&inner));
        out.push(line);
        Some(inner)
    }

    /// `lbmail zoho probe`: read-only calls (`list_accounts`, `list_folders`, one small `list_mails`
    /// page of the Inbox, then `get_mail`, `get_mail_html`, `get_mail_headers`,
    /// `list_mail_attachments` and `get_mail_original` for its newest message), each reported as shape
    /// and parser verdict only. Nothing from mail is printed: no ids, addresses, subjects or bodies.
    pub async fn probe(&self) -> Vec<ProbeLine> {
        let mut out = Vec::new();
        let me = self.cfg.email.clone();
        let mut zid = self.cfg.zoho_account_id.clone();
        let accounts = self
            .probe_call(
                "ZohoMail_list_accounts",
                json!({}),
                &mut out,
                |v| match parse::accounts(v) {
                    Some(list) => {
                        let mine = list
                            .iter()
                            .any(|a| parse::account_addresses(a).contains(&me));
                        Ok(format!(
                            "{} account(s); this account's email {}",
                            list.len(),
                            if mine { "matches one" } else { "matches NONE" }
                        ))
                    }
                    None => Err(format!("no accounts in {}", parse::shape(v))),
                },
            )
            .await;
        if zid.is_none() {
            zid = accounts
                .as_ref()
                .and_then(parse::accounts)
                .and_then(|l| {
                    l.into_iter()
                        .find(|a| parse::account_addresses(a).contains(&me))
                })
                .and_then(|a| parse::text(a.get("accountId")))
                .filter(|z| parse::valid_id(z));
        }
        let Some(zid) = zid else {
            out.push(ProbeLine {
                tool: "(account id)",
                ok: false,
                status: None,
                shape: None,
                parser: None,
                error: Some(
                    "no Zoho account id for this account's email; the rest is skipped".into(),
                ),
                folder_types: Vec::new(),
            });
            return out;
        };
        let pv = |extra: Option<&str>| match extra {
            Some(m) => json!({ "accountId": zid, "messageId": m }),
            None => json!({ "accountId": zid }),
        };
        let folders = self
            .probe_call(
                "ZohoMail_list_folders",
                json!({ "path_variables": pv(None) }),
                &mut out,
                |v| {
                    parse::folders(v)
                        .map(|f| {
                            format!(
                                "{} folder(s), inbox {}",
                                f.len(),
                                if f.iter().any(|f| f.role() == MailboxRole::Inbox) {
                                    "found"
                                } else {
                                    "NOT found"
                                }
                            )
                        })
                        .map_err(|e| e.to_string())
                },
            )
            .await;
        let inbox = folders
            .as_ref()
            .and_then(|v| parse::folders(v).ok())
            .and_then(|f| f.into_iter().find(|f| f.role() == MailboxRole::Inbox))
            .map(|f| f.id);
        let Some(inbox) = inbox else { return out };
        let q = json!({ "folderId": inbox, "start": 1, "limit": 3, "sortBy": "time", "sortOrder": false });
        let listing = self
            .probe_call("ZohoMail_list_mails", json!({ "path_variables": pv(None), "query_params": q }), &mut out, |v| {
                parse::page(v)
                    .map(|p| {
                        let read = p.rows.iter().filter(|r| r.seen.is_some()).count();
                        let flag = p.rows.iter().filter(|r| r.flagged.is_some()).count();
                        let thread = p.rows.iter().filter(|r| r.thread.is_some()).count();
                        let dated = p.rows.iter().filter(|r| r.date > 0).count();
                        format!(
                            "{} of {} row(s) read; with read state {read}, flag {flag}, thread {thread}, date {dated}; next page token {}",
                            p.rows.len(),
                            p.listed,
                            if p.next.is_some() { "yes" } else { "no" }
                        )
                    })
                    .map_err(|e| e.to_string())
            })
            .await;
        let Some(mid) = listing
            .as_ref()
            .and_then(|v| parse::page(v).ok())
            .and_then(|p| p.rows.into_iter().next())
            .map(|r| r.id)
        else {
            return out;
        };
        let m = Some(mid.as_str());
        self.probe_call(
            "ZohoMail_get_mail",
            json!({ "path_variables": pv(m) }),
            &mut out,
            |v| match detail_row(v) {
                Some(r) => Ok(format!(
                    "a row; folder {}, read state {}",
                    if r.folder.is_some() { "yes" } else { "no" },
                    if r.seen.is_some() { "yes" } else { "no" }
                )),
                None => Err(format!("no row in {}", parse::shape(v))),
            },
        )
        .await;
        self.probe_call(
            "ZohoMail_get_mail_html",
            json!({ "path_variables": pv(m) }),
            &mut out,
            |v| match parse::document(v, &HTML_KEYS) {
                Some(h) => Ok(format!("html, {} bytes", h.len())),
                None => Err(format!("no html in {}", parse::shape(v))),
            },
        )
        .await;
        self.probe_call(
            "ZohoMail_get_mail_headers",
            json!({ "path_variables": pv(m), "query_params": { "rawFormat": true } }),
            &mut out,
            |v| {
                let pairs = parse::header_pairs(v);
                if pairs.is_empty() {
                    Err(format!("no headers in {}", parse::shape(v)))
                } else {
                    Ok(format!("{} header(s)", pairs.len()))
                }
            },
        )
        .await;
        self.probe_call(
            "ZohoMail_list_mail_attachments",
            json!({ "path_variables": pv(m), "query_params": { "includeInline": true } }),
            &mut out,
            |v| Ok(format!("{} attachment(s)", parse::attachments(v).len())),
        )
        .await;
        self.probe_call(
            "ZohoMail_get_mail_original",
            json!({ "path_variables": pv(m) }),
            &mut out,
            |v| match parse::document(v, &RAW_KEYS) {
                Some(r) => {
                    let parsed = mail_parser::MessageParser::default()
                        .parse(r.as_bytes())
                        .is_some();
                    Ok(format!(
                        "{} bytes, parses as a message: {}",
                        r.len(),
                        if parsed { "yes" } else { "no" }
                    ))
                }
                None => Err(format!("no message in {}", parse::shape(v))),
            },
        )
        .await;
        out
    }
}

#[async_trait::async_trait]
impl Provider for ZohoProvider {
    fn account(&self) -> &AccountId {
        &self.cfg.account
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            send: true,
            drafts: Support::Local,
            seen: Support::Server,
            flagged: Support::Server,
            archive: Support::Server,
            trash: Support::Server,
            moves: Support::Server,
            raw: true,
            note: Some(
                "Drafts stay on this device (Zoho's mail tools cannot save drafts). A star is Zoho's flag; \
                 removing it has to be done in Zoho Mail. Mail with attachments has to be sent from Zoho Mail."
                    .into(),
            ),
        }
    }

    async fn sync(&self, cursor: Option<&SyncCursor>) -> Result<Delta> {
        self.sync_inner(cursor).await
    }

    async fn fetch_body(&self, id: &MessageId) -> Result<Body> {
        let mid = ids_of(std::slice::from_ref(id))?.remove(0);
        let html = self
            .tool(
                "ZohoMail_get_mail_html",
                json!({ "messageId": mid }),
                None,
                None,
                Retry::Safe,
            )
            .await
            .map(|v| parse::document(&v, &HTML_KEYS))?;
        let headers = match self
            .tool(
                "ZohoMail_get_mail_headers",
                json!({ "messageId": mid }),
                Some(json!({ "rawFormat": true })),
                None,
                Retry::Safe,
            )
            .await
        {
            Ok(v) => parse::body_headers(&parse::header_pairs(&v)),
            Err(e @ (Error::Offline(_) | Error::Auth(_))) => return Err(e),
            Err(_) => Vec::new(),
        };
        let attachments = self.attachment_list(&mid).await?;
        Ok(Body {
            text: None,
            html: html.filter(|h| !h.trim().is_empty()),
            attachments,
            headers,
        })
    }

    async fn fetch_raw(&self, id: &MessageId) -> Result<Vec<u8>> {
        let mid = ids_of(std::slice::from_ref(id))?.remove(0);
        self.raw(&mid).await
    }

    /// Zoho's mail tools have no attachment download, so the attachment is cut out of the raw message
    /// (`get_mail_original`), matched by content id, else by file name and position.
    async fn fetch_attachment(
        &self,
        message: &MessageId,
        attachment: &AttachmentId,
    ) -> Result<Vec<u8>> {
        let mid = ids_of(std::slice::from_ref(message))?.remove(0);
        let list = self.attachment_list(&mid).await?;
        let pos = list
            .iter()
            .position(|a| a.id == *attachment)
            .ok_or_else(|| Error::NotFound(format!("attachment {attachment}")))?;
        let want = &list[pos];
        let nth_same_name = list[..pos]
            .iter()
            .filter(|a| a.filename == want.filename)
            .count();
        let raw = self.raw(&mid).await?;
        let msg = mail_parser::MessageParser::default()
            .parse(&raw)
            .ok_or_else(|| Error::Other("Zoho: the message could not be parsed".into()))?;
        let parts: Vec<&mail_parser::MessagePart<'_>> = msg.attachments().collect();
        use mail_parser::MimeHeaders;
        let by_cid = want.content_id.as_deref().and_then(|cid| {
            parts.iter().find(|p| {
                p.content_id()
                    .is_some_and(|c| c.trim_matches(['<', '>']) == cid)
            })
        });
        let part = by_cid.or_else(|| {
            parts
                .iter()
                .filter(|p| {
                    p.attachment_name().map(parse::sanitise_filename).as_deref()
                        == Some(want.filename.as_str())
                })
                .nth(nth_same_name)
        });
        part.map(|p| p.contents().to_vec())
            .ok_or_else(|| Error::NotFound(format!("attachment {} in the message", want.filename)))
    }

    async fn apply(&self, action: &Action) -> Result<ApplyOutcome> {
        match action {
            Action::SetSeen { ids, seen } => {
                let tool = if *seen {
                    "ZohoMail_mark_mails_as_read"
                } else {
                    "ZohoMail_mark_mails_as_unread"
                };
                self.change(tool, ids, json!({})).await?;
            }
            Action::SetFlagged { ids, flagged: true } => {
                self.change(
                    "ZohoMail_apply_mail_flag",
                    ids,
                    json!({ "flagId": self.cfg.flag as i64 }),
                )
                .await?
            }
            Action::SetFlagged { flagged: false, .. } => {
                return Err(Error::Unsupported(
                    "Zoho's mail tools cannot clear a flag; clear it in Zoho Mail".into(),
                ));
            }
            Action::Archive { ids } => {
                self.change("ZohoMail_archive_mails", ids, json!({}))
                    .await?
            }
            Action::Trash { ids } => {
                self.change("ZohoMail_move_mails_to_trash", ids, json!({}))
                    .await?
            }
            Action::Move { ids, from, to } => {
                if to.as_str() == ARCHIVE {
                    self.change("ZohoMail_archive_mails", ids, json!({}))
                        .await?;
                } else {
                    if !parse::valid_id(to.as_str()) {
                        return Err(Error::NotFound(format!("folder {to}")));
                    }
                    if from.as_ref().is_some_and(|f| f.as_str() == ARCHIVE) {
                        self.change("ZohoMail_unarchive_mails", ids, json!({}))
                            .await?;
                    }
                    self.change(
                        "ZohoMail_move_mails",
                        ids,
                        json!({ "destinationFolderId": to.as_str() }),
                    )
                    .await?;
                }
            }
            // Zoho's mail tools have no draft calls: drafts live in the local store (capabilities: Local).
            Action::SaveDraft { .. } | Action::DeleteDraft { .. } => {}
            Action::Send { draft } => return self.send(draft).await,
        }
        Ok(ApplyOutcome::default())
    }

    async fn wait_for_change(&self, timeout_s: u64) -> Result<()> {
        tokio::time::sleep(Duration::from_secs(timeout_s).min(self.cfg.poll_interval)).await;
        Ok(())
    }

    async fn request_backfill(&self, since: Option<i64>) -> Result<()> {
        *self.backfill_req.lock().unwrap_or_else(|e| e.into_inner()) = Some(since);
        Ok(())
    }

    fn load(&self) -> Option<ProviderLoad> {
        Some(self.conn.load())
    }
}

/// For tests: the per-folder state the cursor keeps (message ids of the polled page).
#[doc(hidden)]
pub fn cursor_page_ids(c: &SyncCursor) -> BTreeMap<String, Vec<String>> {
    Cursor::read(Some(c))
        .ok()
        .flatten()
        .map(|c| {
            c.folders
                .into_iter()
                .map(|(k, v)| (k, v.seen.into_keys().collect()))
                .collect()
        })
        .unwrap_or_default()
}
