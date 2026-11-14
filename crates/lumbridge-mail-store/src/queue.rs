//! The durable action queue: every change the user makes is written here and applied to the local view
//! in one transaction, then the daemon sends it to the provider in order, one at a time per account.
//!
//! Life of an action: `enqueue` (pending, applied locally) -> `next_action` (running) -> `action_done`
//! (deleted) or `action_failed` (pending again after a backoff, or failed for good with the local change
//! rolled back). `recover_interrupted` puts actions that were running when the daemon died back in the
//! queue, marked `interrupted` so a send can be reconciled against the server before it is retried.
//!
//! The shape (state + attempts + next attempt time, exponential backoff, interrupted rows on restart)
//! follows the durable outbox in aviary (`src/runtime/operation_store.rs`, Apache-2.0); the code is our own.

use crate::accounts::acct_pk;
use crate::drafts::{DraftState, delete_draft_row, draft_row, put_draft_row};
use crate::sync::{Touched, mailbox_pk, mailboxes_with_role, message_pk};
use crate::{Db, Store, from_json, now, to_json};
use lumbridge_mail_core::rpc::Caller;
use lumbridge_mail_core::safety::SEND_NEEDS_A_PERSON;
use lumbridge_mail_core::{AccountId, Action, ApplyOutcome, Draft, Error, Flags, MailboxId, MailboxRole, MessageId, Result, ThreadId};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub type ActionId = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionState {
    /// Waiting to be sent (possibly until `next_at`, after a failure).
    Pending,
    /// Handed out by `next_action`, not yet reported done or failed.
    Running,
    /// Failed for good; its local change was rolled back. Kept so the user can see it, until cleared.
    Failed,
}

impl ActionState {
    fn as_str(self) -> &'static str {
        match self {
            ActionState::Pending => "pending",
            ActionState::Running => "running",
            ActionState::Failed => "failed",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "running" => ActionState::Running,
            "failed" => ActionState::Failed,
            _ => ActionState::Pending,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedAction {
    pub id: ActionId,
    pub account: AccountId,
    pub action: Action,
    pub state: ActionState,
    /// Failed attempts so far.
    pub attempts: u32,
    /// Not to be tried before this time (Unix seconds).
    pub next_at: i64,
    pub last_error: Option<String>,
    /// It was running when the daemon stopped: it may or may not have reached the server. For a `Send`,
    /// check the server's Sent mail before trying again.
    pub interrupted: bool,
    pub created_at: i64,
}

/// What [`Store::enqueue`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enqueued {
    pub id: ActionId,
    /// Threads the optimistic local apply changed (`Event::Changed`).
    pub changed_threads: Vec<ThreadId>,
}

/// What [`Store::action_failed`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailOutcome {
    /// Queued again; try after `at`.
    Retry { at: i64, attempts: u32 },
    /// Failed for good (`Event::ActionFailed`); the local change was rolled back on these threads.
    GaveUp { changed_threads: Vec<ThreadId> },
}

/// Exponential backoff for retryable failures (`Error::is_retryable`): `first_delay_s * 2^(n-1)`,
/// capped at `max_delay_s`, giving up after `max_attempts` failed attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub first_delay_s: i64,
    pub max_delay_s: i64,
    pub max_attempts: u32,
}

impl Default for RetryPolicy {
    /// 5 s, 10 s, 20 s ... up to 30 min, 12 attempts (about 3 hours in all).
    fn default() -> Self {
        Self { first_delay_s: 5, max_delay_s: 30 * 60, max_attempts: 12 }
    }
}

impl RetryPolicy {
    /// Delay before the next try after `attempts` failures (attempts >= 1).
    pub fn delay(&self, attempts: u32) -> i64 {
        let shift = attempts.saturating_sub(1).min(30);
        self.first_delay_s.saturating_mul(1i64 << shift).min(self.max_delay_s)
    }
}

