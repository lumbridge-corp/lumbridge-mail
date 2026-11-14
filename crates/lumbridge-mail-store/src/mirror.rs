//! The outbox of the one-way Postgres mirror (`lumbridge-mail-pg`, driven by the daemon).
//!
//! Triggers (schema V4) note *what* changed, in order, inside the same transaction as the change: an
//! account, a mailbox, a message (metadata, flags, mailbox membership, removal), a body, or a thread's
//! triage. The data itself is read back when the mirror drains the outbox, so an entry is idempotent,
//! survives restarts, and any number of changes to one message collapse into one entry. Each entry has a
//! `gen` bumped by every change; [`Store::mirror_done`] deletes an entry only if its `gen` is still the
//! one that was taken, so a change made while the entry was being sent is sent again.
//!
//! Nothing is recorded while the mirror is off ([`Store::set_mirror_enabled`]) and nothing for anon
//! accounts except their account row (so a persona change can be purged from the mirror). The mirror
//! checks the account rules again when it drains; the triggers are the first wall, not the only one.

use crate::{Db, Store, now};
use lumbridge_mail_core::{AccountId, Error, Result};
use rusqlite::{OptionalExtension, params};

/// What an outbox entry is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutboxKind {
    Account,
    Mailbox,
    /// Metadata, flags, mailbox membership, or removal of a message.
    Message,
    /// A message's body was fetched or replaced (implies [`OutboxKind::Message`]).
    Body,
    /// A thread's triage was set, replaced, or the thread went away.
    Triage,
}

impl OutboxKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OutboxKind::Account => "account",
            OutboxKind::Mailbox => "mailbox",
            OutboxKind::Message => "message",
            OutboxKind::Body => "body",
            OutboxKind::Triage => "triage",
        }
    }

    pub fn parse(s: &str) -> Option<OutboxKind> {
        Some(match s {
            "account" => OutboxKind::Account,
            "mailbox" => OutboxKind::Mailbox,
            "message" => OutboxKind::Message,
            "body" => OutboxKind::Body,
            "triage" => OutboxKind::Triage,
            _ => return None,
        })
    }
}

/// One outbox entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxItem {
    pub seq: i64,
    pub generation: i64,
    pub kind: OutboxKind,
    pub account: AccountId,
    /// The mailbox, message or thread id; empty for [`OutboxKind::Account`].
    pub item: String,
    /// When the entry was first written (Unix seconds).
    pub at: i64,
    /// Failed sends of this entry alone (a batch that failed as a whole does not count).
    pub attempts: u32,
}

/// The mirror's bookkeeping, for `lbmail pg status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MirrorState {
    pub enabled: bool,
    /// Entries waiting (not counting stuck ones).
    pub pending: u64,
    /// Entries that failed `max_attempts` times on their own and are no longer tried.
    pub stuck: u64,
    /// When the oldest waiting entry was written (Unix seconds).
    pub oldest_at: Option<i64>,
    pub last_ok: Option<i64>,
    pub last_error: Option<String>,
    pub last_error_at: Option<i64>,
    /// Entries sent since the mirror was first enabled.
    pub sent: u64,
    /// When the last full backfill was queued.
    pub backfill_at: Option<i64>,
}

/// After this many failures on its own an entry is left alone (see [`MirrorState::stuck`]).
pub const MAX_ATTEMPTS: u32 = 10;

impl Store {
    /// Whether changes are being recorded for the mirror.
    pub fn mirror_enabled(&self) -> Result<bool> {
        self.conn
            .query_row("SELECT enabled FROM mirror_state WHERE id=1", [], |r| {
                r.get(0)
            })
            .db()
    }

    /// Turns recording on or off and returns what it was. Turning it off empties the outbox: whatever
    /// happens while it is off is not recorded, so turning it on again needs [`Store::mirror_backfill`].
    pub fn set_mirror_enabled(&mut self, on: bool) -> Result<bool> {
        let tx = self.conn.transaction().db()?;
        let was: bool = tx
            .query_row("SELECT enabled FROM mirror_state WHERE id=1", [], |r| {
                r.get(0)
            })
            .db()?;
        tx.execute("UPDATE mirror_state SET enabled=?1 WHERE id=1", [on])
            .db()?;
        if !on {
            tx.execute("DELETE FROM mirror_outbox", []).db()?;
            tx.execute("DELETE FROM mirror_membership_gone", []).db()?;
        }
        tx.commit().db()?;
        Ok(was)
    }

