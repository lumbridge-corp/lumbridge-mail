//! The embedding worker: `embed` jobs to `mail.embeddings`.
//!
//! Per round: claim a batch of jobs; for each, read the message (subject, sender, clean body) and
//! refuse it if a privacy rule says so; cut it into chunks; if the chunks' hashes equal what is
//! stored for this model, finish without a call. The rest go to the endpoint in requests of up to
//! `batch_inputs` texts (a message's chunks stay together), paced by `max_per_minute` and
//! `concurrency`. A failed request retries its jobs with backoff (parked after `max_attempts`); a
//! vector of the wrong size is a configuration error and stops the worker until it is fixed.

use super::{Db, EmbedSection, accounts_of, lock, note_error, pause, refusal};
use crate::engine::{Shared, now};
use futures::StreamExt;
use lumbridge_mail_core::*;
use lumbridge_mail_index::chunk::{Chunk, ChunkOptions, chunks};
use lumbridge_mail_index::{Embedder, InputKind, OpenAiEmbedder, Pacer};
use lumbridge_mail_pg::work::{EmbedItem, EmbeddingRow, Job, JobKind, Tally, Work};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Settings {
    pub db: Db,
    pub section: EmbedSection,
}

/// The embedder for a section: the API key from `api_key_env`.
pub(crate) fn embedder(section: &EmbedSection) -> Result<OpenAiEmbedder> {
    let key = section
        .endpoint
        .api_key_env
        .as_ref()
        .and_then(|v| std::env::var(v).ok());
    OpenAiEmbedder::new(section.endpoint.clone(), key)
}

pub(crate) async fn run(shared: Arc<Shared>, s: Settings) {
    let live = shared.pgwork.embed_live.clone();
    let e = match embedder(&s.section) {
        Ok(e) => Arc::new(e) as Arc<dyn Embedder>,
        Err(e) => {
            eprintln!("lumbridge-maild: embedding worker: {e}");
            note_error(&live, &e.to_string());
            lock(&live).running = false;
            return;
        }
    };
    run_with(shared, s, e).await
}

