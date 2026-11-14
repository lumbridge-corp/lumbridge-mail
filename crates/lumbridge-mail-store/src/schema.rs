//! Migrations. `PRAGMA user_version` is the applied version; each entry of `MIGRATIONS` moves the
//! database one version up inside a transaction. Never edit a shipped migration: add a new one.
//!
//! Internal keys are integer `pk`s; the provider's string ids are kept alongside and are unique per
//! account. Nothing here stores a password or token (CONTRIBUTING.md rule 6).

use crate::Db;
use lumbridge_mail_core::{Error, Result};
use rusqlite::Connection;

const MIGRATIONS: &[&str] = &[V1, V2, V3, V4, V5];

pub const VERSION: u32 = MIGRATIONS.len() as u32;

const V1: &str = r#"
CREATE TABLE accounts (
    pk INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    -- 1 for Persona::Anon: the anon wall filters on this.
    anon INTEGER NOT NULL,
    -- The whole core::Account.
    json TEXT NOT NULL,
    cursor TEXT,
    last_sync INTEGER
);

CREATE TABLE mailboxes (
    pk INTEGER PRIMARY KEY,
    acct INTEGER NOT NULL REFERENCES accounts(pk) ON DELETE CASCADE,
    id TEXT NOT NULL,
    name TEXT NOT NULL,
    role TEXT NOT NULL,
    -- Counted from the local store (so they include optimistic changes).
    unread INTEGER NOT NULL DEFAULT 0,
    total INTEGER NOT NULL DEFAULT 0,
    UNIQUE (acct, id)
);

-- One row per (account, provider thread id), kept up to date from its messages.
CREATE TABLE threads (
    pk INTEGER PRIMARY KEY,
    acct INTEGER NOT NULL REFERENCES accounts(pk) ON DELETE CASCADE,
    id TEXT NOT NULL,
    subject TEXT NOT NULL DEFAULT '',
    snippet TEXT NOT NULL DEFAULT '',
    participants TEXT NOT NULL DEFAULT '[]',
    message_count INTEGER NOT NULL DEFAULT 0,
    unread_count INTEGER NOT NULL DEFAULT 0,
    date INTEGER NOT NULL DEFAULT 0,
    flagged INTEGER NOT NULL DEFAULT 0,
    has_attachments INTEGER NOT NULL DEFAULT 0,
    mailboxes TEXT NOT NULL DEFAULT '[]',
    category TEXT,
    triage TEXT,
    triage_at INTEGER,
    UNIQUE (acct, id)
);
CREATE INDEX threads_date ON threads(date DESC, pk DESC);
CREATE INDEX threads_acct_date ON threads(acct, date DESC, pk DESC);
CREATE INDEX threads_category ON threads(category, date DESC, pk DESC) WHERE category IS NOT NULL;

CREATE TABLE messages (
    pk INTEGER PRIMARY KEY,
    acct INTEGER NOT NULL REFERENCES accounts(pk) ON DELETE CASCADE,
    id TEXT NOT NULL,
    tid INTEGER NOT NULL REFERENCES threads(pk) ON DELETE CASCADE,
    subject TEXT NOT NULL,
    snippet TEXT NOT NULL,
    from_json TEXT NOT NULL,
    to_json TEXT NOT NULL,
    cc_json TEXT NOT NULL,
    bcc_json TEXT NOT NULL,
    date INTEGER NOT NULL,
    seen INTEGER NOT NULL,
    flagged INTEGER NOT NULL,
    answered INTEGER NOT NULL,
    draft INTEGER NOT NULL,
    has_attachments INTEGER NOT NULL,
    size INTEGER NOT NULL,
    rfc_id TEXT,
    in_reply_to TEXT,
    refs_json TEXT NOT NULL,
    UNIQUE (acct, id)
);
CREATE INDEX messages_thread ON messages(tid, date);
CREATE INDEX messages_rfc ON messages(acct, rfc_id) WHERE rfc_id IS NOT NULL;
-- For searches on dates and flags alone (`before:`, `is:unread`, `is:starred`).
CREATE INDEX messages_date ON messages(date);
CREATE INDEX messages_unread ON messages(tid) WHERE seen = 0;
CREATE INDEX messages_flagged ON messages(tid) WHERE flagged = 1;

