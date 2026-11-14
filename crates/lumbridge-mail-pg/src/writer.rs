//! Writes batches of [`Change`]s: one transaction per batch, statements pipelined, every write an
//! idempotent upsert or delete, unchanged rows left untouched (no dead tuples from a resync).

use crate::connect::{ConnectOptions, connect};
use crate::rows::{BodyChange, Change};
use crate::{SCHEMA_VERSION, pg_err};
use futures::future::try_join_all;
use lumbridge_mail_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Statement, Transaction};

const UPSERT_ACCOUNT: &str = "INSERT INTO mail.accounts (id, persona, label, email, name, provider, color, ai_policy, readonly, kept)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
ON CONFLICT (id) DO UPDATE SET persona = excluded.persona, label = excluded.label, email = excluded.email,
    name = excluded.name, provider = excluded.provider, color = excluded.color, ai_policy = excluded.ai_policy,
    readonly = excluded.readonly, kept = excluded.kept, mirrored_at = now()
WHERE (mail.accounts.persona, mail.accounts.label, mail.accounts.email, mail.accounts.name, mail.accounts.provider,
       mail.accounts.color, mail.accounts.ai_policy, mail.accounts.readonly, mail.accounts.kept)
   IS DISTINCT FROM (excluded.persona, excluded.label, excluded.email, excluded.name, excluded.provider,
       excluded.color, excluded.ai_policy, excluded.readonly, excluded.kept)";

const DELETE_ACCOUNT: &str = "DELETE FROM mail.accounts WHERE id = $1";

const UPSERT_MAILBOX: &str = "INSERT INTO mail.mailboxes (account, id, name, role) VALUES ($1, $2, $3, $4)
ON CONFLICT (account, id) DO UPDATE SET name = excluded.name, role = excluded.role, mirrored_at = now()
WHERE (mail.mailboxes.name, mail.mailboxes.role) IS DISTINCT FROM (excluded.name, excluded.role)";

const DELETE_MAILBOX: &str = "DELETE FROM mail.mailboxes WHERE account = $1 AND id = $2";

const UPSERT_MESSAGE: &str = "INSERT INTO mail.messages (account, id, thread_id, mailbox_ids, subject, snippet,
    from_addrs, to_addrs, cc_addrs, bcc_addrs, from_email, from_name, participants, sent_at, seen, flagged, answered,
    draft, has_attachments, size_bytes, rfc_message_id, in_reply_to, refs, sensitive_kind)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, to_timestamp($14::bigint), $15, $16, $17, $18, $19,
    $20, $21, $22, $23, $24)
ON CONFLICT (account, id) DO UPDATE SET thread_id = excluded.thread_id, mailbox_ids = excluded.mailbox_ids,
    subject = excluded.subject, snippet = excluded.snippet, from_addrs = excluded.from_addrs,
    to_addrs = excluded.to_addrs, cc_addrs = excluded.cc_addrs, bcc_addrs = excluded.bcc_addrs,
    from_email = excluded.from_email, from_name = excluded.from_name, participants = excluded.participants,
    sent_at = excluded.sent_at, seen = excluded.seen, flagged = excluded.flagged, answered = excluded.answered,
    draft = excluded.draft, has_attachments = excluded.has_attachments, size_bytes = excluded.size_bytes,
    rfc_message_id = excluded.rfc_message_id, in_reply_to = excluded.in_reply_to, refs = excluded.refs,
    sensitive_kind = excluded.sensitive_kind, mirrored_at = now()
WHERE (mail.messages.thread_id, mail.messages.mailbox_ids, mail.messages.subject, mail.messages.snippet, mail.messages.from_addrs, mail.messages.to_addrs, mail.messages.cc_addrs, mail.messages.bcc_addrs,
       mail.messages.from_email, mail.messages.from_name, mail.messages.participants, mail.messages.sent_at, mail.messages.seen, mail.messages.flagged, mail.messages.answered, mail.messages.draft,
       mail.messages.has_attachments, mail.messages.size_bytes, mail.messages.rfc_message_id, mail.messages.in_reply_to, mail.messages.refs, mail.messages.sensitive_kind)
   IS DISTINCT FROM (excluded.thread_id, excluded.mailbox_ids, excluded.subject, excluded.snippet,
       excluded.from_addrs, excluded.to_addrs, excluded.cc_addrs, excluded.bcc_addrs, excluded.from_email,
       excluded.from_name, excluded.participants, excluded.sent_at, excluded.seen, excluded.flagged,
       excluded.answered, excluded.draft, excluded.has_attachments, excluded.size_bytes, excluded.rfc_message_id,
       excluded.in_reply_to, excluded.refs, excluded.sensitive_kind)";