async fn run_with(shared: Arc<Shared>, s: Settings, embedder: Arc<dyn Embedder>) {
    let live = shared.pgwork.embed_live.clone();
    let sec = &s.section;
    let pacer = Pacer::new(sec.max_per_minute, sec.concurrency.max(1) as usize);
    let max_backoff = Duration::from_secs(sec.retry.max_retry_s.clamp(1, 300));
    let mut backoff = Duration::from_secs(1);
    let mut work: Option<Work> = None;
    let mut logged = false;
    eprintln!(
        "lumbridge-maild: embedding worker on ({})",
        super::endpoint_label(&sec.endpoint)
    );
    loop {
        if shared.is_shutting_down() {
            return;
        }
        if work.as_ref().is_none_or(Work::is_closed) {
            lock(&live).connected = false;
            match s.db.work("lumbridge-maild embed").await {
                Ok(w) => {
                    if let Some(d) = sec.endpoint.dimensions
                        && d as usize != w.dim()
                    {
                        let msg = format!(
                            "[pg.embed] dimensions = {d} but mail.embeddings holds {}; fix the config or migrate",
                            w.dim()
                        );
                        stalled(&shared, &live, &msg, &mut logged).await;
                        continue;
                    }
                    work = Some(w);
                    backoff = Duration::from_secs(1);
                    logged = false;
                    lock(&live).connected = true;
                }
                Err(e) => {
                    if !logged {
                        eprintln!("lumbridge-maild: embedding worker: {e} (retrying)");
                        logged = true;
                    }
                    note_error(&live, &e.to_string());
                    if !pause(&shared, backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(max_backoff);
                    continue;
                }
            }
        }
        let Some(w) = work.as_mut() else { continue };
        let jobs = match w
            .claim(
                JobKind::Embed,
                i64::from(sec.batch_jobs.clamp(1, 1000)),
                Duration::from_secs(sec.retry.lease_s.max(30)),
            )
            .await
        {
            Ok(j) => j,
            Err(e) => {
                note_error(&live, &e.to_string());
                work = None;
                continue;
            }
        };
        if jobs.is_empty() {
            if !pause(
                &shared,
                Duration::from_millis(sec.retry.poll_ms.clamp(50, 60_000)),
            )
            .await
            {
                return;
            }
            continue;
        }
        let started = Instant::now();
        match batch(&shared, w, &s, embedder.as_ref(), &pacer, jobs).await {
            Ok(mut t) => {
                t.busy_ms = started.elapsed().as_millis() as i64;
                if let Err(e) = w.note(JobKind::Embed, &t).await {
                    note_error(&live, &e.to_string());
                }
                let mut l = lock(&live);
                l.last_batch_at = Some(now());
                l.last_batch_jobs = t.done as usize;
                l.last_batch_calls = t.calls as usize;
                l.last_batch_ms = t.busy_ms as u64;
                l.jobs += t.done as u64;
                l.calls += t.calls as u64;
                l.items += t.items as u64;
                if t.failed == 0 {
                    l.last_error = None;
                }
            }
            Err(Stop::Postgres(e)) => {
                // Postgres went away mid-batch: the leases bring the jobs back.
                note_error(&live, &e.to_string());
                work = None;
            }
            Err(Stop::Config(msg)) => {
                stalled(&shared, &live, &msg, &mut logged).await;
                work = None;
            }
            Err(Stop::Endpoint(e)) => {
                // The endpoint is down: its jobs were retried; wait before the next round.
                note_error(&live, &e.to_string());
                if !pause(&shared, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
        }
        backoff = Duration::from_secs(1);
    }
}

/// A configuration problem: say so once, wait long, try again (the config may be fixed).
async fn stalled(
    shared: &Shared,
    live: &std::sync::Mutex<super::WorkerLive>,
    msg: &str,
    logged: &mut bool,
) {
    if !*logged {
        eprintln!("lumbridge-maild: embedding worker stopped: {msg}");
        *logged = true;
    }
    note_error(live, msg);
    let _ = pause(shared, Duration::from_secs(300)).await;
}

pub(crate) enum Stop {
    Postgres(Error),
    Endpoint(Error),
    Config(String),
}

/// A request's messages (indexes into the round's todo list) and its vectors.
type Answer = (Vec<usize>, Result<Vec<Vec<f32>>>);

struct Todo {
    job: Job,
    chunks: Vec<Chunk>,
}

async fn batch(
    shared: &Arc<Shared>,
    w: &mut Work,
    s: &Settings,
    embedder: &dyn Embedder,
    pacer: &Pacer,
    jobs: Vec<Job>,
) -> std::result::Result<Tally, Stop> {
    let sec = &s.section;
    let model = embedder.model().to_string();
    let opts = ChunkOptions {
        chars: sec.chunk_chars.clamp(500, 32_000) as usize,
        overlap: sec.chunk_overlap as usize,
        max_chunks: sec.max_chunks.clamp(1, 64) as usize,
    };
    let mut t = Tally::default();
    let ids: Vec<String> = jobs.iter().map(|j| j.account.clone()).collect();
    let accounts = accounts_of(shared, ids).await.map_err(Stop::Postgres)?;
    let pg = |e: Error| Stop::Postgres(e);
    let mut todo: Vec<Todo> = Vec::new();
    // Finished without a call (refused, gone, unchanged): deleted together.
    let mut finished: Vec<Job> = Vec::new();
    let keys: Vec<(String, String)> = jobs
        .iter()
        .map(|j| (j.account.clone(), j.message_id.clone()))
        .collect();
    let sources = w.embed_sources(&keys).await.map_err(pg)?;
    let stored = w.chunk_hashes_many(&keys, &model).await.map_err(pg)?;
    for (job, src) in jobs.into_iter().zip(sources) {
        let Some(src) = src else {
            finished.push(job);
            t.done += 1;
            t.skipped += 1;
            continue;
        };
        let account = accounts.get(&job.account).and_then(Option::as_ref);
        if refusal(
            account,
            &src.persona,
            &src.ai_policy,
            src.sensitive_kind.as_deref(),
            Some(embedder.place()),
        )
        .is_some()
        {
            finished.push(job);
            t.done += 1;
            t.skipped += 1;
            continue;
        }
        let Some(body) = src.body_text.as_deref() else {
            finished.push(job);
            t.done += 1;
            t.skipped += 1;
            continue;
        };
        let cs = chunks(
            &src.subject,
            src.from_name.as_deref(),
            &src.from_email,
            body,
            &opts,
        );
        let have = stored
            .get(&(job.account.clone(), job.message_id.clone()))
            .cloned()
            .unwrap_or_default();
        let want: Vec<(i32, String)> = cs.iter().map(|c| (c.index, c.hash.clone())).collect();
        if have == want {
            finished.push(job);
            t.done += 1;
            t.unchanged += 1;
            continue;
        }
        todo.push(Todo { job, chunks: cs });
    }
    w.done_many(&finished).await.map_err(pg)?;
    // Requests: whole messages, up to batch_inputs texts each.
    let cap = sec.batch_inputs.clamp(1, 512) as usize;
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut n = 0usize;
    for (i, td) in todo.iter().enumerate() {
        if groups.is_empty() || n + td.chunks.len() > cap {
            groups.push(Vec::new());
            n = 0;
        }
        n += td.chunks.len();
        groups.last_mut().expect("pushed").push(i);
    }
    let todo_ref = &todo;
    let answers: Vec<Answer> = futures::stream::iter(groups)
        .map(|g| async move {
            let inputs: Vec<String> = g
                .iter()
                .flat_map(|&i| todo_ref[i].chunks.iter().map(|c| c.text.clone()))
                .collect();
            let _permit = pacer.start().await;
            let r = embedder.embed(&inputs, InputKind::Passage).await;
            (g, r)
        })
        .buffered(sec.concurrency.clamp(1, 16) as usize)
        .collect()
        .await;
    let mut down: Option<Error> = None;
    for (g, r) in answers {
        t.calls += 1;
        match r {
            Ok(vectors) => {
                if let Some(v) = vectors.iter().find(|v| v.len() != w.dim()) {
                    let msg = format!(
                        "the embedding model returned {} dimensions; mail.embeddings holds {} (lbmail pg migrate --embedding-dim, or another model)",
                        v.len(),
                        w.dim()
                    );
                    return Err(Stop::Config(msg));
                }
                let mut it = vectors.into_iter();
                let rows: Vec<Vec<EmbeddingRow>> = g
                    .iter()
                    .map(|&i| {
                        todo[i]
                            .chunks
                            .iter()
                            .map(|c| EmbeddingRow {
                                chunk: c.index,
                                content_hash: c.hash.clone(),
                                vector: it.next().unwrap_or_default(),
                            })
                            .collect()
                    })
                    .collect();
                // The whole request's mail and its jobs in one transaction; if Postgres refuses
                // one (it turned sensitive or went away meanwhile), message by message.
                let items: Vec<EmbedItem<'_>> = g
                    .iter()
                    .zip(&rows)
                    .map(|(&i, r)| {
                        let j = &todo[i].job;
                        (
                            j.account.as_str(),
                            j.message_id.as_str(),
                            Some(j),
                            r.as_slice(),
                        )
                    })
                    .collect();
                match w.put_embeddings_many(&model, &items).await {
                    Ok(()) => {
                        t.done += items.len() as i64;
                        t.items += rows.iter().map(Vec::len).sum::<usize>() as i64;
                    }
                    Err(e) if lumbridge_mail_pg::is_refusal(&e) => {
                        for item in &items {
                            match w
                                .put_embeddings_many(&model, std::slice::from_ref(item))
                                .await
                            {
                                Ok(()) => {
                                    t.done += 1;
                                    t.items += item.3.len() as i64;
                                }
                                Err(e) if lumbridge_mail_pg::is_refusal(&e) => {
                                    if let Some(job) = item.2 {
                                        retry(w, job, &e.to_string(), sec, &mut t)
                                            .await
                                            .map_err(pg)?;
                                    }
                                }
                                Err(e) => return Err(Stop::Postgres(e)),
                            }
                        }
                    }
                    Err(e) => return Err(Stop::Postgres(e)),
                }
            }
            Err(e) => {
                for i in g {
                    retry(w, &todo[i].job, &e.to_string(), sec, &mut t)
                        .await
                        .map_err(pg)?;
                }
                if e.is_retryable() || matches!(e, Error::Auth(_)) {
                    down.get_or_insert(e);
                }
            }
        }
    }
    if let Some(e) = down {
        let _ = w.note(JobKind::Embed, &t).await;
        return Err(Stop::Endpoint(e));
    }
    Ok(t)
}

async fn retry(w: &Work, job: &Job, err: &str, sec: &EmbedSection, t: &mut Tally) -> Result<()> {
    let parked = w
        .retry(
            job,
            err,
            sec.retry.backoff(job.attempts),
            sec.retry.max_attempts.max(1) as i32,
        )
        .await?;
    t.failed += 1;
    if parked {
        t.parked += 1;
        eprintln!(
            "lumbridge-maild: embedding worker: parked {}/{} after {} attempts",
            job.account, job.message_id, job.attempts
        );
    }
    Ok(())
}