-- Which mailboxes (folders / labels) a message is in. `seen` is copied from the message so unread
-- counts are one index range.
CREATE TABLE message_mailboxes (
    msg INTEGER NOT NULL REFERENCES messages(pk) ON DELETE CASCADE,
    mbox INTEGER NOT NULL REFERENCES mailboxes(pk) ON DELETE CASCADE,
    seen INTEGER NOT NULL,
    PRIMARY KEY (msg, mbox)
) WITHOUT ROWID;
CREATE INDEX message_mailboxes_mbox ON message_mailboxes(mbox, seen);

-- Which mailboxes a thread shows in, with the thread's latest date: thread lists walk these indexes
-- newest first and stop at the page size. `role` makes the unified inbox one index range.
CREATE TABLE thread_mailboxes (
    tid INTEGER NOT NULL REFERENCES threads(pk) ON DELETE CASCADE,
    mbox INTEGER NOT NULL REFERENCES mailboxes(pk) ON DELETE CASCADE,
    role TEXT NOT NULL,
    acct INTEGER NOT NULL,
    date INTEGER NOT NULL,
    PRIMARY KEY (tid, mbox)
) WITHOUT ROWID;
CREATE INDEX thread_mailboxes_mbox ON thread_mailboxes(mbox, date DESC, tid DESC);
CREATE INDEX thread_mailboxes_role ON thread_mailboxes(role, date DESC, tid DESC);

CREATE TABLE bodies (
    msg INTEGER PRIMARY KEY REFERENCES messages(pk) ON DELETE CASCADE,
    text TEXT,
    html TEXT,
    headers TEXT NOT NULL,
    fetched_at INTEGER NOT NULL
);

CREATE TABLE attachments (
    msg INTEGER NOT NULL REFERENCES messages(pk) ON DELETE CASCADE,
    id TEXT NOT NULL,
    filename TEXT NOT NULL,
    mime TEXT NOT NULL,
    size INTEGER NOT NULL,
    content_id TEXT,
    -- Set once the daemon has saved the file under its cache directory.
    path TEXT,
    PRIMARY KEY (msg, id)
);

CREATE TABLE drafts (
    acct INTEGER NOT NULL REFERENCES accounts(pk) ON DELETE CASCADE,
    local_id TEXT NOT NULL,
    json TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'editing',
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (acct, local_id)
);

-- The durable action queue. `undo` is what the optimistic local apply changed, restored if the action
-- fails for good.
CREATE TABLE actions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    acct INTEGER NOT NULL REFERENCES accounts(pk) ON DELETE CASCADE,
    json TEXT NOT NULL,
    undo TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending',
    attempts INTEGER NOT NULL DEFAULT 0,
    next_at INTEGER NOT NULL,
    last_error TEXT,
    interrupted INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE INDEX actions_queue ON actions(acct, state, id);

-- Full-text index, one row per message (rowid = messages.pk). Contentless: the text lives in messages
-- and bodies, the index only points back.
CREATE VIRTUAL TABLE fts USING fts5(
    subject, sender, recipients, body, files,
    content='', contentless_delete=1,
    tokenize='unicode61 remove_diacritics 2',
    prefix='2 3'
);
-- Every word ever indexed (lower-cased, 2-32 characters), for prefix checks and typo tolerance. Kept by
-- us because fts5vocab reads whole doclists to count documents, which is slow on big mailboxes.
CREATE TABLE terms (term TEXT PRIMARY KEY) WITHOUT ROWID;
"#;

/// V2: full-resync bookkeeping (`Delta::reset`). A reset run has a generation number; every message a
/// page of that run reports is stamped with it, and when the last page arrives the account's messages
/// with another stamp are dropped.
const V2: &str = r#"
ALTER TABLE accounts ADD COLUMN reset_gen INTEGER NOT NULL DEFAULT 0;
ALTER TABLE accounts ADD COLUMN reset_active INTEGER NOT NULL DEFAULT 0;
ALTER TABLE messages ADD COLUMN sync_gen INTEGER;
"#;

/// V3: accounts removed from the config keep their mail until purged (`Store::keep_account`); kept
/// accounts are left out of every "all accounts" listing.
const V3: &str = r#"
ALTER TABLE accounts ADD COLUMN kept INTEGER NOT NULL DEFAULT 0;
"#;