/// What the optimistic apply changed, to put back if the action fails for good.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Undo {
    messages: Vec<MessageSnapshot>,
    draft: Option<DraftSnapshot>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MessageSnapshot {
    id: String,
    flags: Flags,
    mailboxes: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DraftSnapshot {
    local_id: String,
    /// The draft as stored before, None if there was none.
    before: Option<(String, String)>,
}

fn message_ids(action: &Action) -> Option<&[MessageId]> {
    match action {
        Action::SetSeen { ids, .. }
        | Action::SetFlagged { ids, .. }
        | Action::Move { ids, .. }
        | Action::Archive { ids }
        | Action::Trash { ids } => Some(ids),
        Action::SaveDraft { .. } | Action::DeleteDraft { .. } | Action::Send { .. } => None,
    }
}

fn memberships(conn: &Connection, msg: i64) -> Result<Vec<i64>> {
    let mut st = conn.prepare_cached("SELECT mbox FROM message_mailboxes WHERE msg=?1 ORDER BY mbox").db()?;
    st.query_map([msg], |r| r.get(0)).db()?.collect::<rusqlite::Result<_>>().db()
}

fn snapshot(conn: &Connection, msg: i64, id: &MessageId) -> Result<MessageSnapshot> {
    let flags = conn
        .prepare_cached("SELECT seen, flagged, answered, draft FROM messages WHERE pk=?1")
        .db()?
        .query_row([msg], |r| Ok(Flags { seen: r.get(0)?, flagged: r.get(1)?, answered: r.get(2)?, draft: r.get(3)? }))
        .db()?;
    let mut st = conn
        .prepare_cached("SELECT b.id FROM message_mailboxes mm JOIN mailboxes b ON b.pk=mm.mbox WHERE mm.msg=?1")
        .db()?;
    let mailboxes = st.query_map([msg], |r| r.get(0)).db()?.collect::<rusqlite::Result<_>>().db()?;
    Ok(MessageSnapshot { id: id.0.clone(), flags, mailboxes })
}

fn add_to(conn: &Connection, msg: i64, mbox: i64, touched: &mut Touched) -> Result<()> {
    conn.prepare_cached(
        "INSERT OR IGNORE INTO message_mailboxes(msg, mbox, seen) SELECT pk, ?2, seen FROM messages WHERE pk=?1",
    )
    .db()?
    .execute(params![msg, mbox])
    .db()?;
    touched.mboxes.insert(mbox);
    Ok(())
}

fn remove_from(conn: &Connection, msg: i64, mbox: i64, touched: &mut Touched) -> Result<()> {
    conn.prepare_cached("DELETE FROM message_mailboxes WHERE msg=?1 AND mbox=?2").db()?.execute(params![msg, mbox]).db()?;
    touched.mboxes.insert(mbox);
    Ok(())
}

fn set_seen(conn: &Connection, msg: i64, seen: bool) -> Result<()> {
    conn.prepare_cached("UPDATE messages SET seen=?2 WHERE pk=?1").db()?.execute(params![msg, seen]).db()?;
    conn.prepare_cached("UPDATE message_mailboxes SET seen=?2 WHERE msg=?1").db()?.execute(params![msg, seen]).db()?;
    Ok(())
}

/// Applies `action` to the local store. With `undo`, records what it changes first. Unknown messages are
/// skipped (they may not be synced yet); an unknown `Move` target is an error only when `strict`.
fn apply_local(
    conn: &Connection,
    acct: i64,
    action: &Action,
    touched: &mut Touched,
    mut undo: Option<&mut Undo>,
    strict: bool,
) -> Result<()> {
    if let Some(ids) = message_ids(action) {
        let move_to = match action {
            Action::Move { to, from, .. } => {
                let to_pk = mailbox_pk(conn, acct, to)?;
                if to_pk.is_none() && strict {
                    return Err(Error::NotFound(format!("mailbox {to}")));
                }
                let from_pk = match from {
                    Some(f) => mailbox_pk(conn, acct, f)?,
                    None => None,
                };
                Some((to_pk, from_pk))
            }
            _ => None,
        };
        let inbox = mailboxes_with_role(conn, acct, MailboxRole::Inbox)?;
        let archive = mailboxes_with_role(conn, acct, MailboxRole::Archive)?.first().copied();
        let trash = mailboxes_with_role(conn, acct, MailboxRole::Trash)?.first().copied();
        for id in ids {
            let Some((msg, tid)) = message_pk(conn, acct, id)? else { continue };
            if let Some(u) = undo.as_deref_mut() {
                u.messages.push(snapshot(conn, msg, id)?);
            }
            touched.threads.insert(tid);
            match action {
                Action::SetSeen { seen, .. } => {
                    set_seen(conn, msg, *seen)?;
                    touched.mboxes.extend(memberships(conn, msg)?);
                }
                Action::SetFlagged { flagged, .. } => {
                    conn.prepare_cached("UPDATE messages SET flagged=?2 WHERE pk=?1")
                        .db()?
                        .execute(params![msg, flagged])
                        .db()?;
                }
                Action::Move { .. } => {
                    let (to, from) = move_to.expect("set for Move");
                    if let Some(from) = from {
                        remove_from(conn, msg, from, touched)?;
                    }
                    if let Some(to) = to {
                        add_to(conn, msg, to, touched)?;
                    }
                }
                Action::Archive { .. } => {
                    for b in &inbox {
                        remove_from(conn, msg, *b, touched)?;
                    }
                    if let Some(a) = archive {
                        add_to(conn, msg, a, touched)?;
                    }
                }
                Action::Trash { .. } => {
                    for b in memberships(conn, msg)? {
                        if Some(b) != trash {
                            remove_from(conn, msg, b, touched)?;
                        }
                    }
                    if let Some(t) = trash {
                        add_to(conn, msg, t, touched)?;
                    }
                }
                _ => unreachable!("message actions only"),
            }
        }
        return Ok(());
    }
    let (local_id, next) = match action {
        Action::SaveDraft { draft } => (&draft.local_id, Some((draft, DraftState::Editing))),
        Action::Send { draft } => (&draft.local_id, Some((draft, DraftState::Sending))),
        Action::DeleteDraft { local_id, .. } => (local_id, None),
        _ => unreachable!("draft actions only"),
    };
    if local_id.is_empty() {
        return Err(Error::Invalid("draft local_id is empty".into()));
    }
    if let Some(u) = undo {
        u.draft = Some(DraftSnapshot { local_id: local_id.clone(), before: draft_row(conn, acct, local_id)? });
    }
    match next {
        Some((draft, state)) => put_draft_row(conn, acct, draft, state)?,
        None => delete_draft_row(conn, acct, local_id)?,
    }
    Ok(())
}

/// The provider's id of a stored draft, if a save reported one.
fn stored_server_id(conn: &Connection, acct: i64, local_id: &str) -> Result<Option<String>> {
    let Some((json, _)) = draft_row(conn, acct, local_id)? else { return Ok(None) };
    Ok(from_json::<Draft>(&json)?.server_id)
}

/// Draft actions carry the provider's draft id when the store knows it (from an earlier save), so
/// providers need not remember drafts in memory.
fn fill_server_id(conn: &Connection, acct: i64, action: &mut Action) -> Result<()> {
    match action {
        Action::SaveDraft { draft } | Action::Send { draft } if draft.server_id.is_none() => {
            draft.server_id = stored_server_id(conn, acct, &draft.local_id)?;
        }
        Action::DeleteDraft { local_id, server_id } if server_id.is_none() => {
            *server_id = stored_server_id(conn, acct, local_id)?;
        }
        _ => {}
    }
    Ok(())
}

/// After a save reported the provider's draft id: keep it on the stored draft and on queued actions for
/// the same draft that do not have it yet.
fn remember_server_id(conn: &Connection, acct: i64, local_id: &str, id: &str) -> Result<()> {
    if let Some((json, state)) = draft_row(conn, acct, local_id)? {
        let mut d: Draft = from_json(&json)?;
        if d.server_id.as_deref() != Some(id) {
            d.server_id = Some(id.to_owned());
            let state = if state == DraftState::Sending.as_str() { DraftState::Sending } else { DraftState::Editing };
            put_draft_row(conn, acct, &d, state)?;
        }
    }
    let rows: Vec<(i64, String)> = conn
        .prepare_cached("SELECT id, json FROM actions WHERE acct=?1 AND state IN ('pending','running') ORDER BY id")
        .db()?
        .query_map([acct], |r| Ok((r.get(0)?, r.get(1)?)))
        .db()?
        .collect::<rusqlite::Result<_>>()
        .db()?;
    for (qid, json) in rows {
        let Ok(mut action) = serde_json::from_str::<Action>(&json) else { continue };
        let changed = match &mut action {
            Action::SaveDraft { draft } | Action::Send { draft } if draft.local_id == local_id && draft.server_id.is_none() => {
                draft.server_id = Some(id.to_owned());
                true
            }
            Action::DeleteDraft { local_id: l, server_id } if l == local_id && server_id.is_none() => {
                *server_id = Some(id.to_owned());
                true
            }
            _ => false,
        };
        if changed {
            conn.prepare_cached("UPDATE actions SET json=?2 WHERE id=?1").db()?.execute(params![qid, to_json(&action)?]).db()?;
        }
    }
    Ok(())
}

/// Puts back what an action's optimistic apply changed.
fn revert(conn: &Connection, acct: i64, undo: &Undo, touched: &mut Touched) -> Result<()> {
    for s in &undo.messages {
        let Some((msg, tid)) = message_pk(conn, acct, &MessageId(s.id.clone()))? else { continue };
        touched.threads.insert(tid);
        let f = s.flags;
        conn.prepare_cached("UPDATE messages SET seen=?2, flagged=?3, answered=?4, draft=?5 WHERE pk=?1")
            .db()?
            .execute(params![msg, f.seen, f.flagged, f.answered, f.draft])
            .db()?;
        for b in memberships(conn, msg)? {
            remove_from(conn, msg, b, touched)?;
        }
        for id in &s.mailboxes {
            if let Some(b) = mailbox_pk(conn, acct, &MailboxId(id.clone()))? {
                add_to(conn, msg, b, touched)?;
            }
        }
    }
    if let Some(d) = &undo.draft {
        match &d.before {
            Some((json, state)) => {
                conn.execute(
                    "INSERT INTO drafts(acct, local_id, json, state, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(acct, local_id) DO UPDATE SET json=excluded.json, state=excluded.state,
                       updated_at=excluded.updated_at",
                    params![acct, d.local_id, json, state, now()],
                )
                .db()?;
            }
            None => delete_draft_row(conn, acct, &d.local_id)?,
        }
    }
    Ok(())
}

/// Re-applies the account's pending and running message actions that touch any of `ids`, oldest first,
/// so a sync that carries the server's older state does not undo what the user just did.
pub(crate) fn reapply_pending(conn: &Connection, acct: i64, ids: &HashSet<&str>, touched: &mut Touched) -> Result<()> {
    let rows: Vec<String> = conn
        .prepare_cached("SELECT json FROM actions WHERE acct=?1 AND state IN ('pending','running') ORDER BY id")
        .db()?
        .query_map([acct], |r| r.get(0))
        .db()?
        .collect::<rusqlite::Result<_>>()
        .db()?;
    for json in rows {
        let Ok(action) = serde_json::from_str::<Action>(&json) else { continue };
        let Some(targets) = message_ids(&action) else { continue };
        if targets.iter().any(|t| ids.contains(t.as_str())) {
            apply_local(conn, acct, &action, touched, None, false)?;
        }
    }
    Ok(())
}

const ROW_COLUMNS: &str =
    "q.id, a.id, q.json, q.state, q.attempts, q.next_at, q.last_error, q.interrupted, q.created_at";

fn row_to_action(r: &rusqlite::Row<'_>) -> rusqlite::Result<(QueuedAction, String)> {
    let json: String = r.get(2)?;
    let state: String = r.get(3)?;
    Ok((
        QueuedAction {
            id: r.get(0)?,
            account: AccountId(r.get(1)?),
            // Filled in by `decode` (serde errors are not rusqlite errors).
            action: Action::DeleteDraft { local_id: String::new(), server_id: None },
            state: ActionState::parse(&state),
            attempts: r.get(4)?,
            next_at: r.get(5)?,
            last_error: r.get(6)?,
            interrupted: r.get(7)?,
            created_at: r.get(8)?,
        },
        json,
    ))
}

fn decode((mut q, json): (QueuedAction, String)) -> Result<QueuedAction> {
    q.action = from_json(&json)?;
    Ok(q)
}

fn load(conn: &Connection, id: ActionId) -> Result<(QueuedAction, i64, String)> {
    let sql = format!("SELECT {ROW_COLUMNS}, q.acct, q.undo FROM actions q JOIN accounts a ON a.pk=q.acct WHERE q.id=?1");
    let row = conn
        .prepare_cached(&sql)
        .db()?
        .query_row([id], |r| Ok((row_to_action(r)?, r.get::<_, i64>(9)?, r.get::<_, String>(10)?)))
        .optional()
        .db()?
        .ok_or_else(|| Error::NotFound(format!("queued action {id}")))?;
    Ok((decode(row.0)?, row.1, row.2))
}

/// The head of each account's queue (its oldest pending or running action) gates the rest, so actions
/// reach the server in the order they were made.
const HEAD: &str = "q.id = (SELECT MIN(id) FROM actions h WHERE h.acct=q.acct AND h.state IN ('pending','running'))";

impl Store {
    /// Queues `action` for `account` and applies it to the local store at once, in one transaction.
    ///
    /// Refuses `Action::Send` unless `caller` is `Caller::Person` (CONTRIBUTING.md rule 2; the daemon checks too).
    /// A `Move` to a mailbox the store does not know is `NotFound`. Messages not in the store are left
    /// alone locally but the action is still queued for the server.
    pub fn enqueue(&mut self, account: &AccountId, mut action: Action, caller: Caller) -> Result<Enqueued> {
        if matches!(action, Action::Send { .. }) && caller != Caller::Person {
            return Err(Error::Refused(SEND_NEEDS_A_PERSON.into()));
        }
        let tx = self.conn.transaction().db()?;
        let acct = acct_pk(&tx, account)?;
        fill_server_id(&tx, acct, &mut action)?;
        let mut touched = Touched::default();
        let mut undo = Undo::default();
        apply_local(&tx, acct, &action, &mut touched, Some(&mut undo), true)?;
        let t = now();
        tx.prepare_cached("INSERT INTO actions(acct, json, undo, state, next_at, created_at) VALUES (?1, ?2, ?3, 'pending', ?4, ?4)")
            .db()?
            .execute(params![acct, to_json(&action)?, to_json(&undo)?, t])
            .db()?;
        let id = tx.last_insert_rowid();
        let changed_threads = touched.finish(&tx)?;
        tx.commit().db()?;
        Ok(Enqueued { id, changed_threads })
    }

    /// The next action to send, across all accounts: the oldest one at the head of its account's queue
    /// that is due by `now`. It is marked running; report it with `action_done` or `action_failed`.
    /// While an account's head is running or waiting out a backoff, nothing behind it is handed out.
    pub fn next_action(&mut self, now: i64) -> Result<Option<QueuedAction>> {
        self.next_where(None, now)
    }

    /// Like [`Store::next_action`], for one account.
    pub fn next_action_for(&mut self, account: &AccountId, now: i64) -> Result<Option<QueuedAction>> {
        let acct = acct_pk(&self.conn, account)?;
        self.next_where(Some(acct), now)
    }

    fn next_where(&mut self, acct: Option<i64>, now: i64) -> Result<Option<QueuedAction>> {
        let tx = self.conn.transaction().db()?;
        let sql = format!(
            "SELECT {ROW_COLUMNS} FROM actions q JOIN accounts a ON a.pk=q.acct
             WHERE q.state='pending' AND q.next_at<=?1 AND (?2 IS NULL OR q.acct=?2) AND {HEAD}
             ORDER BY q.id LIMIT 1"
        );
        let row = tx.prepare_cached(&sql).db()?.query_row(params![now, acct], row_to_action).optional().db()?;
        let Some(row) = row else { return Ok(None) };
        let mut q = decode(row)?;
        tx.prepare_cached("UPDATE actions SET state='running' WHERE id=?1").db()?.execute([q.id]).db()?;
        tx.commit().db()?;
        q.state = ActionState::Running;
        Ok(Some(q))
    }

    /// The provider accepted the action. A finished `Send` also removes its draft.
    pub fn action_done(&mut self, id: ActionId) -> Result<()> {
        self.action_done_with(id, &ApplyOutcome::default())
    }

    /// [`Store::action_done`] with what the provider reported: a saved draft's `server_id` is kept on
    /// the stored draft (and on queued actions for it), so later saves, sends and deletes carry it.
    pub fn action_done_with(&mut self, id: ActionId, outcome: &ApplyOutcome) -> Result<()> {
        let tx = self.conn.transaction().db()?;
        let (q, acct, _) = load(&tx, id)?;
        match &q.action {
            Action::Send { draft } => delete_draft_row(&tx, acct, &draft.local_id)?,
            Action::SaveDraft { draft } => {
                if let Some(sid) = outcome.server_id.as_deref().filter(|s| !s.is_empty()) {
                    remember_server_id(&tx, acct, &draft.local_id, sid)?;
                }
            }
            _ => {}
        }
        tx.execute("DELETE FROM actions WHERE id=?1", [id]).db()?;
        tx.commit().db()?;
        Ok(())
    }

    /// The provider said no. Retryable errors (`Error::Offline`) go back in the queue after a backoff,
    /// until the policy's attempts run out; anything else fails for good at once, which rolls back the
    /// local change (later pending actions on the same messages stay applied).
    ///
    /// For a `Send`, the daemon should check the server's Sent mail before running it again.
    pub fn action_failed(&mut self, id: ActionId, error: &Error, now: i64) -> Result<FailOutcome> {
        let tx = self.conn.transaction().db()?;
        let (q, acct, undo) = load(&tx, id)?;
        let attempts = q.attempts.saturating_add(1);
        if error.is_retryable() && attempts < self.retry.max_attempts {
            let at = now + self.retry.delay(attempts);
            tx.execute(
                "UPDATE actions SET state='pending', attempts=?2, next_at=?3, last_error=?4 WHERE id=?1",
                params![id, attempts, at, error.to_string()],
            )
            .db()?;
            tx.commit().db()?;
            return Ok(FailOutcome::Retry { at, attempts });
        }
        tx.execute(
            "UPDATE actions SET state='failed', attempts=?2, last_error=?3 WHERE id=?1",
            params![id, attempts, error.to_string()],
        )
        .db()?;
        let changed_threads = roll_back(&tx, acct, &undo)?;
        tx.commit().db()?;
        Ok(FailOutcome::GaveUp { changed_threads })
    }

    /// Takes back an action that has not started yet (e.g. "undo send" during the send delay): rolls back
    /// its local change and deletes it. Returns the changed threads, or `NotFound` / `Invalid` if it is
    /// gone or already running.
    pub fn cancel_action(&mut self, id: ActionId) -> Result<Vec<ThreadId>> {
        let tx = self.conn.transaction().db()?;
        let (q, acct, undo) = load(&tx, id)?;
        if q.state != ActionState::Pending {
            return Err(Error::Invalid(format!("action {id} is {} and cannot be cancelled", q.state.as_str())));
        }
        tx.execute("DELETE FROM actions WHERE id=?1", [id]).db()?;
        let changed = roll_back(&tx, acct, &undo)?;
        tx.commit().db()?;
        Ok(changed)
    }

    /// At daemon start: actions left running by a crash go back to pending, marked `interrupted`.
    /// Returns them (oldest first) so the daemon can reconcile sends.
    pub fn recover_interrupted(&mut self) -> Result<Vec<QueuedAction>> {
        let tx = self.conn.transaction().db()?;
        let sql = format!("SELECT {ROW_COLUMNS} FROM actions q JOIN accounts a ON a.pk=q.acct WHERE q.state='running' ORDER BY q.id");
        let rows: Vec<_> =
            tx.prepare(&sql).db()?.query_map([], row_to_action).db()?.collect::<rusqlite::Result<_>>().db()?;
        tx.execute("UPDATE actions SET state='pending', interrupted=1 WHERE state='running'", []).db()?;
        tx.commit().db()?;
        rows.into_iter()
            .map(|r| {
                let mut q = decode(r)?;
                q.state = ActionState::Pending;
                q.interrupted = true;
                Ok(q)
            })
            .collect()
    }

    /// When the next action becomes due (for the runner's timer), or None if nothing is waiting.
    pub fn next_action_due(&self, account: Option<&AccountId>) -> Result<Option<i64>> {
        let acct = account.map(|a| acct_pk(&self.conn, a)).transpose()?;
        let sql = format!(
            "SELECT MIN(q.next_at) FROM actions q WHERE q.state='pending' AND (?1 IS NULL OR q.acct=?1) AND {HEAD}"
        );
        self.conn.query_row(&sql, [acct], |r| r.get(0)).db()
    }

    /// Actions waiting to be sent (pending or running), for `AccountStatus::queued`.
    pub fn queued_count(&self, account: &AccountId) -> Result<u32> {
        let acct = acct_pk(&self.conn, account)?;
        self.conn
            .query_row("SELECT count(*) FROM actions WHERE acct=?1 AND state IN ('pending','running')", [acct], |r| r.get(0))
            .db()
    }

    /// Every queued action, failed ones included, oldest first.
    pub fn actions(&self, account: Option<&AccountId>) -> Result<Vec<QueuedAction>> {
        let acct = account.map(|a| acct_pk(&self.conn, a)).transpose()?;
        let sql = format!(
            "SELECT {ROW_COLUMNS} FROM actions q JOIN accounts a ON a.pk=q.acct WHERE (?1 IS NULL OR q.acct=?1) ORDER BY q.id"
        );
        let rows: Vec<_> = self
            .conn
            .prepare_cached(&sql)
            .db()?
            .query_map([acct], row_to_action)
            .db()?
            .collect::<rusqlite::Result<_>>()
            .db()?;
        rows.into_iter().map(decode).collect()
    }

    /// Forgets a failed action (after the user has seen it). Returns false if there was no failed action `id`.
    pub fn clear_failed(&mut self, id: ActionId) -> Result<bool> {
        Ok(self.conn.execute("DELETE FROM actions WHERE id=?1 AND state='failed'", [id]).db()? == 1)
    }
}

fn roll_back(conn: &Connection, acct: i64, undo_json: &str) -> Result<Vec<ThreadId>> {
    let undo: Undo = from_json(undo_json)?;
    let mut touched = Touched::default();
    revert(conn, acct, &undo, &mut touched)?;
    let ids: HashSet<&str> = undo.messages.iter().map(|m| m.id.as_str()).collect();
    if !ids.is_empty() {
        reapply_pending(conn, acct, &ids, &mut touched)?;
    }
    touched.finish(conn)
}
