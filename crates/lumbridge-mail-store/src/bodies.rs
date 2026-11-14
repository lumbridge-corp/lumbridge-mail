//! The body cache, the attachments index and AI triage.

use crate::accounts::acct_pk;
use crate::sync::{message_pk, reindex_message, save_terms, thread_pk};
use crate::{Db, Store, category_str, from_json, now, to_json};
use lumbridge_mail_core::rpc::ThreadRef;
use lumbridge_mail_core::{AccountId, Attachment, AttachmentId, Body, Error, MessageId, Result, ThreadId, Triage};
use rusqlite::{OptionalExtension, params};

impl Store {
    /// Caches a fetched body (replacing any earlier one) and its attachment list, and adds its text and
    /// attachment names to the search index. `NotFound` if the message is not stored.
    pub fn put_body(&mut self, account: &AccountId, message: &MessageId, body: &Body) -> Result<()> {
        let tx = self.conn.transaction().db()?;
        let acct = acct_pk(&tx, account)?;
        let (pk, _) = message_pk(&tx, acct, message)?.ok_or_else(|| Error::NotFound(format!("message {message}")))?;
        tx.execute(
            "INSERT INTO bodies(msg, text, html, headers, fetched_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(msg) DO UPDATE SET text=excluded.text, html=excluded.html, headers=excluded.headers,
               fetched_at=excluded.fetched_at",
            params![pk, body.text, body.html, to_json(&body.headers)?, now()],
        )
        .db()?;
        // Keep saved paths of attachments that are still there.
        let paths: Vec<(String, Option<String>)> = tx
            .prepare("SELECT id, path FROM attachments WHERE msg=?1")
            .db()?
            .query_map([pk], |r| Ok((r.get(0)?, r.get(1)?)))
            .db()?
            .collect::<rusqlite::Result<_>>()
            .db()?;
        tx.execute("DELETE FROM attachments WHERE msg=?1", [pk]).db()?;
        for a in &body.attachments {
            let path = paths.iter().find(|(id, _)| id == a.id.as_str()).and_then(|(_, p)| p.clone());
            tx.execute(
                "INSERT OR REPLACE INTO attachments(msg, id, filename, mime, size, content_id, path)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![pk, a.id.as_str(), a.filename, a.mime, a.size as i64, a.content_id, path],
            )
            .db()?;
        }
        let mut terms = std::collections::BTreeSet::new();
        reindex_message(&tx, pk, true, &mut terms)?;
        save_terms(&tx, &terms)?;
        tx.commit().db()?;
        Ok(())
    }

    /// The cached body, or None if it has not been fetched (or was evicted).
    pub fn body(&self, account: &AccountId, message: &MessageId) -> Result<Option<Body>> {
        let acct = acct_pk(&self.conn, account)?;
        let Some((pk, _)) = message_pk(&self.conn, acct, message)? else { return Ok(None) };
        let row: Option<(Option<String>, Option<String>, String)> = self
            .conn
            .query_row("SELECT text, html, headers FROM bodies WHERE msg=?1", [pk], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()
            .db()?;
        let Some((text, html, headers)) = row else { return Ok(None) };
        Ok(Some(Body { text, html, headers: from_json(&headers)?, attachments: self.attachments_of(pk)? }))
    }

    fn attachments_of(&self, pk: i64) -> Result<Vec<Attachment>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT id, filename, mime, size, content_id FROM attachments WHERE msg=?1 ORDER BY rowid")
            .db()?;
        st.query_map([pk], |r| {
            Ok(Attachment {
                id: AttachmentId(r.get(0)?),
                filename: r.get(1)?,
                mime: r.get(2)?,
                size: r.get::<_, i64>(3)? as u64,
                content_id: r.get(4)?,
            })
        })
        .db()?
        .collect::<rusqlite::Result<_>>()
        .db()
    }