    /// Queues everything the store holds (one account, or all), anon accounts left out: their account
    /// rows, mailboxes, messages (with their body when one is cached) and triage. Idempotent; returns how
    /// many entries were queued or bumped. Refused while the mirror is off.
    pub fn mirror_backfill(&mut self, account: Option<&AccountId>) -> Result<u64> {
        if !self.mirror_enabled()? {
            return Err(Error::Refused(
                "the Postgres mirror is off ([pg] enabled = false)".into(),
            ));
        }
        let only = account.map(|a| a.as_str().to_owned());
        let t = now();
        let tx = self.conn.transaction().db()?;
        let mut n = 0u64;
        // ?1 = account id or NULL for all, ?2 = now.
        let statements = [
            "INSERT INTO mirror_outbox(kind, account, item, at)
             SELECT 'account', a.id, '', ?2 FROM accounts a WHERE a.anon=0 AND (?1 IS NULL OR a.id=?1)
             ON CONFLICT(kind, account, item) DO UPDATE SET gen=gen+1",
            "INSERT INTO mirror_outbox(kind, account, item, at)
             SELECT 'mailbox', a.id, b.id, ?2 FROM mailboxes b JOIN accounts a ON a.pk=b.acct
             WHERE a.anon=0 AND (?1 IS NULL OR a.id=?1)
             ON CONFLICT(kind, account, item) DO UPDATE SET gen=gen+1",
            "INSERT INTO mirror_outbox(kind, account, item, at)
             SELECT CASE WHEN EXISTS (SELECT 1 FROM bodies WHERE msg=m.pk) THEN 'body' ELSE 'message' END, a.id, m.id, ?2
             FROM messages m JOIN accounts a ON a.pk=m.acct
             WHERE a.anon=0 AND (?1 IS NULL OR a.id=?1) ORDER BY m.date DESC
             ON CONFLICT(kind, account, item) DO UPDATE SET gen=gen+1",
            "INSERT INTO mirror_outbox(kind, account, item, at)
             SELECT 'triage', a.id, t.id, ?2 FROM threads t JOIN accounts a ON a.pk=t.acct
             WHERE t.triage IS NOT NULL AND a.anon=0 AND (?1 IS NULL OR a.id=?1)
             ON CONFLICT(kind, account, item) DO UPDATE SET gen=gen+1",
        ];
        for sql in statements {
            n += tx.execute(sql, params![only, t]).db()? as u64;
        }
        if only.is_none() {
            tx.execute("UPDATE mirror_state SET backfill_at=?1 WHERE id=1", [t])
                .db()?;
        }
        tx.commit().db()?;
        Ok(n)
    }

    /// Queues one entry by hand (e.g. every account after the mirror's account rules changed). Recorded
    /// even for anon accounts: the mirror decides what to do with it. Refused while the mirror is off.
    pub fn mirror_enqueue(
        &mut self,
        kind: OutboxKind,
        account: &AccountId,
        item: &str,
    ) -> Result<()> {
        if !self.mirror_enabled()? {
            return Err(Error::Refused(
                "the Postgres mirror is off ([pg] enabled = false)".into(),
            ));
        }
        self.conn
            .execute(
                "INSERT INTO mirror_outbox(kind, account, item, at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(kind, account, item) DO UPDATE SET gen=gen+1",
                params![kind.as_str(), account.as_str(), item, now()],
            )
            .db()?;
        Ok(())
    }

