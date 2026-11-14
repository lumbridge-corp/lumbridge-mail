//! Drafts, kept locally by our `local_id`. Saving a draft here does not touch the server; queue
//! `Action::SaveDraft` for that (which also saves it here).

use crate::accounts::acct_pk;
use crate::{Db, Store, from_json, now, to_json};
use lumbridge_mail_core::{AccountId, Draft, Error, Result};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftState {
    /// Being written.
    Editing,
    /// A `Send` for it is queued; it is removed when the send is done, and goes back to `Editing` if the
    /// send fails for good or is cancelled.
    Sending,
}

impl DraftState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            DraftState::Editing => "editing",
            DraftState::Sending => "sending",
        }
    }
    fn parse(s: &str) -> Self {
        if s == "sending" { DraftState::Sending } else { DraftState::Editing }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDraft {
    pub account: AccountId,
    pub draft: Draft,
    pub state: DraftState,
    pub updated_at: i64,
}

pub(crate) fn put_draft_row(conn: &Connection, acct: i64, draft: &Draft, state: DraftState) -> Result<()> {
    if draft.local_id.is_empty() {
        return Err(Error::Invalid("draft local_id is empty".into()));
    }
    conn.prepare_cached(
        "INSERT INTO drafts(acct, local_id, json, state, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(acct, local_id) DO UPDATE SET json=excluded.json, state=excluded.state, updated_at=excluded.updated_at",
    )
    .db()?
    .execute(params![acct, draft.local_id, to_json(draft)?, state.as_str(), now()])
    .db()?;
    Ok(())
}

/// The stored (json, state) of a draft.
pub(crate) fn draft_row(conn: &Connection, acct: i64, local_id: &str) -> Result<Option<(String, String)>> {
    conn.prepare_cached("SELECT json, state FROM drafts WHERE acct=?1 AND local_id=?2")
        .db()?
        .query_row(params![acct, local_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .db()
}

pub(crate) fn delete_draft_row(conn: &Connection, acct: i64, local_id: &str) -> Result<()> {
    conn.prepare_cached("DELETE FROM drafts WHERE acct=?1 AND local_id=?2").db()?.execute(params![acct, local_id]).db()?;
    Ok(())
}

impl Store {
    /// Saves a draft locally (autosave). Does not queue anything for the server.
    pub fn save_draft(&mut self, account: &AccountId, draft: &Draft) -> Result<()> {
        let acct = acct_pk(&self.conn, account)?;
        put_draft_row(&self.conn, acct, draft, DraftState::Editing)
    }

    pub fn draft(&self, account: &AccountId, local_id: &str) -> Result<Option<StoredDraft>> {
        let acct = acct_pk(&self.conn, account)?;
        let row: Option<(String, String, i64)> = self
            .conn
            .query_row(
                "SELECT json, state, updated_at FROM drafts WHERE acct=?1 AND local_id=?2",
                params![acct, local_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .db()?;
        row.map(|(json, state, updated_at)| {
            Ok(StoredDraft { account: account.clone(), draft: from_json(&json)?, state: DraftState::parse(&state), updated_at })
        })
        .transpose()
    }

    /// One account's drafts, most recently changed first.
    pub fn drafts(&self, account: &AccountId) -> Result<Vec<StoredDraft>> {
        let acct = acct_pk(&self.conn, account)?;
        let mut st = self
            .conn
            .prepare_cached("SELECT json, state, updated_at FROM drafts WHERE acct=?1 ORDER BY updated_at DESC, local_id")
            .db()?;
        let rows: Vec<(String, String, i64)> =
            st.query_map([acct], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).db()?.collect::<rusqlite::Result<_>>().db()?;
        rows.into_iter()
            .map(|(json, state, updated_at)| {
                Ok(StoredDraft {
                    account: account.clone(),
                    draft: from_json(&json)?,
                    state: DraftState::parse(&state),
                    updated_at,
                })
            })
            .collect()
    }

    /// Deletes a draft locally only. Returns false if there was none.
    pub fn delete_draft(&mut self, account: &AccountId, local_id: &str) -> Result<bool> {
        let acct = acct_pk(&self.conn, account)?;
        Ok(self.conn.execute("DELETE FROM drafts WHERE acct=?1 AND local_id=?2", params![acct, local_id]).db()? == 1)
    }
}