const DELETE_MESSAGE: &str = "DELETE FROM mail.messages WHERE account = $1 AND id = $2";

const UPSERT_BODY: &str = "INSERT INTO mail.bodies (account, message_id, body_text, quoted_text, blocks, headers, content_hash, structured)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
ON CONFLICT (account, message_id) DO UPDATE SET body_text = excluded.body_text, quoted_text = excluded.quoted_text,
    blocks = excluded.blocks, headers = excluded.headers, content_hash = excluded.content_hash,
    structured = excluded.structured, mirrored_at = now()
WHERE mail.bodies.content_hash IS DISTINCT FROM excluded.content_hash";

const DELETE_BODY: &str = "DELETE FROM mail.bodies WHERE account = $1 AND message_id = $2";
const DELETE_ATTACHMENTS: &str =
    "DELETE FROM mail.attachments WHERE account = $1 AND message_id = $2";
const INSERT_ATTACHMENT: &str = "INSERT INTO mail.attachments (account, message_id, id, filename, mime, size_bytes, content_id)
VALUES ($1, $2, $3, $4, $5, $6, $7)
ON CONFLICT (account, message_id, id) DO UPDATE SET filename = excluded.filename, mime = excluded.mime,
    size_bytes = excluded.size_bytes, content_id = excluded.content_id";
const DELETE_DERIVED: &str =
    "WITH e AS (DELETE FROM mail.embeddings WHERE account = $1 AND message_id = $2),
     p AS (DELETE FROM mail.parsed WHERE account = $1 AND message_id = $2)
DELETE FROM mail.jobs WHERE account = $1 AND message_id = $2";

const UPSERT_TRIAGE: &str = "INSERT INTO mail.triage (account, thread_id, category, priority, summary, needs_reply, due_at, by, at, suspicious)
VALUES ($1, $2, $3, $4, $5, $6, to_timestamp($7::bigint), $8, to_timestamp($9::bigint), $10)
ON CONFLICT (account, thread_id) DO UPDATE SET category = excluded.category, priority = excluded.priority,
    summary = excluded.summary, needs_reply = excluded.needs_reply, due_at = excluded.due_at, by = excluded.by,
    at = excluded.at, suspicious = excluded.suspicious, mirrored_at = now()
WHERE (mail.triage.category, mail.triage.priority, mail.triage.summary, mail.triage.needs_reply, mail.triage.due_at,
       mail.triage.by, mail.triage.at, mail.triage.suspicious)
   IS DISTINCT FROM (excluded.category, excluded.priority, excluded.summary, excluded.needs_reply, excluded.due_at,
       excluded.by, excluded.at, excluded.suspicious)";

const DELETE_TRIAGE: &str = "DELETE FROM mail.triage WHERE account = $1 AND thread_id = $2";

/// Prepared once per connection.
struct Stmts {
    upsert_account: Statement,
    delete_account: Statement,
    upsert_mailbox: Statement,
    delete_mailbox: Statement,
    upsert_message: Statement,
    delete_message: Statement,
    upsert_body: Statement,
    delete_body: Statement,
    delete_attachments: Statement,
    insert_attachment: Statement,
    delete_derived: Statement,
    upsert_triage: Statement,
    delete_triage: Statement,
}

/// Rows per table in the mirror, for `lbmail pg status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub schema_version: Option<i32>,
    pub accounts: i64,
    pub mailboxes: i64,
    pub messages: i64,
    pub sensitive: i64,
    pub bodies: i64,
    pub attachments: i64,
    pub triage: i64,
    pub embeddings: i64,
    pub parsed: i64,
    pub jobs: i64,
}

/// A connection that writes the mirror (as `mail_writer`).
pub struct Writer {
    client: Client,
    stmts: Stmts,
}

