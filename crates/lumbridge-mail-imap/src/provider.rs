//! `ImapProvider`: the `Provider` trait over IMAP + SMTP.

use crate::config::{Credentials, ImapAccount};
use crate::cursor::{CursorState, FlagSnapshot, FolderCursor};
use crate::folders::{Folder, sync_rank};
use crate::ids::{self, MessageKey, parse_message_id};
use crate::parse;
use crate::send;
use crate::session::{Conn, FolderStatus};
use crate::uidset::{UidSet, chunks};
use lumbridge_mail_core::{
    AccountId, Action, ApplyOutcome, AttachmentId, Body, Delta, Draft, Error, Flags, Mailbox, MailboxId, MailboxRole, MessageId,
    MessageMeta, Provider, Result, SyncCursor,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;
use std::time::Duration;
use tokio::sync::Mutex;

/// Fetch commands carry at most this many UIDs, so command lines stay short.
const FETCH_CHUNK: usize = 200;
/// When a burst of new mail arrives, at least this many are taken per sync call even if the page is full.
const MIN_NEW_PER_CALL: usize = 50;
/// The longest one IDLE runs before we re-issue it (servers drop IDLE after 30 minutes, RFC 2177).
const MAX_IDLE: Duration = Duration::from_secs(25 * 60);

pub struct ImapProvider {
    account: ImapAccount,
    creds: RwLock<(Credentials, Option<Credentials>)>,
    main: Mutex<Option<Conn>>,
    idle: Mutex<Option<Conn>>,
}

impl std::fmt::Debug for ImapProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapProvider").field("account", &self.account.id).field("imap", &self.account.imap.host).finish()
    }
}

/// Keeps the connection unless the error says it is broken.
fn keep_or_drop<T>(slot: &mut Option<Conn>, r: Result<T>) -> Result<T> {
    if matches!(r, Err(Error::Offline(_))) {
        *slot = None;
    }
    r
}

fn meta_items(preview: usize) -> String {
    format!("(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[]<0.{preview}>)")
}

impl ImapProvider {
    /// Builds the provider. Does not connect; the first call does (see `check` to sign in up front).
    pub fn new(account: ImapAccount) -> Self {
        let creds = (account.credentials.clone(), account.smtp_credentials.clone());
        Self { account, creds: RwLock::new(creds), main: Mutex::new(None), idle: Mutex::new(None) }
    }

    /// Replaces the credentials (a refreshed OAuth token, a changed password). Open connections stay open;
    /// the next connection uses the new ones.
    pub fn set_credentials(&self, imap: Credentials, smtp: Option<Credentials>) {
        if let Ok(mut c) = self.creds.write() {
            *c = (imap, smtp);
        }
    }

    fn imap_creds(&self) -> Credentials {
        self.creds.read().map(|c| c.0.clone()).unwrap_or_else(|e| e.into_inner().0.clone())
    }

    fn smtp_creds(&self) -> Credentials {
        let c = self.creds.read().map(|c| c.clone()).unwrap_or_else(|e| e.into_inner().clone());
        c.1.unwrap_or(c.0)
    }

    fn timeout(&self) -> Duration {
        self.account.options.timeout
    }

    /// Signs in and lists folders, to tell the user at once whether the settings work.
    pub async fn check(&self) -> Result<Vec<Mailbox>> {
        let mut slot = self.main.lock().await;
        let r = async {
            let conn = self.ensure(&mut slot).await?;
            let folders = conn.list().await?;
            Ok(folders.iter().filter(|f| f.selectable()).map(|f| self.mailbox(f, None)).collect())
        }
        .await;
        keep_or_drop(&mut slot, r)
    }

