//! The background body fill's work list (schema V5): which message without a cached body comes next,
//! and how far the walk is, kept in the database so a restart goes on where it stopped.
//!
//! Per account the fill walks the messages newest first, by `(date, pk)`. It remembers two marks:
//! `top`, the newest message when the walk started (anything newer is new mail, looked at first), and
//! `cur`, how far down the walk has come. Each step is one indexed range read from a mark, so neither a
//! restart nor a mailbox where most messages already have bodies rescans from the top. When the walk
//! reaches the oldest message it is done; [`Store::body_fill_rewind`] starts it again (the daemon does
//! once a day, to pick up mail that arrived below the marks, e.g. backfilled older mail).
//!
//! Left out: messages with a body, drafts (the flag or a Drafts mailbox), and messages marked not
//! fetchable ([`Store::body_fill_unfetchable`]: the server no longer has them, or the provider cannot
//! give a body).

use crate::accounts::acct_pk;
use crate::sync::message_pk;
use crate::{Db, Store, now, role_str};
use lumbridge_mail_core::{AccountId, MailboxRole, MessageId, Result, ThreadId};
use rusqlite::{OptionalExtension, params};

/// One message for the fill to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillItem {
    pub message: MessageId,
    pub thread: ThreadId,
    pub date: i64,
    /// Newer than the walk's top mark (new mail since the walk started).
    pub new_mail: bool,
    pk: i64,
}

/// How far the fill is for one account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FillProgress {
    /// Messages (no drafts) with a cached body, and all of them.
    pub have: u64,
    pub total: u64,
    /// Messages set aside as not fetchable.
    pub unfetchable: u64,
    /// The walk reached the oldest message (Unix time it did), if it has.
    pub done_at: Option<i64>,
    /// `sync.bodies` asked for the fill on this account (it runs even when the config has it off),
    /// until the walk is done.
    pub asked: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct Marks {
    top: Option<(i64, i64)>,
    cur: Option<(i64, i64)>,
    done_at: Option<i64>,
    asked: bool,
}

/// Messages of `?1` the fill may fetch: no body, not a draft, not set aside.
const ELIGIBLE: &str = "SELECT m.pk, m.id, t.id, m.date FROM messages m JOIN threads t ON t.pk = m.tid
     WHERE m.acct = ?1 AND m.draft = 0
       AND NOT EXISTS (SELECT 1 FROM bodies b WHERE b.msg = m.pk)
       AND NOT EXISTS (SELECT 1 FROM body_fill_unfetchable u WHERE u.msg = m.pk)
       AND NOT EXISTS (SELECT 1 FROM message_mailboxes mm JOIN mailboxes x ON x.pk = mm.mbox
                       WHERE mm.msg = m.pk AND x.role = ?2)";

fn marks(conn: &rusqlite::Connection, acct: i64) -> Result<Marks> {
    let row = conn
        .prepare_cached("SELECT top_date, top_pk, cur_date, cur_pk, done_at, asked FROM body_fill WHERE acct = ?1")
        .db()?
        .query_row([acct], |r| {
            let pair = |d: Option<i64>, p: Option<i64>| d.zip(p);
            Ok(Marks {
                top: pair(r.get(0)?, r.get(1)?),
                cur: pair(r.get(2)?, r.get(3)?),
                done_at: r.get(4)?,
                asked: r.get::<_, i64>(5)? != 0,
            })
        })
        .optional()
        .db()?;
    Ok(row.unwrap_or_default())
}

fn save(conn: &rusqlite::Connection, acct: i64, m: &Marks) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO body_fill(acct, top_date, top_pk, cur_date, cur_pk, done_at, asked) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(acct) DO UPDATE SET top_date = excluded.top_date, top_pk = excluded.top_pk, cur_date = excluded.cur_date,
           cur_pk = excluded.cur_pk, done_at = excluded.done_at, asked = excluded.asked",
    )
    .db()?
    .execute(params![
        acct,
        m.top.map(|x| x.0),
        m.top.map(|x| x.1),
        m.cur.map(|x| x.0),
        m.cur.map(|x| x.1),
        m.done_at,
        i64::from(m.asked)
    ])
    .db()?;
    Ok(())
}