    /// A message's attachments, as known from its cached body (empty if the body is not cached).
    pub fn attachments(&self, account: &AccountId, message: &MessageId) -> Result<Vec<Attachment>> {
        let acct = acct_pk(&self.conn, account)?;
        let Some((pk, _)) = message_pk(&self.conn, acct, message)? else { return Ok(Vec::new()) };
        self.attachments_of(pk)
    }

    /// Records where the daemon saved an attachment's bytes. `NotFound` if the attachment is not indexed.
    pub fn set_attachment_path(
        &mut self,
        account: &AccountId,
        message: &MessageId,
        attachment: &AttachmentId,
        path: &str,
    ) -> Result<()> {
        let acct = acct_pk(&self.conn, account)?;
        let (pk, _) = message_pk(&self.conn, acct, message)?.ok_or_else(|| Error::NotFound(format!("message {message}")))?;
        let n = self
            .conn
            .execute("UPDATE attachments SET path=?3 WHERE msg=?1 AND id=?2", params![pk, attachment.as_str(), path])
            .db()?;
        if n == 0 {
            return Err(Error::NotFound(format!("attachment {attachment}")));
        }
        Ok(())
    }

    /// Where an attachment was saved, if it has been.
    pub fn attachment_path(&self, account: &AccountId, message: &MessageId, attachment: &AttachmentId) -> Result<Option<String>> {
        let acct = acct_pk(&self.conn, account)?;
        let Some((pk, _)) = message_pk(&self.conn, acct, message)? else { return Ok(None) };
        Ok(self
            .conn
            .query_row("SELECT path FROM attachments WHERE msg=?1 AND id=?2", params![pk, attachment.as_str()], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()
            .db()?
            .flatten())
    }

    /// Drops cached bodies fetched before `fetched_before` (Unix seconds), to bound the cache. Their words
    /// stay searchable (the index keeps them) until the message is re-indexed. Returns how many were dropped.
    pub fn evict_bodies(&mut self, fetched_before: i64) -> Result<u32> {
        Ok(self.conn.execute("DELETE FROM bodies WHERE fetched_at < ?1", [fetched_before]).db()? as u32)
    }

    /// Stores the AI layer's verdict on a thread (replacing any earlier one). `NotFound` if the thread is not stored.
    pub fn put_triage(&mut self, account: &AccountId, thread: &ThreadId, triage: &Triage) -> Result<()> {
        let acct = acct_pk(&self.conn, account)?;
        let tid = thread_pk(&self.conn, acct, thread)?.ok_or_else(|| Error::NotFound(format!("thread {thread}")))?;
        self.conn
            .execute(
                "UPDATE threads SET triage=?2, category=?3, triage_at=?4 WHERE pk=?1",
                params![tid, to_json(triage)?, category_str(triage.category), triage.at],
            )
            .db()?;
        Ok(())
    }

    pub fn triage(&self, account: &AccountId, thread: &ThreadId) -> Result<Option<Triage>> {
        let acct = acct_pk(&self.conn, account)?;
        let Some(tid) = thread_pk(&self.conn, acct, thread)? else { return Ok(None) };
        let json: Option<String> =
            self.conn.query_row("SELECT triage FROM threads WHERE pk=?1", [tid], |r| r.get(0)).db()?;
        json.map(|j| from_json(&j)).transpose()
    }

    /// Threads with no triage, or with mail newer than their triage, newest first: the AI layer's work list.
    /// Covers every account (anon included, since local models may see it); the caller checks
    /// `safety::ai_allowed` per account.
    pub fn threads_needing_triage(&self, limit: u32) -> Result<Vec<ThreadRef>> {
        let mut st = self
            .conn
            .prepare_cached(
                "SELECT a.id, t.id FROM threads t JOIN accounts a ON a.pk=t.acct
                 WHERE t.triage_at IS NULL OR t.triage_at < t.date ORDER BY t.date DESC, t.pk DESC LIMIT ?1",
            )
            .db()?;
        st.query_map([limit], |r| Ok(ThreadRef { account: AccountId(r.get(0)?), thread: ThreadId(r.get(1)?) }))
            .db()?
            .collect::<rusqlite::Result<_>>()
            .db()
    }
}
