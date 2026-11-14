//! Accounts, their mailboxes and sync cursors.

use crate::{Db, Store, from_json, parse_role, to_json};
use lumbridge_mail_core::{Account, AccountId, Error, Mailbox, MailboxId, MailboxRole, Persona, Result, SyncCursor};
use rusqlite::{Connection, OptionalExtension, params};

/// The internal key of an account, or `NotFound`.
pub(crate) fn acct_pk(conn: &Connection, id: &AccountId) -> Result<i64> {
    conn.prepare_cached("SELECT pk FROM accounts WHERE id=?1")
        .db()?
        .query_row([id.as_str()], |r| r.get(0))
        .optional()
        .db()?
        .ok_or_else(|| Error::NotFound(format!("account {id}")))
}

/// Accounts a query with `accounts: None` may show: everything but anon (CONTRIBUTING.md rule 4).
/// With a list, exactly those accounts (anon included, because the user named it); unknown ids are skipped.
/// Returns `None` for "every account" so callers can leave the filter out.
pub(crate) fn allowed_accounts(conn: &Connection, named: Option<&[AccountId]>) -> Result<Option<Vec<i64>>> {
    match named {
        Some(ids) => {
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Ok(pk) = acct_pk(conn, id) {
                    out.push(pk);
                }
            }
            Ok(Some(out))
        }
        None => {
            // Kept accounts (removed from the config, mail not purged yet) are hidden like anon ones.
            let hidden: i64 = conn.query_row("SELECT count(*) FROM accounts WHERE anon=1 OR kept=1", [], |r| r.get(0)).db()?;
            if hidden == 0 {
                return Ok(None);
            }
            let mut st = conn.prepare_cached("SELECT pk FROM accounts WHERE anon=0 AND kept=0 ORDER BY pk").db()?;
            let pks = st.query_map([], |r| r.get(0)).db()?.collect::<rusqlite::Result<Vec<i64>>>().db()?;
            Ok(Some(pks))
        }
    }
}

pub(crate) fn mailbox_from_row(r: &rusqlite::Row<'_>, account: AccountId) -> rusqlite::Result<Mailbox> {
    let role: String = r.get(2)?;
    Ok(Mailbox {
        id: MailboxId::new(r.get::<_, String>(0)?),
        account,
        name: r.get(1)?,
        role: parse_role(&role).unwrap_or(MailboxRole::Custom),
        unread: r.get(3)?,
        total: r.get(4)?,
    })
}

impl Store {
    /// Adds an account, or updates it in place (keeping its mail) if the id exists. A kept account
    /// ([`Store::keep_account`]) that is added again is live again, with its mail and sync cursor.
    pub fn upsert_account(&mut self, account: &Account) -> Result<()> {
        if account.id.as_str().is_empty() {
            return Err(Error::Invalid("account id is empty".into()));
        }
        let anon = account.persona == Persona::Anon;
        self.conn
            .execute(
                "INSERT INTO accounts(id, anon, json) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET anon=excluded.anon, json=excluded.json, kept=0",
                params![account.id.as_str(), anon, to_json(account)?],
            )
            .db()?;
        Ok(())
    }

    pub fn account(&self, id: &AccountId) -> Result<Option<Account>> {
        let json: Option<String> =
            self.conn.query_row("SELECT json FROM accounts WHERE id=?1", [id.as_str()], |r| r.get(0)).optional().db()?;
        json.map(|j| from_json(&j)).transpose()
    }

    /// Every account, in the order they were added.
    pub fn accounts(&self) -> Result<Vec<Account>> {
        let mut st = self.conn.prepare_cached("SELECT json FROM accounts ORDER BY pk").db()?;
        let rows = st.query_map([], |r| r.get::<_, String>(0)).db()?;
        rows.map(|j| from_json(&j.db()?)).collect()
    }

    /// Marks an account as removed from the config but keeps its mail (and sync cursor), so adding it
    /// again costs no resync. Kept accounts are left out of every listing over "all accounts" (like
    /// anon ones) until they are purged ([`Store::remove_account`]) or added again. False if unknown.
    pub fn keep_account(&mut self, id: &AccountId) -> Result<bool> {
        Ok(self.conn.execute("UPDATE accounts SET kept=1 WHERE id=?1", [id.as_str()]).db()? > 0)
    }

    /// Accounts marked with [`Store::keep_account`], in the order they were added.
    pub fn kept_accounts(&self) -> Result<Vec<AccountId>> {
        let mut st = self.conn.prepare_cached("SELECT id FROM accounts WHERE kept=1 ORDER BY pk").db()?;
        st.query_map([], |r| Ok(AccountId(r.get(0)?))).db()?.collect::<rusqlite::Result<_>>().db()
    }

    /// Removes an account and everything stored for it (mail, drafts, queued actions, search index).
    /// Returns false if there was no such account.
    pub fn remove_account(&mut self, id: &AccountId) -> Result<bool> {
        let Ok(pk) = acct_pk(&self.conn, id) else { return Ok(false) };
        let tx = self.conn.transaction().db()?;
        tx.execute("DELETE FROM fts WHERE rowid IN (SELECT pk FROM messages WHERE acct=?1)", [pk]).db()?;
        tx.execute("DELETE FROM accounts WHERE pk=?1", [pk]).db()?;
        tx.commit().db()?;
        Ok(true)
    }

    /// The cursor from the last applied [`Delta`](lumbridge_mail_core::Delta), to hand back to
    /// `Provider::sync`. None means a full sync.
    pub fn sync_cursor(&self, account: &AccountId) -> Result<Option<SyncCursor>> {
        let pk = acct_pk(&self.conn, account)?;
        let c: Option<String> =
            self.conn.query_row("SELECT cursor FROM accounts WHERE pk=?1", [pk], |r| r.get(0)).db()?;
        Ok(c.map(SyncCursor))
    }

    /// Forgets the cursor so the next sync is a full one (e.g. after the provider says it expired).
    pub fn reset_sync_cursor(&mut self, account: &AccountId) -> Result<()> {
        let pk = acct_pk(&self.conn, account)?;
        self.conn.execute("UPDATE accounts SET cursor=NULL WHERE pk=?1", [pk]).db()?;
        Ok(())
    }

    /// When a delta was last applied (Unix seconds), for `AccountStatus::last_sync`.
    pub fn last_sync(&self, account: &AccountId) -> Result<Option<i64>> {
        let pk = acct_pk(&self.conn, account)?;
        self.conn.query_row("SELECT last_sync FROM accounts WHERE pk=?1", [pk], |r| r.get(0)).db()
    }

    /// Mailboxes of one account, or of every account (anon included: the sidebar lists them under their
    /// own account). Counts are from the local store and include not-yet-sent local changes.
    pub fn mailboxes(&self, account: Option<&AccountId>) -> Result<Vec<Mailbox>> {
        let mut out = Vec::new();
        let accounts: Vec<(i64, String)> = match account {
            Some(id) => vec![(acct_pk(&self.conn, id)?, id.0.clone())],
            None => {
                let mut st = self.conn.prepare_cached("SELECT pk, id FROM accounts ORDER BY pk").db()?;
                st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).db()?.collect::<rusqlite::Result<_>>().db()?
            }
        };
        let mut st = self
            .conn
            .prepare_cached("SELECT id, name, role, unread, total FROM mailboxes WHERE acct=?1 ORDER BY pk")
            .db()?;
        for (pk, id) in accounts {
            let account = AccountId(id);
            let rows = st.query_map([pk], |r| mailbox_from_row(r, account.clone())).db()?;
            for m in rows {
                out.push(m.db()?);
            }
        }
        Ok(out)
    }
}