/// The newest message of the account (any, with or without a body): where a walk starts.
fn newest(conn: &rusqlite::Connection, acct: i64) -> Result<Option<(i64, i64)>> {
    conn.prepare_cached("SELECT date, pk FROM messages WHERE acct = ?1 ORDER BY date DESC, pk DESC LIMIT 1")
        .db()?
        .query_row([acct], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .db()
}

fn first(conn: &rusqlite::Connection, sql: &str, args: &[&dyn rusqlite::ToSql], new_mail: bool) -> Result<Option<FillItem>> {
    conn.prepare_cached(sql)
        .db()?
        .query_row(args, |r| {
            Ok(FillItem { pk: r.get(0)?, message: MessageId(r.get(1)?), thread: ThreadId(r.get(2)?), date: r.get(3)?, new_mail })
        })
        .optional()
        .db()
}

impl Store {
    /// The next message the fill should fetch for `account`, newest first: new mail above the walk's top
    /// mark, then the walk downwards from where it is. None when there is nothing left (the walk is then
    /// marked done). Starts a walk the first time.
    pub fn body_fill_next(&mut self, account: &AccountId) -> Result<Option<FillItem>> {
        let acct = acct_pk(&self.conn, account)?;
        let drafts = role_str(MailboxRole::Drafts);
        let mut m = marks(&self.conn, acct)?;
        if let Some((d, p)) = m.top {
            let sql = format!("{ELIGIBLE} AND (m.date, m.pk) > (?3, ?4) ORDER BY m.date DESC, m.pk DESC LIMIT 1");
            if let Some(item) = first(&self.conn, &sql, &[&acct, &drafts, &d, &p], true)? {
                return Ok(Some(item));
            }
        }
        if m.done_at.is_some() {
            // Nothing new above the top: it may move up to the newest message (all of it is done).
            let n = newest(&self.conn, acct)?;
            if n != m.top {
                m.top = n;
                save(&self.conn, acct, &m)?;
            }
            return Ok(None);
        }
        if m.top.is_none() {
            m.top = newest(&self.conn, acct)?;
            if m.top.is_none() {
                return Ok(None);
            }
            m.cur = None;
            save(&self.conn, acct, &m)?;
        }
        let item = match (m.cur, m.top) {
            (Some((d, p)), _) => {
                let sql = format!("{ELIGIBLE} AND (m.date, m.pk) < (?3, ?4) ORDER BY m.date DESC, m.pk DESC LIMIT 1");
                first(&self.conn, &sql, &[&acct, &drafts, &d, &p], false)?
            }
            (None, Some((d, p))) => {
                let sql = format!("{ELIGIBLE} AND (m.date, m.pk) <= (?3, ?4) ORDER BY m.date DESC, m.pk DESC LIMIT 1");
                first(&self.conn, &sql, &[&acct, &drafts, &d, &p], false)?
            }
            (None, None) => None,
        };
        if item.is_none() {
            m.done_at = Some(now());
            m.asked = false;
            save(&self.conn, acct, &m)?;
        }
        Ok(item)
    }

    /// The walk moves past `item`, whatever became of it (fetched, or set aside). New mail above the top
    /// mark needs no mark: it leaves the work list by getting a body or being set aside.
    pub fn body_fill_passed(&mut self, account: &AccountId, item: &FillItem) -> Result<()> {
        if item.new_mail {
            return Ok(());
        }
        let acct = acct_pk(&self.conn, account)?;
        let mut m = marks(&self.conn, acct)?;
        let key = (item.date, item.pk);
        if m.cur.is_none_or(|c| key < c) {
            m.cur = Some(key);
            save(&self.conn, acct, &m)?;
        }
        Ok(())
    }

    /// Sets a message aside: the fill does not ask for it again (until a rewind with `since` covers it).
    /// `reason` is a short word (`not_found`, `unsupported`), never mail content.
    pub fn body_fill_unfetchable(&mut self, account: &AccountId, message: &MessageId, reason: &str) -> Result<()> {
        let acct = acct_pk(&self.conn, account)?;
        let Some((pk, _)) = message_pk(&self.conn, acct, message)? else { return Ok(()) };
        self.conn
            .prepare_cached("INSERT OR REPLACE INTO body_fill_unfetchable(msg, at, reason) VALUES (?1, ?2, ?3)")
            .db()?
            .execute(params![pk, now(), reason])
            .db()?;
        Ok(())
    }

    /// Starts the walk again from the newest message. With `since`, messages from that time on that
    /// were set aside are looked at again too. `ask`: the fill runs for this account until the walk is
    /// done, whatever the config says (`sync.bodies`).
    pub fn body_fill_rewind(&mut self, account: &AccountId, since: Option<i64>, ask: bool) -> Result<()> {
        let acct = acct_pk(&self.conn, account)?;
        let tx = self.conn.transaction().db()?;
        let mut m = marks(&tx, acct)?;
        m.top = newest(&tx, acct)?;
        m.cur = None;
        m.done_at = None;
        m.asked = m.asked || ask;
        save(&tx, acct, &m)?;
        if let Some(t) = since {
            tx.prepare_cached(
                "DELETE FROM body_fill_unfetchable WHERE msg IN (SELECT pk FROM messages WHERE acct = ?1 AND date >= ?2)",
            )
            .db()?
            .execute(params![acct, t])
            .db()?;
        }
        tx.commit().db()?;
        Ok(())
    }

    /// The fill runs for this account until the walk is done, whatever the config says; the walk goes
    /// on from where it is.
    pub fn body_fill_ask(&mut self, account: &AccountId) -> Result<()> {
        let acct = acct_pk(&self.conn, account)?;
        let mut m = marks(&self.conn, acct)?;
        if !m.asked {
            m.asked = true;
            save(&self.conn, acct, &m)?;
        }
        Ok(())
    }

    /// How far the fill is for `account`.
    pub fn body_fill_progress(&self, account: &AccountId) -> Result<FillProgress> {
        let acct = acct_pk(&self.conn, account)?;
        let (total, have, unfetchable): (i64, i64, i64) = self
            .conn
            .prepare_cached(
                "SELECT count(*), count(b.msg), count(u.msg) FROM messages m
                 LEFT JOIN bodies b ON b.msg = m.pk LEFT JOIN body_fill_unfetchable u ON u.msg = m.pk
                 WHERE m.acct = ?1 AND m.draft = 0
                   AND NOT EXISTS (SELECT 1 FROM message_mailboxes mm JOIN mailboxes x ON x.pk = mm.mbox
                                   WHERE mm.msg = m.pk AND x.role = ?2)",
            )
            .db()?
            .query_row(params![acct, role_str(MailboxRole::Drafts)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .db()?;
        let m = marks(&self.conn, acct)?;
        Ok(FillProgress { have: have as u64, total: total as u64, unfetchable: unfetchable as u64, done_at: m.done_at, asked: m.asked })
    }
}