    /// The oldest `limit` entries still to send, oldest first (stuck entries left out). First turns
    /// mailbox memberships removed by committed transactions into entries (see the V4 schema).
    pub fn mirror_take(&mut self, limit: u32) -> Result<Vec<OutboxItem>> {
        let parked: i64 = self
            .conn
            .query_row("SELECT count(*) FROM mirror_membership_gone", [], |r| {
                r.get(0)
            })
            .db()?;
        if parked > 0 {
            let tx = self.conn.transaction().db()?;
            tx.execute(
                "INSERT INTO mirror_outbox(kind, account, item, at)
                 SELECT DISTINCT 'message', a.id, m.id, ?1 FROM mirror_membership_gone g
                 JOIN messages m ON m.pk = g.msg JOIN accounts a ON a.pk = m.acct WHERE a.anon = 0
                 ON CONFLICT(kind, account, item) DO UPDATE SET gen = gen + 1",
                [now()],
            )
            .db()?;
            tx.execute("DELETE FROM mirror_membership_gone", []).db()?;
            tx.commit().db()?;
        }
        let mut st = self
            .conn
            .prepare_cached(
                "SELECT seq, gen, kind, account, item, at, attempts FROM mirror_outbox
                 WHERE attempts < ?1 ORDER BY seq LIMIT ?2",
            )
            .db()?;
        let rows = st
            .query_map(params![MAX_ATTEMPTS, limit], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, u32>(6)?,
                ))
            })
            .db()?
            .collect::<rusqlite::Result<Vec<_>>>()
            .db()?;
        Ok(rows
            .into_iter()
            .filter_map(|(seq, generation, kind, account, item, at, attempts)| {
                Some(OutboxItem {
                    seq,
                    generation,
                    kind: OutboxKind::parse(&kind)?,
                    account: AccountId(account),
                    item,
                    at,
                    attempts,
                })
            })
            .collect())
    }

    /// Removes sent entries (unless they changed again since they were taken) and counts them as sent.
    pub fn mirror_done(&mut self, items: &[OutboxItem]) -> Result<()> {
        let tx = self.conn.transaction().db()?;
        {
            let mut st = tx
                .prepare_cached("DELETE FROM mirror_outbox WHERE seq=?1 AND gen=?2")
                .db()?;
            for i in items {
                st.execute(params![i.seq, i.generation]).db()?;
            }
        }
        tx.execute(
            "UPDATE mirror_state SET sent=sent+?1, last_ok=?2, last_error=NULL, last_error_at=NULL WHERE id=1",
            params![items.len() as i64, now()],
        )
        .db()?;
        tx.commit().db()?;
        Ok(())
    }

    /// One entry failed on its own (the rest of its batch went through).
    pub fn mirror_failed(&mut self, item: &OutboxItem, error: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE mirror_outbox SET attempts=attempts+1 WHERE seq=?1",
                [item.seq],
            )
            .db()?;
        self.mirror_error(error)
    }

    /// Records the last error (Postgres down, a refused row), for status.
    pub fn mirror_error(&mut self, error: &str) -> Result<()> {
        let error: String = error.chars().take(500).collect();
        self.conn
            .execute(
                "UPDATE mirror_state SET last_error=?1, last_error_at=?2 WHERE id=1",
                params![error, now()],
            )
            .db()?;
        Ok(())
    }

    /// Stuck entries are tried again (after the cause was fixed).
    pub fn mirror_retry_stuck(&mut self) -> Result<u64> {
        Ok(self
            .conn
            .execute(
                "UPDATE mirror_outbox SET attempts=0 WHERE attempts>=?1",
                [MAX_ATTEMPTS],
            )
            .db()? as u64)
    }

    pub fn mirror_state(&self) -> Result<MirrorState> {
        let (enabled, last_ok, last_error, last_error_at, sent, backfill_at) = self
            .conn
            .query_row(
                "SELECT enabled, last_ok, last_error, last_error_at, sent, backfill_at FROM mirror_state WHERE id=1",
                [],
                |r| {
                    Ok((
                        r.get::<_, bool>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, Option<i64>>(5)?,
                    ))
                },
            )
            .db()?;
        let parked: i64 = self
            .conn
            .query_row("SELECT count(*) FROM mirror_membership_gone", [], |r| {
                r.get(0)
            })
            .db()?;
        let (pending, oldest_at): (i64, Option<i64>) = self
            .conn
            .query_row(
                "SELECT count(*), min(at) FROM mirror_outbox WHERE attempts < ?1",
                [MAX_ATTEMPTS],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .db()?;
        let stuck: i64 = self
            .conn
            .query_row(
                "SELECT count(*) FROM mirror_outbox WHERE attempts >= ?1",
                [MAX_ATTEMPTS],
                |r| r.get(0),
            )
            .db()?;
        Ok(MirrorState {
            enabled,
            pending: (pending + parked) as u64,
            stuck: stuck as u64,
            oldest_at,
            last_ok,
            last_error,
            last_error_at,
            sent: sent as u64,
            backfill_at,
        })
    }

    /// Whether an account is marked kept (removed from the config, mail not purged).
    pub fn account_kept(&self, id: &AccountId) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT kept FROM accounts WHERE id=?1",
                [id.as_str()],
                |r| r.get::<_, bool>(0),
            )
            .optional()
            .db()?
            .unwrap_or(false))
    }
}
