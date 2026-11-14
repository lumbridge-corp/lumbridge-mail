//! What the daemon's embedding and parsing workers do in Postgres (schema version 2): claim jobs
//! from `mail.jobs`, read what a job needs, write `mail.embeddings` / `mail.parsed`, and count what
//! happened in `mail.job_stats`. Also the read side: [`search`] (hybrid, with a query vector) and
//! [`parsed`] (the facts), which any role that may read `mail` can run.
//!
//! Jobs are claimed with `FOR UPDATE SKIP LOCKED` and leased with `locked_until`, so two workers
//! never take the same job; a worker that dies leaves its jobs to come back when the lease ends. A
//! job is identified by its row and the time it was (re)queued ([`Job::generation`]): when a body changes
//! while its job is being worked on, the trigger re-queues it and finishing the old work does not
//! delete the new job.
//!
//! The workers log in as `mail_writer` (the daemon's role): a separate role would not separate
//! anything, since the same process holds both. The schema's triggers refuse any embedding, fact or
//! job for sensitive mail whoever writes it; the workers also refuse it before they read anything.

use crate::connect::{ConnectOptions, connect};
use crate::pg_err;
use lumbridge_mail_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio_postgres::Client;

/// The two queues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Embed,
    Parse,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            JobKind::Embed => "embed",
            JobKind::Parse => "parse",
        }
    }
}

/// A claimed job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub kind: JobKind,
    pub account: String,
    pub message_id: String,
    /// Tries so far, this one included.
    pub attempts: i32,
    /// When it was (re)queued, in microseconds: finishing deletes it only if it was not re-queued.
    pub generation: i64,
    /// Seconds since it was (re)queued.
    pub age_s: i64,
}

/// What an embedding needs. `body_text` is the clean new text (no quoted history), as mirrored.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedSource {
    pub subject: String,
    pub from_name: Option<String>,
    pub from_email: String,
    pub sensitive_kind: Option<String>,
    /// None: no body in Postgres (the job is stale).
    pub body_text: Option<String>,
    pub content_hash: Option<String>,
    /// The account row's `ai_policy` and `persona` (wire names).
    pub ai_policy: String,
    pub persona: String,
}

/// What parsing needs.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseSource {
    pub thread_id: String,
    pub subject: String,
    pub snippet: String,
    pub from_name: Option<String>,
    pub from_email: String,
    /// Unix seconds.
    pub sent_at: Option<i64>,
    pub sensitive_kind: Option<String>,
    pub body_text: Option<String>,
    pub content_hash: Option<String>,
    /// `[[name, value], ...]` as mirrored (List-Id, List-Unsubscribe, ...).
    pub headers: Value,
    /// schema.org objects from the HTML.
    pub structured: Value,
    /// The thread's triage category, if it has been triaged.
    pub category: Option<String>,
    pub attachment_mimes: Vec<String>,
    pub ai_policy: String,
    pub persona: String,
}

/// One embedded chunk to write.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingRow {
    pub chunk: i32,
    /// sha256 of the text that was embedded.
    pub content_hash: String,
    pub vector: Vec<f32>,
}

/// One fact to write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FactRow {
    pub kind: String,
    pub data: Value,
    /// `rules/jsonld`, `model/local/my-model`, ...: the workers own every parser under `rules/` and
    /// `model/` and replace those rows on each parse.
    pub parser: String,
    pub parser_version: String,
    pub confidence: f32,
}

/// One message's embeddings for [`Work::put_embeddings_many`]: account, message id, the job to
/// finish with it, the rows.
pub type EmbedItem<'a> = (&'a str, &'a str, Option<&'a Job>, &'a [EmbeddingRow]);

/// One message's facts for [`Work::put_facts_many`]: account, message id, the body's content hash,
/// the job to finish with it, the facts.
pub type FactItem<'a> = (
    &'a str,
    &'a str,
    Option<&'a str>,
    Option<&'a Job>,
    &'a [FactRow],
);

/// Counters for `mail.job_stats` (added to the current hour).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tally {
    pub done: i64,
    pub unchanged: i64,
    pub skipped: i64,
    pub failed: i64,
    pub parked: i64,
    pub calls: i64,
    pub items: i64,
    pub busy_ms: i64,
}

impl Tally {
    pub fn is_empty(&self) -> bool {
        *self == Tally::default()
    }
    pub fn add(&mut self, o: &Tally) {
        self.done += o.done;
        self.unchanged += o.unchanged;
        self.skipped += o.skipped;
        self.failed += o.failed;
        self.parked += o.parked;
        self.calls += o.calls;
        self.items += o.items;
        self.busy_ms += o.busy_ms;
    }
}