type Param<'a> = &'a (dyn ToSql + Sync);

impl Writer {
    /// Connects and checks the schema: refused (with what to run) if it is missing or older than
    /// this build.
    pub async fn connect(dsn: &str, opts: &ConnectOptions) -> Result<Writer> {
        let client = connect(dsn, opts).await?;
        Self::from_client(client).await
    }

    /// Wraps a connected client (tests).
    pub async fn from_client(client: Client) -> Result<Writer> {
        match crate::migrate::current_version(&client).await? {
            None => {
                return Err(Error::Refused(
                    "there is no mail schema in this database: run `lbmail pg migrate`".into(),
                ));
            }
            Some(v) if v < SCHEMA_VERSION => {
                return Err(Error::Refused(format!(
                    "the mail schema is at version {v}, this build writes version {SCHEMA_VERSION}: run `lbmail pg migrate`"
                )));
            }
            Some(_) => {}
        }
        let p = |sql: &'static str| {
            let client = &client;
            async move { client.prepare(sql).await.map_err(pg_err) }
        };
        let stmts = Stmts {
            upsert_account: p(UPSERT_ACCOUNT).await?,
            delete_account: p(DELETE_ACCOUNT).await?,
            upsert_mailbox: p(UPSERT_MAILBOX).await?,
            delete_mailbox: p(DELETE_MAILBOX).await?,
            upsert_message: p(UPSERT_MESSAGE).await?,
            delete_message: p(DELETE_MESSAGE).await?,
            upsert_body: p(UPSERT_BODY).await?,
            delete_body: p(DELETE_BODY).await?,
            delete_attachments: p(DELETE_ATTACHMENTS).await?,
            insert_attachment: p(INSERT_ATTACHMENT).await?,
            delete_derived: p(DELETE_DERIVED).await?,
            upsert_triage: p(UPSERT_TRIAGE).await?,
            delete_triage: p(DELETE_TRIAGE).await?,
        };
        Ok(Writer { client, stmts })
    }

    /// Whether the connection is gone (reconnect).
    pub fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    /// Applies `changes` in one transaction, in order (account rows first). All or nothing.
    pub async fn apply(&mut self, changes: &[Change]) -> Result<()> {
        let tx = self.client.transaction().await.map_err(pg_err)?;
        // Accounts first: everything else refers to them.
        let accounts: Vec<&Change> = changes
            .iter()
            .filter(|c| matches!(c, Change::UpsertAccount(_)))
            .collect();
        run_all(&tx, &self.stmts, &accounts).await?;
        let rest: Vec<&Change> = changes
            .iter()
            .filter(|c| !matches!(c, Change::UpsertAccount(_)))
            .collect();
        run_all(&tx, &self.stmts, &rest).await?;
        tx.commit().await.map_err(pg_err)
    }

    /// The ids of the accounts in the mirror.
    pub async fn account_ids(&self) -> Result<HashSet<String>> {
        let rows = self
            .client
            .query("SELECT id FROM mail.accounts", &[])
            .await
            .map_err(pg_err)?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    pub async fn counts(&self) -> Result<Counts> {
        counts(&self.client).await
    }
}

/// Row counts of the mirror (any role that may read `mail`).
pub async fn counts(client: &Client) -> Result<Counts> {
    let v = crate::migrate::current_version(client).await?;
    if v.is_none() {
        return Ok(Counts::default());
    }
    let r = client
        .query_one(
            "SELECT (SELECT count(*) FROM mail.accounts), (SELECT count(*) FROM mail.mailboxes),
                    (SELECT count(*) FROM mail.messages), (SELECT count(*) FROM mail.messages WHERE sensitive_kind IS NOT NULL),
                    (SELECT count(*) FROM mail.bodies), (SELECT count(*) FROM mail.attachments),
                    (SELECT count(*) FROM mail.triage), (SELECT count(*) FROM mail.embeddings),
                    (SELECT count(*) FROM mail.parsed), (SELECT count(*) FROM mail.jobs)",
            &[],
        )
        .await
        .map_err(pg_err)?;
    Ok(Counts {
        schema_version: v,
        accounts: r.get(0),
        mailboxes: r.get(1),
        messages: r.get(2),
        sensitive: r.get(3),
        bodies: r.get(4),
        attachments: r.get(5),
        triage: r.get(6),
        embeddings: r.get(7),
        parsed: r.get(8),
        jobs: r.get(9),
    })
}