/// V4: the Postgres mirror's outbox (see `mirror.rs`). Triggers note what changed (which account,
/// mailbox, message, body or thread triage), not the data: the mirror reads the current rows when it
/// drains, so an entry is idempotent and many changes to one message collapse into one entry (`gen`
/// counts them, so a change made while the entry is being sent is not lost). Nothing is recorded while
/// the mirror is off (`mirror_state.enabled = 0`) or for anon accounts (only their account row, so a
/// persona change to anon can be purged from the mirror).
const V4: &str = r#"
CREATE TABLE mirror_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    enabled INTEGER NOT NULL DEFAULT 0,
    last_ok INTEGER,
    last_error TEXT,
    last_error_at INTEGER,
    sent INTEGER NOT NULL DEFAULT 0,
    backfill_at INTEGER
);
INSERT INTO mirror_state(id) VALUES (1);

CREATE TABLE mirror_outbox (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    -- account | mailbox | message | body | triage
    kind TEXT NOT NULL,
    account TEXT NOT NULL,
    -- The mailbox, message or thread id; '' for an account.
    item TEXT NOT NULL DEFAULT '',
    gen INTEGER NOT NULL DEFAULT 0,
    -- When the entry was first written (the mirror's lag).
    at INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    UNIQUE (kind, account, item)
);

CREATE TRIGGER mirror_account_ins AFTER INSERT ON accounts
WHEN (SELECT enabled FROM mirror_state WHERE id = 1) = 1
BEGIN
    INSERT INTO mirror_outbox(kind, account, item, at) VALUES ('account', NEW.id, '', unixepoch())
    ON CONFLICT(kind, account, item) DO UPDATE SET gen = gen + 1;
END;
CREATE TRIGGER mirror_account_upd AFTER UPDATE OF anon, json, kept ON accounts
WHEN (SELECT enabled FROM mirror_state WHERE id = 1) = 1
BEGIN
    INSERT INTO mirror_outbox(kind, account, item, at) VALUES ('account', NEW.id, '', unixepoch())
    ON CONFLICT(kind, account, item) DO UPDATE SET gen = gen + 1;
END;
CREATE TRIGGER mirror_account_del AFTER DELETE ON accounts
WHEN (SELECT enabled FROM mirror_state WHERE id = 1) = 1
BEGIN
    INSERT INTO mirror_outbox(kind, account, item, at) VALUES ('account', OLD.id, '', unixepoch())
    ON CONFLICT(kind, account, item) DO UPDATE SET gen = gen + 1;
END;

CREATE TRIGGER mirror_mailbox_ins AFTER INSERT ON mailboxes
WHEN (SELECT enabled FROM mirror_state WHERE id = 1) = 1
  AND EXISTS (SELECT 1 FROM accounts WHERE pk = NEW.acct AND anon = 0)
BEGIN
    INSERT INTO mirror_outbox(kind, account, item, at)
    VALUES ('mailbox', (SELECT id FROM accounts WHERE pk = NEW.acct), NEW.id, unixepoch())
    ON CONFLICT(kind, account, item) DO UPDATE SET gen = gen + 1;
END;
CREATE TRIGGER mirror_mailbox_upd AFTER UPDATE OF name, role ON mailboxes
WHEN (SELECT enabled FROM mirror_state WHERE id = 1) = 1
  AND (OLD.name IS NOT NEW.name OR OLD.role IS NOT NEW.role)
  AND EXISTS (SELECT 1 FROM accounts WHERE pk = NEW.acct AND anon = 0)
BEGIN
    INSERT INTO mirror_outbox(kind, account, item, at)
    VALUES ('mailbox', (SELECT id FROM accounts WHERE pk = NEW.acct), NEW.id, unixepoch())
    ON CONFLICT(kind, account, item) DO UPDATE SET gen = gen + 1;
END;
CREATE TRIGGER mirror_mailbox_del AFTER DELETE ON mailboxes
WHEN (SELECT enabled FROM mirror_state WHERE id = 1) = 1
  AND EXISTS (SELECT 1 FROM accounts WHERE pk = OLD.acct AND anon = 0)
BEGIN
    INSERT INTO mirror_outbox(kind, account, item, at)
    VALUES ('mailbox', (SELECT id FROM accounts WHERE pk = OLD.acct), OLD.id, unixepoch())
    ON CONFLICT(kind, account, item) DO UPDATE SET gen = gen + 1;
END;

CREATE TRIGGER mirror_message_ins AFTER INSERT ON messages
WHEN (SELECT enabled FROM mirror_state WHERE id = 1) = 1
  AND EXISTS (SELECT 1 FROM accounts WHERE pk = NEW.acct AND anon = 0)
BEGIN
    INSERT INTO mirror_outbox(kind, account, item, at)
    VALUES ('message', (SELECT id FROM accounts WHERE pk = NEW.acct), NEW.id, unixepoch())
    ON CONFLICT(kind, account, item) DO UPDATE SET gen = gen + 1;
END;
-- Only real changes: a full sync rewrites every row it lists with the same values.
CREATE TRIGGER mirror_message_upd AFTER UPDATE ON messages
WHEN (SELECT enabled FROM mirror_state WHERE id = 1) = 1
  AND (OLD.tid IS NOT NEW.tid OR OLD.subject IS NOT NEW.subject OR OLD.snippet IS NOT NEW.snippet
       OR OLD.from_json IS NOT NEW.from_json OR OLD.to_json IS NOT NEW.to_json OR OLD.cc_json IS NOT NEW.cc_json
       OR OLD.bcc_json IS NOT NEW.bcc_json OR OLD.date IS NOT NEW.date OR OLD.seen IS NOT NEW.seen
       OR OLD.flagged IS NOT NEW.flagged OR OLD.answered IS NOT NEW.answered OR OLD.draft IS NOT NEW.draft
       OR OLD.has_attachments IS NOT NEW.has_attachments OR OLD.size IS NOT NEW.size
       OR OLD.rfc_id IS NOT NEW.rfc_id OR OLD.in_reply_to IS NOT NEW.in_reply_to OR OLD.refs_json IS NOT NEW.refs_json)
  AND EXISTS (SELECT 1 FROM accounts WHERE pk = NEW.acct AND anon = 0)
BEGIN
    INSERT INTO mirror_outbox(kind, account, item, at)
    VALUES ('message', (SELECT id FROM accounts WHERE pk = NEW.acct), NEW.id, unixepoch())
    ON CONFLICT(kind, account, item) DO UPDATE SET gen = gen + 1;
END;
CREATE TRIGGER mirror_message_del AFTER DELETE ON messages
WHEN (SELECT enabled FROM mirror_state WHERE id = 1) = 1
  AND EXISTS (SELECT 1 FROM accounts WHERE pk = OLD.acct AND anon = 0)
BEGIN
    INSERT INTO mirror_outbox(kind, account, item, at)
    VALUES ('message', (SELECT id FROM accounts WHERE pk = OLD.acct), OLD.id, unixepoch())
    ON CONFLICT(kind, account, item) DO UPDATE SET gen = gen + 1;
END;

-- Mailbox membership (archive, labels). A sync rewrites a message's rows (delete, then insert) even
-- when nothing changed, so a delete only parks the pair in mirror_membership_gone and an insert of the
-- same pair takes it back out. A new pair is a change at once; pairs still parked after the transaction
-- are real removals, which `Store::mirror_take` turns into entries.
CREATE TABLE mirror_membership_gone (
    msg INTEGER NOT NULL,
    mbox INTEGER NOT NULL,
    PRIMARY KEY (msg, mbox)
) WITHOUT ROWID;
CREATE TRIGGER mirror_membership_ins AFTER INSERT ON message_mailboxes
WHEN (SELECT enabled FROM mirror_state WHERE id = 1) = 1
  AND EXISTS (SELECT 1 FROM messages m JOIN accounts a ON a.pk = m.acct WHERE m.pk = NEW.msg AND a.anon = 0)
BEGIN
    INSERT INTO mirror_outbox(kind, account, item, at)
    SELECT 'message', a.id, m.id, unixepoch() FROM messages m JOIN accounts a ON a.pk = m.acct
    WHERE m.pk = NEW.msg AND NOT EXISTS (SELECT 1 FROM mirror_membership_gone g WHERE g.msg = NEW.msg AND g.mbox = NEW.mbox)
    ON CONFLICT(kind, account, item) DO UPDATE SET gen = gen + 1;
    DELETE FROM mirror_membership_gone WHERE msg = NEW.msg AND mbox = NEW.mbox;
END;
CREATE TRIGGER mirror_membership_del AFTER DELETE ON message_mailboxes
WHEN (SELECT enabled FROM mirror_state WHERE id = 1) = 1
  AND EXISTS (SELECT 1 FROM messages m JOIN accounts a ON a.pk = m.acct WHERE m.pk = OLD.msg AND a.anon = 0)
BEGIN
    INSERT OR IGNORE INTO mirror_membership_gone(msg, mbox) VALUES (OLD.msg, OLD.mbox);
END;

-- Bodies: fetched or replaced. Evicting one from the local cache is not a change to the mail, so the
-- mirror keeps it (no delete trigger).
CREATE TRIGGER mirror_body_ins AFTER INSERT ON bodies
WHEN (SELECT enabled FROM mirror_state WHERE id = 1) = 1
  AND EXISTS (SELECT 1 FROM messages m JOIN accounts a ON a.pk = m.acct WHERE m.pk = NEW.msg AND a.anon = 0)
BEGIN
    INSERT INTO mirror_outbox(kind, account, item, at)
    SELECT 'body', a.id, m.id, unixepoch() FROM messages m JOIN accounts a ON a.pk = m.acct WHERE m.pk = NEW.msg
    ON CONFLICT(kind, account, item) DO UPDATE SET gen = gen + 1;
END;
CREATE TRIGGER mirror_body_upd AFTER UPDATE OF text, html, headers ON bodies
WHEN (SELECT enabled FROM mirror_state WHERE id = 1) = 1
  AND (OLD.text IS NOT NEW.text OR OLD.html IS NOT NEW.html OR OLD.headers IS NOT NEW.headers)
  AND EXISTS (SELECT 1 FROM messages m JOIN accounts a ON a.pk = m.acct WHERE m.pk = NEW.msg AND a.anon = 0)
BEGIN
    INSERT INTO mirror_outbox(kind, account, item, at)
    SELECT 'body', a.id, m.id, unixepoch() FROM messages m JOIN accounts a ON a.pk = m.acct WHERE m.pk = NEW.msg
    ON CONFLICT(kind, account, item) DO UPDATE SET gen = gen + 1;
END;

-- Triage lives on the thread row.
CREATE TRIGGER mirror_triage_upd AFTER UPDATE OF triage ON threads
WHEN (SELECT enabled FROM mirror_state WHERE id = 1) = 1
  AND OLD.triage IS NOT NEW.triage
  AND EXISTS (SELECT 1 FROM accounts WHERE pk = NEW.acct AND anon = 0)
BEGIN
    INSERT INTO mirror_outbox(kind, account, item, at)
    VALUES ('triage', (SELECT id FROM accounts WHERE pk = NEW.acct), NEW.id, unixepoch())
    ON CONFLICT(kind, account, item) DO UPDATE SET gen = gen + 1;
END;
CREATE TRIGGER mirror_triage_del AFTER DELETE ON threads
WHEN (SELECT enabled FROM mirror_state WHERE id = 1) = 1
  AND OLD.triage IS NOT NULL
  AND EXISTS (SELECT 1 FROM accounts WHERE pk = OLD.acct AND anon = 0)
BEGIN
    INSERT INTO mirror_outbox(kind, account, item, at)
    VALUES ('triage', (SELECT id FROM accounts WHERE pk = OLD.acct), OLD.id, unixepoch())
    ON CONFLICT(kind, account, item) DO UPDATE SET gen = gen + 1;
END;
"#;

/// V5: the background body fill (`fill.rs`): per account the walk's marks, the messages set aside as
/// not fetchable, and an index to walk an account's messages newest first.
const V5: &str = r#"
CREATE INDEX messages_acct_date ON messages(acct, date DESC, pk DESC);
CREATE TABLE body_fill (
    acct INTEGER PRIMARY KEY REFERENCES accounts(pk) ON DELETE CASCADE,
    -- The newest message when the walk started (newer = new mail), and how far down it has come.
    top_date INTEGER,
    top_pk INTEGER,
    cur_date INTEGER,
    cur_pk INTEGER,
    done_at INTEGER,
    -- sync.bodies asked for it: runs even with the fill off in the config, until the walk is done.
    asked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE body_fill_unfetchable (
    msg INTEGER PRIMARY KEY REFERENCES messages(pk) ON DELETE CASCADE,
    at INTEGER NOT NULL,
    -- A short word (not_found, unsupported), never mail content.
    reason TEXT NOT NULL
);
"#;

pub fn migrate(conn: &Connection) -> Result<()> {
    let current: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).db()?;
    if current > VERSION {
        return Err(Error::Invalid(format!(
            "mail database is schema {current}, this build knows up to {VERSION}: update Lumbridge Mail"
        )));
    }
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let version = i as u32 + 1;
        if version > current {
            conn.execute_batch(&format!("BEGIN;\n{sql}\nPRAGMA user_version = {version};\nCOMMIT;"))
                .map_err(|e| {
                    let _ = conn.execute_batch("ROLLBACK");
                    Error::Other(format!("store: migration {version} failed: {e}"))
                })?;
        }
    }
    Ok(())
}