/// One queue's state, for `lbmail pg status`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueueState {
    pub kind: String,
    /// Waiting to be claimed now.
    pub ready: i64,
    /// Claimed (leased) or waiting out a retry backoff.
    pub waiting: i64,
    pub parked: i64,
    /// Seconds since the oldest unparked job was queued.
    pub oldest_s: Option<i64>,
    /// The most recent error of a job still in the queue.
    pub last_error: Option<String>,
    /// From `mail.job_stats`: the last hour, the last 24 hours, all time.
    pub last_hour: Tally,
    pub last_day: Tally,
    pub total: Tally,
}

const COLUMN_INFO: &str = "SELECT format_type(a.atttypid, NULL), a.atttypmod FROM pg_attribute a
WHERE a.attrelid = to_regclass('mail.embeddings') AND a.attname = 'embedding' AND NOT a.attisdropped";

/// The embedding column's type name (schema-qualified when its schema is not on the search path)
/// and dimension.
pub async fn embedding_column(client: &Client) -> Result<(String, usize)> {
    let row = client
        .query_opt(COLUMN_INFO, &[])
        .await
        .map_err(pg_err)?
        .ok_or_else(|| {
            Error::Refused("there is no mail.embeddings: run `lbmail pg migrate`".into())
        })?;
    let ty: String = row.get(0);
    let dim: i32 = row.get(1);
    // From the catalog, but it goes into SQL text: plain names only.
    if ty.is_empty()
        || !ty.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.' || c == '"'
        })
    {
        return Err(Error::Invalid(format!(
            "mail.embeddings.embedding has an unexpected type {ty:?}"
        )));
    }
    if dim <= 0 {
        return Err(Error::Invalid(
            "mail.embeddings.embedding has no dimension".into(),
        ));
    }
    Ok((ty, dim as usize))
}