/// Sends every statement of `changes` without waiting between them (tokio-postgres pipelines them
/// on the connection, in order), then waits for all.
async fn run_all(tx: &Transaction<'_>, s: &Stmts, changes: &[&Change]) -> Result<()> {
    // Owned parameters live here while the statements run.
    struct Call<'a> {
        stmt: &'a Statement,
        params: Vec<Box<dyn ToSql + Sync + Send + 'a>>,
    }
    let mut calls: Vec<Call<'_>> = Vec::new();
    macro_rules! call {
        ($stmt:expr, $($p:expr),* $(,)?) => {
            calls.push(Call { stmt: $stmt, params: vec![$(Box::new($p)),*] })
        };
    }
    for c in changes {
        match c {
            Change::UpsertAccount(a) => call!(
                &s.upsert_account,
                &a.id,
                &a.persona,
                &a.label,
                &a.email,
                &a.name,
                &a.provider,
                &a.color,
                &a.ai_policy,
                a.readonly,
                a.kept
            ),
            Change::DeleteAccount { id } => call!(&s.delete_account, id),
            Change::UpsertMailbox(m) => {
                call!(&s.upsert_mailbox, &m.account, &m.id, &m.name, &m.role)
            }
            Change::DeleteMailbox { account, id } => call!(&s.delete_mailbox, account, id),
            Change::UpsertMessage { row: m, body } => {
                call!(
                    &s.upsert_message,
                    &m.account,
                    &m.id,
                    &m.thread_id,
                    &m.mailbox_ids,
                    &m.subject,
                    &m.snippet,
                    &m.from_addrs,
                    &m.to_addrs,
                    &m.cc_addrs,
                    &m.bcc_addrs,
                    &m.from_email,
                    &m.from_name,
                    &m.participants,
                    m.sent_at,
                    m.seen,
                    m.flagged,
                    m.answered,
                    m.draft,
                    m.has_attachments,
                    m.size_bytes,
                    &m.rfc_message_id,
                    &m.in_reply_to,
                    &m.refs,
                    &m.sensitive_kind,
                );
                // A sensitive message never gets a body, whatever the caller built.
                let body = if m.sensitive_kind.is_some() {
                    &BodyChange::Drop
                } else {
                    body
                };
                match body {
                    BodyChange::Keep => {}
                    BodyChange::Put(b) => {
                        call!(
                            &s.upsert_body,
                            &b.account,
                            &b.message_id,
                            &b.body_text,
                            &b.quoted_text,
                            &b.blocks,
                            &b.headers,
                            &b.content_hash,
                            &b.structured
                        );
                        call!(&s.delete_attachments, &m.account, &m.id);
                        for a in &b.attachments {
                            call!(
                                &s.insert_attachment,
                                &m.account,
                                &m.id,
                                &a.id,
                                &a.filename,
                                &a.mime,
                                a.size_bytes,
                                &a.content_id
                            );
                        }
                    }
                    BodyChange::Drop => {
                        call!(&s.delete_body, &m.account, &m.id);
                        call!(&s.delete_attachments, &m.account, &m.id);
                        call!(&s.delete_derived, &m.account, &m.id);
                    }
                }
            }
            Change::DeleteMessage { account, id } => call!(&s.delete_message, account, id),
            Change::UpsertTriage(t) => call!(
                &s.upsert_triage,
                &t.account,
                &t.thread_id,
                &t.category,
                t.priority,
                &t.summary,
                t.needs_reply,
                t.due_at,
                &t.by,
                t.at,
                t.suspicious
            ),
            Change::DeleteTriage { account, thread_id } => {
                call!(&s.delete_triage, account, thread_id)
            }
        }
    }
    let futs = calls.iter().map(|c| {
        let params: Vec<Param<'_>> = c.params.iter().map(|p| p.as_ref() as Param<'_>).collect();
        async move { tx.execute(c.stmt, &params).await }
    });
    try_join_all(futs).await.map_err(pg_err)?;
    Ok(())
}