    async fn ensure<'a>(&self, slot: &'a mut Option<Conn>) -> Result<&'a mut Conn> {
        if slot.is_none() {
            *slot = Some(Conn::open(&self.account.imap, &self.imap_creds(), self.timeout()).await?);
        }
        Ok(slot.as_mut().expect("just set"))
    }

    fn mailbox(&self, f: &Folder, st: Option<&FolderStatus>) -> Mailbox {
        Mailbox {
            id: MailboxId::new(f.name.clone()),
            account: self.account.id.clone(),
            name: f.display(),
            role: f.role,
            unread: st.map(|s| s.unseen).unwrap_or(0),
            total: st.map(|s| s.exists).unwrap_or(0),
        }
    }

    fn synced_folders(&self, folders: &[Folder]) -> Vec<Folder> {
        let mut v: Vec<Folder> = folders
            .iter()
            .filter(|f| f.selectable() && !self.account.options.skip_roles.contains(&f.role))
            .cloned()
            .collect();
        v.sort_by(|a, b| sync_rank(a.role).cmp(&sync_rank(b.role)).then_with(|| a.name.cmp(&b.name)));
        v
    }

    /// Metas for `uids` of the selected folder, newest first.
    async fn fetch_metas(&self, conn: &mut Conn, folder: &str, validity: u32, uids: &[u32]) -> Result<Vec<(MessageMeta, Flags)>> {
        let items = meta_items(self.account.options.preview_bytes);
        let mut out = Vec::with_capacity(uids.len());
        for set in chunks(uids, FETCH_CHUNK) {
            for f in conn.fetch(&set.to_imap(), &items).await? {
                let Some(uid) = f.uid else { continue };
                if !set.contains(uid) {
                    continue;
                }
                let key = MessageKey { folder: folder.to_owned(), validity, uid };
                let bytes = f.body.as_deref().unwrap_or(&[]);
                let info = f.info();
                let truncated = info.size.is_some_and(|s| s as usize > bytes.len());
                out.push((parse::meta(&self.account.id, &key, bytes, truncated, &info), info.flags));
            }
        }
        out.sort_by_key(|m| std::cmp::Reverse(m.0.date));
        Ok(out)
    }

    async fn sync_inner(&self, conn: &mut Conn, cursor: Option<&SyncCursor>) -> Result<Delta> {
        let mut state = CursorState::decode(cursor)?;
        let all = conn.list().await?;
        let synced = self.synced_folders(&all);
        let mut delta = Delta::default();
        let mut budget = self.account.options.page_size.max(1);
        let mut pending_new = false;

        // Folders that are gone (deleted, renamed, or now skipped).
        let names: Vec<String> = state.folders.keys().cloned().collect();
        for name in names {
            if !synced.iter().any(|f| f.name == name)
                && let Some(fc) = state.folders.remove(&name)
            {
                delta.removed.extend(fc.known_set().iter().map(|u| ids::message_id(&name, fc.validity, u)));
            }
        }

        // Pass 1: what changed in folders we already know; start new folders.
        for f in &synced {
            let st = conn.status(&f.name).await?;
            delta.mailboxes.get_or_insert_with(Vec::new).push(self.mailbox(f, Some(&st)));
            let fresh = FolderCursor {
                validity: st.validity,
                uidnext: st.uidnext,
                modseq: st.modseq,
                known: String::new(),
                floor: st.uidnext,
                below: st.exists,
                flags: None,
            };
            let Some(fc) = state.folders.get_mut(&f.name) else {
                state.folders.insert(f.name.clone(), fresh);
                continue;
            };
            if fc.validity != st.validity {
                // UIDVALIDITY changed: every UID we had is meaningless. Drop them all and start over.
                delta.removed.extend(fc.known_set().iter().map(|u| ids::message_id(&f.name, fc.validity, u)));
                *fc = fresh;
                continue;
            }
            let known = fc.known_set();
            let quiet = st.uidnext == fc.uidnext
                && known.len() + fc.below as usize == st.exists as usize
                && conn.caps.condstore
                && st.modseq.is_some()
                && st.modseq == fc.modseq;
            if quiet {
                continue;
            }
            pending_new |= self.incremental(conn, &f.name, &st, fc, known, &mut delta, &mut budget).await?;
        }

        // Pass 2: first sync, newest first, as far as the page allows.
        for f in &synced {
            if budget == 0 {
                break;
            }
            let Some(fc) = state.folders.get_mut(&f.name) else { continue };
            if !fc.complete() {
                self.backfill(conn, &f.name, fc, &mut delta, &mut budget).await?;
            }
        }

        delta.more = pending_new || synced.iter().any(|f| state.folders.get(&f.name).is_some_and(|fc| !fc.complete()));
        // A first sync reports every message across its pages; the engine drops anything else it holds.
        delta.reset = state.initial;
        if delta.mailboxes.is_none() {
            delta.mailboxes = Some(Vec::new());
        }
        state.initial = state.initial && delta.more;
        delta.cursor = Some(state.encode());
        Ok(delta)
    }

    /// New mail, flag changes and expunges in one folder we have synced before. Returns true when new
    /// mail is left for the next call.
    #[allow(clippy::too_many_arguments)]
    async fn incremental(
        &self,
        conn: &mut Conn,
        name: &str,
        st: &FolderStatus,
        fc: &mut FolderCursor,
        mut known: UidSet,
        delta: &mut Delta,
        budget: &mut usize,
    ) -> Result<bool> {
        let sel = conn.examine(name).await?;
        if sel.validity != fc.validity {
            // Changed between STATUS and SELECT; the next sync starts the folder over.
            return Ok(true);
        }
        let validity = fc.validity;
        let old_uidnext = fc.uidnext;
        let mut changed: Vec<u32> = Vec::new();
        let mut gone: Vec<u32> = Vec::new();

        // New messages.
        let mut pending = false;
        let new_uids: Vec<u32> = conn.search(&format!("UID {old_uidnext}:*")).await?.into_iter().filter(|u| *u >= old_uidnext).collect();
        if !new_uids.is_empty() {
            let take = new_uids.len().min((*budget).max(MIN_NEW_PER_CALL));
            let taken = &new_uids[..take];
            let metas = self.fetch_metas(conn, name, validity, taken).await?;
            *budget = budget.saturating_sub(metas.len());
            known.extend(taken.iter().copied());
            delta.upserted.extend(metas.into_iter().map(|(m, _)| m));
            if take < new_uids.len() {
                pending = true;
                fc.uidnext = taken[take - 1] + 1;
            } else {
                fc.uidnext = sel.uidnext.max(st.uidnext).max(taken[take - 1] + 1);
            }
        } else {
            fc.uidnext = sel.uidnext.max(st.uidnext).max(old_uidnext);
        }

        // Flag changes and expunges among messages we already had.
        let had_before = |u: u32| u < old_uidnext;
        let mut expunges_known = false;
        if conn.caps.condstore && fc.modseq.is_some() && old_uidnext > 1 {
            let since = fc.modseq.unwrap_or(0);
            let items = if conn.caps.qresync {
                format!("(UID FLAGS) (CHANGEDSINCE {since} VANISHED)")
            } else {
                format!("(UID FLAGS) (CHANGEDSINCE {since})")
            };
            let (fetched, vanished) = conn.fetch_vanished(&format!("1:{}", old_uidnext - 1), &items).await?;
            for f in fetched {
                if let Some(uid) = f.uid
                    && had_before(uid)
                    && known.contains(uid)
                {
                    changed.push(uid);
                }
            }
            if conn.caps.qresync {
                gone.extend(vanished.into_iter().filter(|u| known.contains(*u)));
                expunges_known = true;
            }
        } else if !conn.caps.condstore {
            let (flag_changed, vanished, snapshot) = self.flag_diff(conn, fc, &known, old_uidnext).await?;
            changed.extend(flag_changed);
            gone.extend(vanished);
            fc.flags = Some(snapshot);
        }
        fc.modseq = st.modseq.or(sel.modseq);

        for u in &gone {
            known.remove(*u);
        }
        if !expunges_known && sel.exists as usize != known.len() + fc.below as usize && !known.is_empty() {
            // Counts disagree: ask which of our UIDs still exist.
            let present = UidSet::from_uids(conn.search(&format!("UID {}", known.to_imap())).await?);
            for u in known.difference(&present) {
                gone.push(u);
                known.remove(u);
            }
            if !fc.complete() {
                fc.below = conn.search(&format!("UID 1:{}", fc.floor - 1)).await?.into_iter().filter(|u| *u < fc.floor).count() as u32;
            }
        }
        gone.sort_unstable();
        gone.dedup();
        delta.removed.extend(gone.iter().map(|u| ids::message_id(name, validity, *u)));
        if let Some(snap) = fc.flags.as_mut() {
            for u in &gone {
                for s in [&mut snap.seen, &mut snap.flagged, &mut snap.answered, &mut snap.draft] {
                    let mut set = UidSet::parse(s);
                    set.remove(*u);
                    *s = set.to_imap();
                }
            }
        }

        changed.retain(|u| !gone.contains(u));
        changed.sort_unstable();
        changed.dedup();
        if !changed.is_empty() {
            let metas = self.fetch_metas(conn, name, validity, &changed).await?;
            delta.upserted.extend(metas.into_iter().map(|(m, _)| m));
        }
        fc.known = known.to_imap();
        Ok(pending)
    }

    /// Without CONDSTORE: compare the flags of the newest `flag_window` known messages with last time.
    /// Returns (changed, vanished, new snapshot).
    async fn flag_diff(
        &self,
        conn: &mut Conn,
        fc: &FolderCursor,
        known: &UidSet,
        old_uidnext: u32,
    ) -> Result<(Vec<u32>, Vec<u32>, FlagSnapshot)> {
        let window = known.newest(self.account.options.flag_window.max(1));
        let Some(&from) = window.first() else {
            return Ok((Vec::new(), Vec::new(), FlagSnapshot { from: fc.uidnext, ..Default::default() }));
        };
        let mut now: BTreeMap<u32, Flags> = BTreeMap::new();
        for f in conn.fetch(&format!("{from}:*"), "(UID FLAGS)").await? {
            if let Some(uid) = f.uid
                && uid >= from
                && known.contains(uid)
            {
                now.insert(uid, f.flags);
            }
        }
        let mut changed = Vec::new();
        let mut vanished = Vec::new();
        if let Some(old) = &fc.flags {
            let sets = [&old.seen, &old.flagged, &old.answered, &old.draft].map(|s| UidSet::parse(s));
            for &u in &window {
                // Only messages that were in the old window and are not new this time.
                if u < old.from || u >= old_uidnext {
                    continue;
                }
                match now.get(&u) {
                    None => vanished.push(u),
                    Some(f) => {
                        let before = (sets[0].contains(u), sets[1].contains(u), sets[2].contains(u), sets[3].contains(u));
                        if before != (f.seen, f.flagged, f.answered, f.draft) {
                            changed.push(u);
                        }
                    }
                }
            }
        }
        let pick = |p: fn(&Flags) -> bool| UidSet::from_uids(now.iter().filter(|(_, f)| p(f)).map(|(u, _)| *u)).to_imap();
        let snapshot = FlagSnapshot {
            from,
            seen: pick(|f| f.seen),
            flagged: pick(|f| f.flagged),
            answered: pick(|f| f.answered),
            draft: pick(|f| f.draft),
        };
        Ok((changed, vanished, snapshot))
    }

    /// One page of a folder's first sync, newest first.
    async fn backfill(&self, conn: &mut Conn, name: &str, fc: &mut FolderCursor, delta: &mut Delta, budget: &mut usize) -> Result<()> {
        let sel = conn.examine(name).await?;
        if sel.validity != fc.validity {
            return Ok(());
        }
        let floor = fc.floor;
        let below: Vec<u32> = conn.search(&format!("UID 1:{}", floor - 1)).await?.into_iter().filter(|u| *u < floor).collect();
        if below.is_empty() {
            fc.floor = 0;
            fc.below = 0;
            return Ok(());
        }
        let take = below.len().min(*budget);
        let taken = &below[below.len() - take..];
        let metas = self.fetch_metas(conn, name, fc.validity, taken).await?;
        *budget = budget.saturating_sub(take);
        let mut known = fc.known_set();
        known.extend(taken.iter().copied());
        if !conn.caps.condstore && fc.flags.is_none() {
            // First page of a folder: remember the flags of the newest messages for the next flag diff.
            let window = known.newest(self.account.options.flag_window.max(1));
            let from = window.first().copied().unwrap_or(floor);
            let flags: HashMap<u32, Flags> = metas
                .iter()
                .filter_map(|(m, f)| parse_message_id(&m.id).map(|k| (k.uid, *f)))
                .collect();
            let pick = |p: fn(&Flags) -> bool| {
                UidSet::from_uids(flags.iter().filter(|(u, f)| **u >= from && p(f)).map(|(u, _)| *u)).to_imap()
            };
            fc.flags = Some(FlagSnapshot {
                from,
                seen: pick(|f| f.seen),
                flagged: pick(|f| f.flagged),
                answered: pick(|f| f.answered),
                draft: pick(|f| f.draft),
            });
        }
        delta.upserted.extend(metas.into_iter().map(|(m, _)| m));
        fc.known = known.to_imap();
        if take == below.len() {
            fc.floor = 0;
            fc.below = 0;
        } else {
            fc.floor = taken[0];
            fc.below = (below.len() - take) as u32;
        }
        Ok(())
    }

    // ---- reading ----------------------------------------------------------------------------------

    async fn raw_inner(&self, conn: &mut Conn, id: &MessageId) -> Result<Vec<u8>> {
        let key = parse_message_id(id).ok_or_else(|| Error::Invalid(format!("not an IMAP message id: {id}")))?;
        let st = conn.examine(&key.folder).await?;
        if st.validity != key.validity {
            return Err(Error::NotFound(format!("message {id} (folder was reset)")));
        }
        let fetches = conn.fetch(&key.uid.to_string(), "(UID BODY.PEEK[])").await?;
        fetches
            .iter()
            .find(|f| f.uid == Some(key.uid))
            .and_then(|f| f.body.clone())
            .ok_or_else(|| Error::NotFound(format!("message {id}")))
    }

    async fn raw(&self, id: &MessageId) -> Result<Vec<u8>> {
        let mut slot = self.main.lock().await;
        let r = async {
            let conn = self.ensure(&mut slot).await?;
            self.raw_inner(conn, id).await
        }
        .await;
        keep_or_drop(&mut slot, r)
    }

    // ---- actions ----------------------------------------------------------------------------------

    fn group(ids: &[MessageId]) -> Result<BTreeMap<(String, u32), Vec<u32>>> {
        let mut out: BTreeMap<(String, u32), Vec<u32>> = BTreeMap::new();
        for id in ids {
            let k = parse_message_id(id).ok_or_else(|| Error::Invalid(format!("not an IMAP message id: {id}")))?;
            out.entry((k.folder, k.validity)).or_default().push(k.uid);
        }
        Ok(out)
    }

    /// Selects `folder` read-write; false when its UIDVALIDITY is no longer `validity` (the ids are stale
    /// and there is nothing left to change).
    async fn select_for(&self, conn: &mut Conn, folder: &str, validity: u32) -> Result<bool> {
        match conn.select(folder).await {
            Ok(st) => Ok(st.validity == validity),
            // The folder is gone: so are the messages.
            Err(Error::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn set_flag(&self, conn: &mut Conn, ids: &[MessageId], flag: &str, on: bool) -> Result<()> {
        for ((folder, validity), uids) in Self::group(ids)? {
            if !self.select_for(conn, &folder, validity).await? {
                continue;
            }
            let item = format!("{}FLAGS.SILENT ({flag})", if on { "+" } else { "-" });
            conn.store(&UidSet::from_uids(uids), &item).await?;
        }
        Ok(())
    }

    /// The folder with `role`, creating `fallback` when there is none (and `fallback` is given).
    async fn role_folder(&self, conn: &mut Conn, role: MailboxRole, fallback: Option<&str>) -> Result<Option<String>> {
        let find = |fs: &[Folder]| fs.iter().filter(|f| f.selectable()).find(|f| f.role == role).map(|f| f.name.clone());
        if let Some(n) = find(&conn.folders().await?) {
            return Ok(Some(n));
        }
        if let Some(n) = find(&conn.list().await?) {
            return Ok(Some(n));
        }
        let Some(name) = fallback else { return Ok(None) };
        conn.create(name).await?;
        Ok(find(&conn.folders().await?).or_else(|| Some(name.to_owned())))
    }

    async fn folder_exists(&self, conn: &mut Conn, name: &str) -> Result<bool> {
        if conn.folders().await?.iter().any(|f| f.name == name) {
            return Ok(true);
        }
        Ok(conn.list().await?.iter().any(|f| f.name == name))
    }

    /// Moves messages to `dest`. UID MOVE where offered; otherwise COPY, then delete, skipping messages
    /// whose Message-ID is already in `dest` (a retry after a COPY that worked must not copy twice).
    async fn move_to(&self, conn: &mut Conn, ids: &[MessageId], dest: &str) -> Result<()> {
        self.transfer(conn, ids, dest, false).await
    }

    /// Copies messages to `dest` and leaves them where they are (`Move { from: None }`: add to `to`,
    /// remove nothing). Skips messages whose Message-ID is already in `dest`.
    async fn copy_to(&self, conn: &mut Conn, ids: &[MessageId], dest: &str) -> Result<()> {
        self.transfer(conn, ids, dest, true).await
    }

    async fn transfer(&self, conn: &mut Conn, ids: &[MessageId], dest: &str, keep_source: bool) -> Result<()> {
        for ((folder, validity), uids) in Self::group(ids)? {
            if folder == dest {
                continue;
            }
            if !self.select_for(conn, &folder, validity).await? {
                continue;
            }
            let set = UidSet::from_uids(uids);
            if conn.caps.mv && !keep_source {
                conn.mv(&set, dest).await?;
                continue;
            }
            // Message-IDs of what is still here.
            let mut by_uid: Vec<(u32, Option<String>)> = Vec::new();
            for f in conn.fetch(&set.to_imap(), "(UID BODY.PEEK[HEADER])").await? {
                let Some(uid) = f.uid else { continue };
                if !set.contains(uid) {
                    continue;
                }
                let mid = f
                    .header
                    .as_deref()
                    .and_then(|h| mail_parser::MessageParser::default().parse_headers(h).and_then(|m| m.message_id().map(str::to_owned)));
                by_uid.push((uid, mid));
            }
            if by_uid.is_empty() {
                continue; // already moved
            }
            conn.examine(dest).await?;
            let mut to_copy = Vec::new();
            for (uid, mid) in &by_uid {
                let there = match mid {
                    Some(m) => !conn.find_message_id(m).await?.is_empty(),
                    None => false,
                };
                if !there {
                    to_copy.push(*uid);
                }
            }
            conn.select(&folder).await?;
            if !to_copy.is_empty() {
                conn.copy(&UidSet::from_uids(to_copy), dest).await?;
            }
            if !keep_source {
                conn.delete_uids(&UidSet::from_uids(by_uid.iter().map(|(u, _)| *u))).await?;
            }
        }
        Ok(())
    }

    async fn trash(&self, conn: &mut Conn, ids: &[MessageId]) -> Result<()> {
        let trash = self.role_folder(conn, MailboxRole::Trash, Some("Trash")).await?.unwrap_or_else(|| "Trash".into());
        let (in_trash, elsewhere): (Vec<MessageId>, Vec<MessageId>) = ids
            .iter()
            .cloned()
            .partition(|id| parse_message_id(id).is_some_and(|k| k.folder == trash));
        self.move_to(conn, &elsewhere, &trash).await?;
        // Trashing what is already in Trash deletes it for good.
        for ((folder, validity), uids) in Self::group(&in_trash)? {
            if self.select_for(conn, &folder, validity).await? {
                conn.delete_uids(&UidSet::from_uids(uids)).await?;
            }
        }
        Ok(())
    }

    async fn archive(&self, conn: &mut Conn, ids: &[MessageId]) -> Result<()> {
        // A real Archive folder; else Gmail-style All Mail (moving there removes the Inbox label); else
        // make one.
        let dest = match self.role_folder(conn, MailboxRole::Archive, None).await? {
            Some(a) => a,
            None => match self.role_folder(conn, MailboxRole::All, None).await? {
                Some(all) => all,
                None => self.role_folder(conn, MailboxRole::Archive, Some("Archive")).await?.unwrap_or_else(|| "Archive".into()),
            },
        };
        self.move_to(conn, ids, &dest).await
    }

    fn outgoing_id(&self, local_id: &str) -> String {
        ids::outgoing_message_id(self.account.id.as_str(), local_id, self.account.identity.domain())
    }

    /// Deletes every message in `folder` with this Message-ID except the UIDs in `keep`.
    async fn delete_by_message_id(&self, conn: &mut Conn, folder: &str, message_id: &str, keep: &[u32]) -> Result<()> {
        conn.select(folder).await?;
        let uids: Vec<u32> = conn.find_message_id(message_id).await?.into_iter().filter(|u| !keep.contains(u)).collect();
        conn.delete_uids(&UidSet::from_uids(uids)).await
    }

    /// Returns the new copy's id (`Drafts:<validity>:<uid>`) when the server lets us find it.
    async fn save_draft(&self, conn: &mut Conn, draft: &Draft) -> Result<ApplyOutcome> {
        if draft.local_id.trim().is_empty() {
            return Err(Error::Invalid("a draft needs a local_id".into()));
        }
        let drafts = self.role_folder(conn, MailboxRole::Drafts, Some("Drafts")).await?.unwrap_or_else(|| "Drafts".into());
        let mid = self.outgoing_id(&draft.local_id);
        let bytes = send::build(draft, &self.account.identity, &mid, true, now()).await?;
        // Old copies first, so a retry after a lost reply never deletes the new one.
        conn.select(&drafts).await?;
        let old = conn.find_message_id(&mid).await?;
        conn.append(&drafts, "(\\Draft \\Seen)", &bytes).await?;
        let st = conn.select(&drafts).await?;
        if !old.is_empty() {
            conn.delete_uids(&UidSet::from_uids(old.iter().copied())).await?;
        }
        // The copy just appended: the one with our Message-ID that was not there before.
        let new = conn.find_message_id(&mid).await?.into_iter().filter(|u| !old.contains(u)).max();
        Ok(ApplyOutcome { server_id: new.map(|uid| ids::message_id(&drafts, st.validity, uid).0) })
    }

    async fn delete_draft(&self, conn: &mut Conn, local_id: &str) -> Result<()> {
        let Some(drafts) = self.role_folder(conn, MailboxRole::Drafts, None).await? else { return Ok(()) };
        let mid = self.outgoing_id(local_id);
        self.delete_by_message_id(conn, &drafts, &mid, &[]).await
    }

    fn server_saves_sent(&self, conn: &Conn) -> bool {
        if let Some(v) = self.account.options.server_saves_sent {
            return v;
        }
        let host = |s: &str| s.to_ascii_lowercase();
        let imap = host(&self.account.imap.host);
        let smtp = self.account.smtp.as_ref().map(|s| host(&s.host)).unwrap_or_default();
        conn.caps.gmail
            || imap == "imap.gmail.com"
            || smtp == "smtp.gmail.com"
            || imap == "outlook.office365.com"
            || smtp == "smtp.office365.com"
    }

    /// Send, at most once. The Sent folder is checked for the Message-ID first: if a previous attempt got
    /// the mail out (and it was filed), this attempt only tidies up.
    async fn send(&self, conn: &mut Conn, draft: &Draft) -> Result<()> {
        let Some(smtp) = self.account.smtp.clone() else {
            return Err(Error::Unsupported("this account has no SMTP server".into()));
        };
        if draft.local_id.trim().is_empty() {
            return Err(Error::Invalid("a draft needs a local_id".into()));
        }
        let to = send::recipients(draft)?;
        let mid = self.outgoing_id(&draft.local_id);
        let saves = self.server_saves_sent(conn);
        let sent = self.role_folder(conn, MailboxRole::Sent, None).await?;
        if let Some(sent) = &sent {
            conn.examine(sent).await?;
            if !conn.find_message_id(&mid).await?.is_empty() {
                return self.after_send(conn, draft).await;
            }
        }
        let date = now();
        let wire = send::build(draft, &self.account.identity, &mid, false, date).await?;
        send::submit(&smtp, &self.smtp_creds(), &self.account.identity, to, &wire, self.timeout()).await?;
        if !saves {
            let sent = match sent {
                Some(s) => s,
                None => self.role_folder(conn, MailboxRole::Sent, Some("Sent")).await?.unwrap_or_else(|| "Sent".into()),
            };
            let copy = send::build(draft, &self.account.identity, &mid, true, date).await?;
            conn.append(&sent, "(\\Seen)", &copy).await?;
        }
        self.after_send(conn, draft).await
    }

    /// After a send: the draft copy goes, the message answered gets \Answered. Both best effort.
    async fn after_send(&self, conn: &mut Conn, draft: &Draft) -> Result<()> {
        if let Err(e) = self.delete_draft(conn, &draft.local_id).await
            && e.is_retryable()
        {
            return Err(e);
        }
        if let Some(r) = &draft.reply_to
            && let Err(e) = self.set_flag(conn, std::slice::from_ref(r), "\\Answered", true).await
            && e.is_retryable()
        {
            return Err(e);
        }
        Ok(())
    }

    async fn apply_inner(&self, conn: &mut Conn, action: &Action) -> Result<ApplyOutcome> {
        let r = match action {
            Action::SetSeen { ids, seen } => self.set_flag(conn, ids, "\\Seen", *seen).await,
            Action::SetFlagged { ids, flagged } => self.set_flag(conn, ids, "\\Flagged", *flagged).await,
            Action::Move { ids, from, to } => {
                if !self.folder_exists(conn, to.as_str()).await? {
                    return Err(Error::NotFound(format!("folder {to}")));
                }
                match from {
                    Some(from) => {
                        let ids: Vec<MessageId> =
                            ids.iter().filter(|id| parse_message_id(id).is_none_or(|k| k.folder == from.as_str())).cloned().collect();
                        self.move_to(conn, &ids, to.as_str()).await
                    }
                    // Add to `to`, remove nothing (core's contract): a copy.
                    None => self.copy_to(conn, ids, to.as_str()).await,
                }
            }
            Action::Archive { ids } => self.archive(conn, ids).await,
            Action::Trash { ids } => self.trash(conn, ids).await,
            Action::SaveDraft { draft } => return self.save_draft(conn, draft).await,
            // The draft's Message-ID is derived from `local_id`, so the server id is not needed here.
            Action::DeleteDraft { local_id, .. } => self.delete_draft(conn, local_id).await,
            Action::Send { draft } => self.send(conn, draft).await,
        };
        r.map(|()| ApplyOutcome::default())
    }

    async fn idle_inner(&self, slot: &mut Option<Conn>, timeout: Duration) -> Result<()> {
        if slot.is_none() {
            let mut c = Conn::open(&self.account.imap, &self.imap_creds(), self.timeout()).await?;
            if !c.caps.idle {
                *slot = Some(c);
                tokio::time::sleep(timeout).await;
                return Ok(());
            }
            c.examine("INBOX").await?;
            *slot = Some(c);
        }
        let mut conn = slot.take().expect("just set");
        if !conn.caps.idle {
            *slot = Some(conn);
            tokio::time::sleep(timeout).await;
            return Ok(());
        }
        conn.drain();
        let (session, caps, folders) = conn.into_parts();
        let mut handle = session.idle();
        crate::conn::with_timeout(self.timeout(), "IDLE", async {
            handle.init().await.map_err(|e| crate::conn::imap_err("IDLE", e))
        })
        .await?;
        let (wait, _stop) = handle.wait_with_timeout(timeout);
        let r = wait.await.map_err(|e| crate::conn::imap_err("IDLE", e));
        r?;
        let session = crate::conn::with_timeout(self.timeout(), "DONE", async {
            handle.done().await.map_err(|e| crate::conn::imap_err("IDLE", e))
        })
        .await?;
        let mut conn = Conn::from_parts(session, caps, folders, self.timeout());
        conn.examine("INBOX").await?;
        *slot = Some(conn);
        Ok(())
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[async_trait::async_trait]
impl Provider for ImapProvider {
    fn account(&self) -> &AccountId {
        &self.account.id
    }

    async fn sync(&self, cursor: Option<&SyncCursor>) -> Result<Delta> {
        let mut slot = self.main.lock().await;
        let r = async {
            let conn = self.ensure(&mut slot).await?;
            self.sync_inner(conn, cursor).await
        }
        .await;
        keep_or_drop(&mut slot, r)
    }

    async fn fetch_body(&self, id: &MessageId) -> Result<Body> {
        Ok(parse::body(&self.raw(id).await?))
    }

    async fn fetch_raw(&self, id: &MessageId) -> Result<Vec<u8>> {
        self.raw(id).await
    }

    async fn fetch_attachment(&self, message: &MessageId, attachment: &AttachmentId) -> Result<Vec<u8>> {
        let raw = self.raw(message).await?;
        parse::attachment(&raw, attachment).ok_or_else(|| Error::NotFound(format!("attachment {attachment} of {message}")))
    }

    async fn apply(&self, action: &Action) -> Result<ApplyOutcome> {
        let mut slot = self.main.lock().await;
        let r = async {
            let conn = self.ensure(&mut slot).await?;
            self.apply_inner(conn, action).await
        }
        .await;
        keep_or_drop(&mut slot, r)
    }

    async fn wait_for_change(&self, timeout_s: u64) -> Result<()> {
        let timeout = Duration::from_secs(timeout_s).min(MAX_IDLE);
        let mut slot = self.idle.lock().await;
        let r = self.idle_inner(&mut slot, timeout).await;
        if r.is_err() {
            *slot = None;
        }
        r
    }
}