/// A vector as pgvector's text form. Refuses values a `halfvec` cannot hold.
pub fn vector_literal(v: &[f32]) -> Result<String> {
    let mut s = String::with_capacity(v.len() * 10 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if !x.is_finite() || x.abs() > 65_000.0 {
            return Err(Error::Invalid(format!(
                "embedding value {i} is out of range"
            )));
        }
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push(']');
    Ok(s)
}

const GEN: &str = "(extract(epoch FROM enqueued_at) * 1000000)::bigint";

fn split(keys: &[(String, String)]) -> (Vec<&str>, Vec<&str>) {
    keys.iter().map(|(a, m)| (a.as_str(), m.as_str())).unzip()
}

fn done_sql() -> String {
    format!(
        "DELETE FROM mail.jobs WHERE kind = $1 AND account = $2 AND message_id = $3 AND {GEN} = $4"
    )
}

/// A connection for the workers (as `mail_writer`).
pub struct Work {
    client: Client,
    vector_type: String,
    dim: usize,
}

impl Work {
    pub async fn connect(dsn: &str, opts: &ConnectOptions) -> Result<Work> {
        Self::from_client(connect(dsn, opts).await?).await
    }

    /// Checks the schema is at version 2 or later and reads the embedding column.
    pub async fn from_client(client: Client) -> Result<Work> {
        match crate::migrate::current_version(&client).await? {
            None => {
                return Err(Error::Refused(
                    "there is no mail schema in this database: run `lbmail pg migrate`".into(),
                ));
            }
            Some(v) if v < 2 => {
                return Err(Error::Refused(format!(
                    "the mail schema is at version {v}; the workers need version 2: run `lbmail pg migrate`"
                )));
            }
            Some(_) => {}
        }
        let (vector_type, dim) = embedding_column(&client).await?;
        Ok(Work {
            client,
            vector_type,
            dim,
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Dimensions the embedding column holds.
    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    /// Claims up to `n` ready jobs of `kind`, newest first, leased for `lease`.
    pub async fn claim(&self, kind: JobKind, n: i64, lease: Duration) -> Result<Vec<Job>> {
        const SQL: &str = "UPDATE mail.jobs j SET locked_until = now() + make_interval(secs => $3), attempts = j.attempts + 1
             FROM (SELECT kind, account, message_id FROM mail.jobs
                   WHERE kind = $1 AND parked_at IS NULL AND (locked_until IS NULL OR locked_until < now())
                   ORDER BY enqueued_at DESC LIMIT $2 FOR UPDATE SKIP LOCKED) c
             WHERE j.kind = c.kind AND j.account = c.account AND j.message_id = c.message_id
             RETURNING j.account, j.message_id, j.attempts, (extract(epoch FROM j.enqueued_at) * 1000000)::bigint,
                       greatest(extract(epoch FROM now() - j.enqueued_at), 0)::bigint";
        let rows = self
            .client
            .query(SQL, &[&kind.as_str(), &n.max(1), &lease.as_secs_f64()])
            .await
            .map_err(pg_err)?;
        let mut jobs: Vec<Job> = rows
            .iter()
            .map(|r| Job {
                kind,
                account: r.get(0),
                message_id: r.get(1),
                attempts: r.get(2),
                generation: r.get(3),
                age_s: r.get(4),
            })
            .collect();
        jobs.sort_by_key(|j| std::cmp::Reverse(j.generation));
        Ok(jobs)
    }

    /// Done: deletes the job unless it was re-queued meanwhile. True if it was deleted.
    pub async fn done(&self, job: &Job) -> Result<bool> {
        let sql = done_sql();
        let n = self
            .client
            .execute(
                &sql,
                &[
                    &job.kind.as_str(),
                    &job.account,
                    &job.message_id,
                    &job.generation,
                ],
            )
            .await
            .map_err(pg_err)?;
        Ok(n > 0)
    }

    /// Failed: try again after `backoff`, or park it with the error when it has had
    /// `max_attempts`. True if it was parked. The error must not carry mail content.
    pub async fn retry(
        &self,
        job: &Job,
        error: &str,
        backoff: Duration,
        max_attempts: i32,
    ) -> Result<bool> {
        let sql = format!(
            "UPDATE mail.jobs SET last_error = $5, locked_until = now() + make_interval(secs => $6),
                 parked_at = CASE WHEN attempts >= $7 THEN now() END
             WHERE kind = $1 AND account = $2 AND message_id = $3 AND {GEN} = $4
             RETURNING parked_at IS NOT NULL"
        );
        let error: String = error.chars().take(500).collect();
        let row = self
            .client
            .query_opt(
                &sql,
                &[
                    &job.kind.as_str(),
                    &job.account,
                    &job.message_id,
                    &job.generation,
                    &error,
                    &backoff.as_secs_f64(),
                    &max_attempts.max(1),
                ],
            )
            .await
            .map_err(pg_err)?;
        Ok(row.is_some_and(|r| r.get::<_, bool>(0)))
    }

    /// Not now (e.g. waiting for triage): back in `after`, and this try does not count.
    pub async fn defer(&self, job: &Job, after: Duration) -> Result<()> {
        let sql = format!(
            "UPDATE mail.jobs SET locked_until = now() + make_interval(secs => $5), attempts = greatest(attempts - 1, 0)
             WHERE kind = $1 AND account = $2 AND message_id = $3 AND {GEN} = $4"
        );
        self.client
            .execute(
                &sql,
                &[
                    &job.kind.as_str(),
                    &job.account,
                    &job.message_id,
                    &job.generation,
                    &after.as_secs_f64(),
                ],
            )
            .await
            .map_err(pg_err)?;
        Ok(())
    }

    /// What embedding a message needs; None when the message is gone.
    pub async fn embed_source(
        &self,
        account: &str,
        message_id: &str,
    ) -> Result<Option<EmbedSource>> {
        let jobs = [(account.to_string(), message_id.to_string())];
        Ok(self.embed_sources(&jobs).await?.pop().flatten())
    }

    /// [`Work::embed_source`] for several messages in one query, in the order given.
    pub async fn embed_sources(
        &self,
        keys: &[(String, String)],
    ) -> Result<Vec<Option<EmbedSource>>> {
        let (accounts, ids) = split(keys);
        let rows = self
            .client
            .query(
                "SELECT x.ord, m.subject, m.from_name, m.from_email, m.sensitive_kind, b.body_text, b.content_hash,
                        a.ai_policy, a.persona
                 FROM unnest($1::text[], $2::text[]) WITH ORDINALITY AS x(account, message_id, ord)
                 JOIN mail.messages m ON m.account = x.account AND m.id = x.message_id
                 JOIN mail.accounts a ON a.id = m.account
                 LEFT JOIN mail.bodies b ON b.account = m.account AND b.message_id = m.id",
                &[&accounts, &ids],
            )
            .await
            .map_err(pg_err)?;
        let mut out: Vec<Option<EmbedSource>> = vec![None; keys.len()];
        for r in rows {
            let i = r.get::<_, i64>(0) as usize - 1;
            out[i] = Some(EmbedSource {
                subject: r.get(1),
                from_name: r.get(2),
                from_email: r.get(3),
                sensitive_kind: r.get(4),
                body_text: r.get(5),
                content_hash: r.get(6),
                ai_policy: r.get(7),
                persona: r.get(8),
            });
        }
        Ok(out)
    }

    /// [`Work::chunk_hashes`] for several messages in one query, by (account, message id).
    pub async fn chunk_hashes_many(
        &self,
        keys: &[(String, String)],
        model: &str,
    ) -> Result<std::collections::HashMap<(String, String), Vec<(i32, String)>>> {
        let (accounts, ids) = split(keys);
        let rows = self
            .client
            .query(
                "SELECT e.account, e.message_id, e.chunk, e.content_hash FROM mail.embeddings e
                 JOIN unnest($1::text[], $2::text[]) AS x(account, message_id)
                   ON e.account = x.account AND e.message_id = x.message_id
                 WHERE e.model = $3 ORDER BY e.account, e.message_id, e.chunk",
                &[&accounts, &ids, &model],
            )
            .await
            .map_err(pg_err)?;
        let mut out: std::collections::HashMap<(String, String), Vec<(i32, String)>> =
            std::collections::HashMap::new();
        for r in rows {
            out.entry((r.get(0), r.get(1)))
                .or_default()
                .push((r.get(2), r.get(3)));
        }
        Ok(out)
    }

    /// The chunk hashes already embedded for a message with `model`, by chunk.
    pub async fn chunk_hashes(
        &self,
        account: &str,
        message_id: &str,
        model: &str,
    ) -> Result<Vec<(i32, String)>> {
        let rows = self
            .client
            .query(
                "SELECT chunk, content_hash FROM mail.embeddings
                 WHERE account = $1 AND message_id = $2 AND model = $3 ORDER BY chunk",
                &[&account, &message_id, &model],
            )
            .await
            .map_err(pg_err)?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    /// Writes a message's embeddings for `model` in one transaction: chunks beyond `rows` and rows
    /// of other models go (a model change replaces them message by message); unchanged chunks are
    /// not rewritten.
    pub async fn put_embeddings(
        &mut self,
        account: &str,
        message_id: &str,
        model: &str,
        rows: &[EmbeddingRow],
    ) -> Result<()> {
        self.put_embeddings_many(model, &[(account, message_id, None, rows)])
            .await
    }

    /// [`Work::put_embeddings`] for several messages, and each one's job finished (when given), in
    /// one transaction (one commit for a whole request's worth of mail). All or nothing.
    pub async fn put_embeddings_many(
        &mut self,
        model: &str,
        items: &[EmbedItem<'_>],
    ) -> Result<()> {
        let mut literals = Vec::with_capacity(items.len());
        for (_, _, _, rows) in items {
            for r in rows.iter() {
                if r.vector.len() != self.dim {
                    return Err(Error::Invalid(format!(
                        "the embedding has {} dimensions; mail.embeddings holds {}",
                        r.vector.len(),
                        self.dim
                    )));
                }
            }
            literals.push(
                rows.iter()
                    .map(|r| vector_literal(&r.vector))
                    .collect::<Result<Vec<_>>>()?,
            );
        }
        let insert = format!(
            "INSERT INTO mail.embeddings (account, message_id, chunk, model, content_hash, embedding)
             VALUES ($1, $2, $3, $4, $5, $6::text::{})
             ON CONFLICT (account, message_id, model, chunk) DO UPDATE
                 SET content_hash = excluded.content_hash, embedding = excluded.embedding, created_at = now()
             WHERE mail.embeddings.content_hash IS DISTINCT FROM excluded.content_hash",
            self.vector_type
        );
        let tx = self.client.transaction().await.map_err(pg_err)?;
        let stmt = tx.prepare(&insert).await.map_err(pg_err)?;
        let done = tx.prepare(&done_sql()).await.map_err(pg_err)?;
        for ((account, message_id, job, rows), lits) in items.iter().zip(&literals) {
            let n = rows.len() as i32;
            tx.execute(
                "DELETE FROM mail.embeddings WHERE account = $1 AND message_id = $2 AND (model <> $3 OR chunk >= $4)",
                &[account, message_id, &model, &n],
            )
            .await
            .map_err(pg_err)?;
            for (r, lit) in rows.iter().zip(lits) {
                tx.execute(
                    &stmt,
                    &[account, message_id, &r.chunk, &model, &r.content_hash, lit],
                )
                .await
                .map_err(pg_err)?;
            }
            if let Some(job) = job {
                tx.execute(
                    &done,
                    &[
                        &job.kind.as_str(),
                        &job.account,
                        &job.message_id,
                        &job.generation,
                    ],
                )
                .await
                .map_err(pg_err)?;
            }
        }
        tx.commit().await.map_err(pg_err)
    }

    /// Finishes several jobs in one statement; returns how many were deleted (a job re-queued
    /// meanwhile stays).
    pub async fn done_many(&self, jobs: &[Job]) -> Result<u64> {
        if jobs.is_empty() {
            return Ok(0);
        }
        let kinds: Vec<&str> = jobs.iter().map(|j| j.kind.as_str()).collect();
        let accounts: Vec<&str> = jobs.iter().map(|j| j.account.as_str()).collect();
        let ids: Vec<&str> = jobs.iter().map(|j| j.message_id.as_str()).collect();
        let gens: Vec<i64> = jobs.iter().map(|j| j.generation).collect();
        const SQL: &str = "DELETE FROM mail.jobs j USING unnest($1::text[], $2::text[], $3::text[], $4::bigint[]) AS x(kind, account, message_id, g)
             WHERE j.kind = x.kind AND j.account = x.account AND j.message_id = x.message_id
               AND (extract(epoch FROM j.enqueued_at) * 1000000)::bigint = x.g";
        self.client
            .execute(SQL, &[&kinds, &accounts, &ids, &gens])
            .await
            .map_err(pg_err)
    }

    /// What parsing a message needs; None when the message is gone.
    pub async fn parse_source(
        &self,
        account: &str,
        message_id: &str,
    ) -> Result<Option<ParseSource>> {
        let jobs = [(account.to_string(), message_id.to_string())];
        Ok(self.parse_sources(&jobs).await?.pop().flatten())
    }

    /// [`Work::parse_source`] for several messages in one query, in the order given.
    pub async fn parse_sources(
        &self,
        keys: &[(String, String)],
    ) -> Result<Vec<Option<ParseSource>>> {
        let (accounts, ids) = split(keys);
        let rows = self
            .client
            .query(
                "SELECT x.ord, m.thread_id, m.subject, m.snippet, m.from_name, m.from_email,
                        extract(epoch FROM m.sent_at)::bigint, m.sensitive_kind,
                        b.body_text, b.content_hash, coalesce(b.headers, '[]'::jsonb), coalesce(b.structured, '[]'::jsonb),
                        t.category,
                        coalesce((SELECT array_agg(y.mime ORDER BY y.id) FROM mail.attachments y
                                  WHERE y.account = m.account AND y.message_id = m.id), '{}'),
                        a.ai_policy, a.persona
                 FROM unnest($1::text[], $2::text[]) WITH ORDINALITY AS x(account, message_id, ord)
                 JOIN mail.messages m ON m.account = x.account AND m.id = x.message_id
                 JOIN mail.accounts a ON a.id = m.account
                 LEFT JOIN mail.bodies b ON b.account = m.account AND b.message_id = m.id
                 LEFT JOIN mail.triage t ON t.account = m.account AND t.thread_id = m.thread_id",
                &[&accounts, &ids],
            )
            .await
            .map_err(pg_err)?;
        let mut out: Vec<Option<ParseSource>> = vec![None; keys.len()];
        for r in rows {
            let i = r.get::<_, i64>(0) as usize - 1;
            out[i] = Some(ParseSource {
                thread_id: r.get(1),
                subject: r.get(2),
                snippet: r.get(3),
                from_name: r.get(4),
                from_email: r.get(5),
                sent_at: r.get(6),
                sensitive_kind: r.get(7),
                body_text: r.get(8),
                content_hash: r.get(9),
                headers: r.get(10),
                structured: r.get(11),
                category: r.get(12),
                attachment_mimes: r.get(13),
                ai_policy: r.get(14),
                persona: r.get(15),
            });
        }
        Ok(out)
    }

    /// Replaces the workers' facts for a message (parsers under `rules/` and `model/`) with
    /// `facts`, in one transaction. Facts that did not change keep their row and `created_at`.
    pub async fn put_facts(
        &mut self,
        account: &str,
        message_id: &str,
        content_hash: Option<&str>,
        facts: &[FactRow],
    ) -> Result<()> {
        self.put_facts_many(&[(account, message_id, content_hash, None, facts)])
            .await
    }

    /// [`Work::put_facts`] for several messages, and each one's job finished (when given), in one
    /// transaction. All or nothing.
    pub async fn put_facts_many(&mut self, items: &[FactItem<'_>]) -> Result<()> {
        let tx = self.client.transaction().await.map_err(pg_err)?;
        let stmt = tx
            .prepare(
                "INSERT INTO mail.parsed (account, message_id, kind, data, parser, parser_version, confidence, content_hash)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (account, message_id, kind, parser) DO UPDATE
                     SET data = excluded.data, parser_version = excluded.parser_version, confidence = excluded.confidence,
                         content_hash = excluded.content_hash, updated_at = now()
                 WHERE (mail.parsed.data, mail.parsed.parser_version, mail.parsed.confidence, mail.parsed.content_hash)
                    IS DISTINCT FROM (excluded.data, excluded.parser_version, excluded.confidence, excluded.content_hash)",
            )
            .await
            .map_err(pg_err)?;
        let done = tx.prepare(&done_sql()).await.map_err(pg_err)?;
        for (account, message_id, content_hash, job, facts) in items {
            let keep: Vec<String> = facts
                .iter()
                .map(|f| format!("{}\u{1f}{}", f.kind, f.parser))
                .collect();
            tx.execute(
                "DELETE FROM mail.parsed WHERE account = $1 AND message_id = $2
                     AND (parser LIKE 'rules/%' OR parser LIKE 'model/%')
                     AND NOT ((kind || chr(31) || parser) = ANY ($3))",
                &[account, message_id, &keep],
            )
            .await
            .map_err(pg_err)?;
            for f in facts.iter() {
                tx.execute(
                    &stmt,
                    &[
                        account,
                        message_id,
                        &f.kind,
                        &f.data,
                        &f.parser,
                        &f.parser_version,
                        &f.confidence,
                        content_hash,
                    ],
                )
                .await
                .map_err(pg_err)?;
            }
            if let Some(job) = job {
                tx.execute(
                    &done,
                    &[
                        &job.kind.as_str(),
                        &job.account,
                        &job.message_id,
                        &job.generation,
                    ],
                )
                .await
                .map_err(pg_err)?;
            }
        }
        tx.commit().await.map_err(pg_err)
    }

    /// Adds `t` to this hour's counters for `kind`.
    pub async fn note(&self, kind: JobKind, t: &Tally) -> Result<()> {
        if t.is_empty() {
            return Ok(());
        }
        self.client
            .execute(
                "INSERT INTO mail.job_stats (kind, hour, done, unchanged, skipped, failed, parked, calls, items, busy_ms)
                 VALUES ($1, date_trunc('hour', now()), $2, $3, $4, $5, $6, $7, $8, $9)
                 ON CONFLICT (kind, hour) DO UPDATE SET
                     done = mail.job_stats.done + excluded.done, unchanged = mail.job_stats.unchanged + excluded.unchanged,
                     skipped = mail.job_stats.skipped + excluded.skipped, failed = mail.job_stats.failed + excluded.failed,
                     parked = mail.job_stats.parked + excluded.parked, calls = mail.job_stats.calls + excluded.calls,
                     items = mail.job_stats.items + excluded.items, busy_ms = mail.job_stats.busy_ms + excluded.busy_ms",
                &[
                    &kind.as_str(),
                    &t.done,
                    &t.unchanged,
                    &t.skipped,
                    &t.failed,
                    &t.parked,
                    &t.calls,
                    &t.items,
                    &t.busy_ms,
                ],
            )
            .await
            .map_err(pg_err)?;
        Ok(())
    }

    /// Queues work again; returns how many jobs were queued or reset.
    pub async fn requeue(&self, r: &Requeue) -> Result<u64> {
        if r.parked_only {
            return self
                .client
                .execute(
                    "UPDATE mail.jobs SET parked_at = NULL, attempts = 0, last_error = NULL, locked_until = NULL
                     WHERE parked_at IS NOT NULL AND kind = $1 AND ($2::text IS NULL OR account = $2)",
                    &[&r.kind.as_str(), &r.account],
                )
                .await
                .map_err(pg_err);
        }
        // Embed: only what lacks embeddings of `model` (or has another model's), unless `all`.
        let filter = match (r.kind, &r.model, r.all) {
            (JobKind::Embed, Some(_), false) => {
                "AND (NOT EXISTS (SELECT 1 FROM mail.embeddings e WHERE e.account = b.account AND e.message_id = b.message_id AND e.model = $2)
                      OR EXISTS (SELECT 1 FROM mail.embeddings e WHERE e.account = b.account AND e.message_id = b.message_id AND e.model <> $2))"
            }
            _ => "AND ($2::text IS NULL OR true)",
        };
        let sql = format!(
            "INSERT INTO mail.jobs (kind, account, message_id)
             SELECT $1, b.account, b.message_id FROM mail.bodies b
             JOIN mail.messages m ON m.account = b.account AND m.id = b.message_id
             WHERE m.sensitive_kind IS NULL AND ($3::text IS NULL OR b.account = $3) {filter}
             ON CONFLICT (kind, account, message_id) DO UPDATE
                 SET parked_at = NULL, attempts = 0, last_error = NULL, locked_until = NULL"
        );
        self.client
            .execute(&sql, &[&r.kind.as_str(), &r.model, &r.account])
            .await
            .map_err(pg_err)
    }
}

/// What `requeue` queues.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requeue {
    pub kind: JobKind,
    pub account: Option<String>,
    /// Embed: the current model; messages without its embeddings are queued.
    pub model: Option<String>,
    /// Every body, whatever is there already.
    pub all: bool,
    /// Only give parked jobs another chance.
    pub parked_only: bool,
}

/// Both queues' state (any role that may read `mail`).
pub async fn queue_state(client: &Client) -> Result<Vec<QueueState>> {
    let rows = client
        .query(
            "SELECT k.kind,
                    count(j.kind) FILTER (WHERE j.parked_at IS NULL AND (j.locked_until IS NULL OR j.locked_until < now())),
                    count(j.kind) FILTER (WHERE j.parked_at IS NULL AND j.locked_until >= now()),
                    count(j.kind) FILTER (WHERE j.parked_at IS NOT NULL),
                    extract(epoch FROM now() - min(j.enqueued_at) FILTER (WHERE j.parked_at IS NULL))::bigint,
                    (array_agg(j.last_error ORDER BY j.locked_until DESC NULLS LAST) FILTER (WHERE j.last_error IS NOT NULL))[1]
             FROM (VALUES ('embed'), ('parse')) k(kind)
             LEFT JOIN mail.jobs j ON j.kind = k.kind
             GROUP BY k.kind ORDER BY k.kind",
            &[],
        )
        .await
        .map_err(pg_err)?;
    let stats = client
        .query(
            "SELECT kind, w,
                    coalesce(sum(done), 0)::bigint, coalesce(sum(unchanged), 0)::bigint, coalesce(sum(skipped), 0)::bigint,
                    coalesce(sum(failed), 0)::bigint, coalesce(sum(parked), 0)::bigint, coalesce(sum(calls), 0)::bigint,
                    coalesce(sum(items), 0)::bigint, coalesce(sum(busy_ms), 0)::bigint
             FROM (SELECT s.*, w FROM mail.job_stats s
                   CROSS JOIN LATERAL (VALUES ('hour', interval '1 hour'), ('day', interval '1 day'), ('total', NULL::interval)) v(w, span)
                   WHERE v.span IS NULL OR s.hour >= date_trunc('hour', now()) - v.span + interval '1 hour') x
             GROUP BY kind, w",
            &[],
        )
        .await
        .map_err(pg_err)?;
    let mut out: Vec<QueueState> = rows
        .iter()
        .map(|r| QueueState {
            kind: r.get(0),
            ready: r.get(1),
            waiting: r.get(2),
            parked: r.get(3),
            oldest_s: r.get(4),
            last_error: r.get(5),
            ..QueueState::default()
        })
        .collect();
    for r in &stats {
        let kind: String = r.get(0);
        let w: String = r.get(1);
        let t = Tally {
            done: r.get(2),
            unchanged: r.get(3),
            skipped: r.get(4),
            failed: r.get(5),
            parked: r.get(6),
            calls: r.get(7),
            items: r.get(8),
            busy_ms: r.get(9),
        };
        if let Some(q) = out.iter_mut().find(|q| q.kind == kind) {
            match w.as_str() {
                "hour" => q.last_hour = t,
                "day" => q.last_day = t,
                _ => q.total = t,
            }
        }
    }
    Ok(out)
}

/// A hybrid search (`mail.search`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchArgs {
    pub q: String,
    /// None: every account in the mirror.
    pub accounts: Option<Vec<String>>,
    pub limit: i32,
    /// Unix seconds.
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// The query's embedding (same model as the stored ones) and that model's name.
    pub vector: Option<Vec<f32>>,
    pub model: Option<String>,
}

/// One result of [`search`]. Sensitive mail has its one line as subject and no snippet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchRow {
    pub account: String,
    pub message_id: String,
    pub thread_id: String,
    pub sent_at: Option<i64>,
    pub subject: String,
    pub from_email: String,
    pub snippet: String,
    pub sensitive_kind: Option<String>,
    pub score: f64,
    /// `text`, `fuzzy`, `vector`.
    pub matched: Vec<String>,
}

/// Runs `mail.search`.
pub async fn search(client: &Client, a: &SearchArgs) -> Result<Vec<SearchRow>> {
    let (ty, dim) = embedding_column(client).await?;
    let lit = match &a.vector {
        Some(v) if v.len() != dim => {
            return Err(Error::Invalid(format!(
                "the query embedding has {} dimensions; mail.embeddings holds {dim}",
                v.len()
            )));
        }
        Some(v) => Some(vector_literal(v)?),
        None => None,
    };
    let sql = format!(
        "SELECT account, message_id, thread_id, extract(epoch FROM sent_at)::bigint, subject, from_email, snippet,
                sensitive_kind, score, matched
         FROM mail.search($1, $2, $3, to_timestamp($4::bigint), to_timestamp($5::bigint), $6::text::{ty}, $7)"
    );
    let rows = client
        .query(
            &sql,
            &[
                &a.q,
                &a.accounts,
                &a.limit.clamp(1, 500),
                &a.since,
                &a.until,
                &lit,
                &a.model,
            ],
        )
        .await
        .map_err(pg_err)?;
    Ok(rows
        .iter()
        .map(|r| SearchRow {
            account: r.get(0),
            message_id: r.get(1),
            thread_id: r.get(2),
            sent_at: r.get(3),
            subject: r.get(4),
            from_email: r.get(5),
            snippet: r.get(6),
            sensitive_kind: r.get(7),
            score: r.get(8),
            matched: r.get(9),
        })
        .collect())
}

/// Which facts to list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedQuery {
    pub kind: Option<String>,
    /// Mail sent at or after (unix seconds).
    pub since: Option<i64>,
    pub accounts: Option<Vec<String>>,
    pub limit: i64,
}

/// One fact (the best per message and kind: `mail.facts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedRow {
    pub account: String,
    pub message_id: String,
    pub thread_id: String,
    pub sent_at: Option<i64>,
    pub subject: String,
    pub from_email: String,
    pub kind: String,
    pub data: Value,
    pub parser: String,
    pub confidence: Option<f32>,
}

/// Lists facts, newest mail first.
pub async fn parsed(client: &Client, q: &ParsedQuery) -> Result<Vec<ParsedRow>> {
    let rows = client
        .query(
            "SELECT account, message_id, thread_id, extract(epoch FROM sent_at)::bigint, subject, from_email,
                    kind, data, parser, confidence
             FROM mail.facts
             WHERE ($1::text IS NULL OR kind = $1)
               AND ($2::bigint IS NULL OR sent_at >= to_timestamp($2::bigint))
               AND ($3::text[] IS NULL OR account = ANY ($3))
             ORDER BY sent_at DESC NULLS LAST, kind
             LIMIT $4",
            &[&q.kind, &q.since, &q.accounts, &q.limit.clamp(1, 10_000)],
        )
        .await
        .map_err(pg_err)?;
    Ok(rows
        .iter()
        .map(|r| ParsedRow {
            account: r.get(0),
            message_id: r.get(1),
            thread_id: r.get(2),
            sent_at: r.get(3),
            subject: r.get(4),
            from_email: r.get(5),
            kind: r.get(6),
            data: r.get(7),
            parser: r.get(8),
            confidence: r.get(9),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_literals_are_pgvector_text_and_refuse_bad_values() {
        assert_eq!(vector_literal(&[1.0, -0.5, 0.25]).unwrap(), "[1,-0.5,0.25]");
        assert!(vector_literal(&[f32::NAN]).is_err());
        assert!(vector_literal(&[f32::INFINITY]).is_err());
        assert!(vector_literal(&[70_000.0]).is_err(), "beyond halfvec");
    }

    #[test]
    fn tallies_add_up() {
        let mut a = Tally {
            done: 1,
            calls: 2,
            ..Tally::default()
        };
        assert!(!a.is_empty());
        a.add(&Tally {
            done: 2,
            items: 5,
            ..Tally::default()
        });
        assert_eq!((a.done, a.calls, a.items), (3, 2, 5));
        assert!(Tally::default().is_empty());
    }

    #[test]
    fn job_kinds_have_wire_names() {
        assert_eq!(JobKind::Embed.as_str(), "embed");
        assert_eq!(serde_json::to_string(&JobKind::Parse).unwrap(), "\"parse\"");
    }
}
