//! The parsing worker: `parse` jobs to `mail.parsed`. Rules first, model second.
//!
//! Per job: read the message (headers, structured data, clean body, the thread's triage category)
//! and refuse it if a privacy rule says so (AI off: not even the rules). Run the rules. Then, if the
//! account may use the `[ai]` endpoint (`safety::ai_allowed`), the mail does not talk to an
//! assistant, and the category and words point at a kind the rules did not find, ask the model for
//! that one kind (paced). Validated facts replace the workers' earlier facts for the message. A
//! message whose thread is not triaged yet waits up to `triage_wait_s` (the category decides
//! whether a model is worth asking).

use super::{Db, ParseSection, accounts_of, lock, note_error, pause, refusal};
use crate::engine::{Shared, now};
use lumbridge_mail_ai::{Model, OpenAiModel};
use lumbridge_mail_core::*;
use lumbridge_mail_index::fact::MailInput;
use lumbridge_mail_index::model::{ModelOptions, Outcome, addresses_assistant, candidate, extract};
use lumbridge_mail_index::{FactKind, PARSER_VERSION, Pacer, rules};
use lumbridge_mail_pg::work::{FactItem, FactRow, Job, JobKind, ParseSource, Tally, Work};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Settings {
    pub db: Db,
    pub section: ParseSection,
    pub ai_enabled: bool,
    /// The `[ai]` endpoint as configured (a change restarts the worker).
    pub ai_endpoint: String,
}

/// The chat model the extractors use: the test override, else the `[ai]` endpoint.
fn model(shared: &Shared, s: &Settings) -> Option<Arc<dyn Model>> {
    if !s.section.model {
        return None;
    }
    if let Some(m) = &shared.model_override {
        return Some(m.clone());
    }
    if !s.ai_enabled {
        return None;
    }
    let cfg = shared
        .config
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut endpoint = cfg.ai.endpoint.clone().with_env();
    if let Some(var) = &cfg.ai.api_key_env {
        endpoint.api_key = std::env::var(var).ok().filter(|k| !k.is_empty());
    }
    match OpenAiModel::new(endpoint) {
        Ok(m) => Some(Arc::new(m)),
        Err(e) => {
            eprintln!(
                "lumbridge-maild: parsing worker: the AI endpoint is not usable ({e}); rules only"
            );
            None
        }
    }
}

pub(crate) async fn run(shared: Arc<Shared>, s: Settings) {
    let live = shared.pgwork.parse_live.clone();
    let sec = &s.section;
    let model = model(&shared, &s);
    let pacer = Pacer::new(sec.max_per_minute, sec.concurrency.max(1) as usize);
    let max_backoff = Duration::from_secs(sec.retry.max_retry_s.clamp(1, 300));
    let mut backoff = Duration::from_secs(1);
    let mut work: Option<Work> = None;
    let mut logged = false;
    eprintln!(
        "lumbridge-maild: parsing worker on ({})",
        model
            .as_ref()
            .map(|m| format!("rules + model {}", m.label()))
            .unwrap_or_else(|| "rules only".into())
    );
    loop {
        if shared.is_shutting_down() {
            return;
        }
        if work.as_ref().is_none_or(Work::is_closed) {
            lock(&live).connected = false;
            match s.db.work("lumbridge-maild parse").await {
                Ok(w) => {
                    work = Some(w);
                    backoff = Duration::from_secs(1);
                    logged = false;
                    lock(&live).connected = true;
                }
                Err(e) => {
                    if !logged {
                        eprintln!("lumbridge-maild: parsing worker: {e} (retrying)");
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
                JobKind::Parse,
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
        let mut rejected = 0u64;
        match batch(
            &shared,
            w,
            sec,
            model.as_deref(),
            &pacer,
            jobs,
            &mut rejected,
        )
        .await
        {
            Ok((mut t, endpoint_down)) => {
                t.busy_ms = started.elapsed().as_millis() as i64;
                if let Err(e) = w.note(JobKind::Parse, &t).await {
                    note_error(&live, &e.to_string());
                }
                {
                    let mut l = lock(&live);
                    l.last_batch_at = Some(now());
                    l.last_batch_jobs = t.done as usize;
                    l.last_batch_calls = t.calls as usize;
                    l.last_batch_ms = t.busy_ms as u64;
                    l.jobs += t.done as u64;
                    l.calls += t.calls as u64;
                    l.items += t.items as u64;
                    l.rejected += rejected;
                    if t.failed == 0 {
                        l.last_error = None;
                    }
                }
                if let Some(e) = endpoint_down {
                    note_error(&live, &e.to_string());
                    if !pause(&shared, backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(max_backoff);
                    continue;
                }
            }
            Err(e) => {
                note_error(&live, &e.to_string());
                work = None;
            }
        }
        backoff = Duration::from_secs(1);
    }
}

/// What the extractors read, from what Postgres holds.
pub(crate) fn input_of(src: &ParseSource) -> MailInput {
    MailInput {
        subject: src.subject.clone(),
        from_email: src.from_email.clone(),
        from_name: src.from_name.clone(),
        sent_at: src.sent_at.unwrap_or(0),
        body_text: src.body_text.clone().unwrap_or_default(),
        headers: MailInput::headers_from_json(&src.headers),
        structured: src.structured.as_array().cloned().unwrap_or_default(),
        attachment_mimes: src.attachment_mimes.clone(),
        category: src.category.clone(),
    }
}

/// One round. Returns the tally and, when the model endpoint was unreachable, its error (its jobs
/// were retried; the worker waits before the next round). `Err` = Postgres trouble.
async fn batch(
    shared: &Arc<Shared>,
    w: &mut Work,
    sec: &ParseSection,
    model: Option<&dyn Model>,
    pacer: &Pacer,
    jobs: Vec<Job>,
    rejected: &mut u64,
) -> Result<(Tally, Option<Error>)> {
    let mut t = Tally::default();
    let ids: Vec<String> = jobs.iter().map(|j| j.account.clone()).collect();
    let accounts = accounts_of(shared, ids).await?;
    let opts = ModelOptions {
        max_chars: sec.max_chars.clamp(500, 50_000) as usize,
        min_confidence_pct: sec.min_confidence_pct.min(100),
    };
    let mut down: Option<Error> = None;
    // Refused or gone: deleted together. Parsed: written together with their jobs.
    let mut finished: Vec<Job> = Vec::new();
    let mut parsed: Vec<(Job, Option<String>, Vec<FactRow>)> = Vec::new();
    let keys: Vec<(String, String)> = jobs
        .iter()
        .map(|j| (j.account.clone(), j.message_id.clone()))
        .collect();
    let sources = w.parse_sources(&keys).await?;
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
            None,
        )
        .is_some()
            || src.body_text.is_none()
        {
            finished.push(job);
            t.done += 1;
            t.skipped += 1;
            continue;
        }
        if src.category.is_none() && job.age_s < sec.triage_wait_s as i64 {
            let left = (sec.triage_wait_s as i64 - job.age_s).clamp(5, 120);
            w.defer(&job, Duration::from_secs(left as u64)).await?;
            continue;
        }
        let input = input_of(&src);
        let mut facts = rules::extract(&input);
        let have: Vec<FactKind> = facts.iter().map(|f| f.kind).collect();
        let usable = model.filter(|m| {
            refusal(account, &src.persona, &src.ai_policy, None, Some(m.place())).is_none()
                && !addresses_assistant(&input)
        });
        let ask = usable.zip(candidate(&input, &have));
        if ask.is_some() && down.is_some() {
            // The endpoint just failed: this one waits for it rather than being parsed without it.
            w.defer(&job, sec.retry.backoff(1)).await?;
            continue;
        }
        if let Some((m, kind)) = ask {
            let _permit = pacer.start().await;
            t.calls += 1;
            match extract(m, &input, kind, &opts).await {
                Ok(Outcome::Found(f)) => facts.push(f),
                Ok(Outcome::NotFound) => {}
                Ok(Outcome::Rejected(_)) => *rejected += 1,
                Err(e) => {
                    let parked = w
                        .retry(
                            &job,
                            &e.to_string(),
                            sec.retry.backoff(job.attempts),
                            sec.retry.max_attempts.max(1) as i32,
                        )
                        .await?;
                    t.failed += 1;
                    if parked {
                        t.parked += 1;
                        eprintln!(
                            "lumbridge-maild: parsing worker: parked {}/{} after {} attempts",
                            job.account, job.message_id, job.attempts
                        );
                    }
                    if e.is_retryable() || matches!(e, Error::Auth(_)) {
                        down.get_or_insert(e);
                    }
                    continue;
                }
            }
        }
        let rows: Vec<FactRow> = facts
            .iter()
            .map(|f| FactRow {
                kind: f.kind.as_str().into(),
                data: f.data.clone(),
                parser: f.parser.clone(),
                parser_version: PARSER_VERSION.into(),
                confidence: f.confidence,
            })
            .collect();
        parsed.push((job, src.content_hash.clone(), rows));
    }
    w.done_many(&finished).await?;
    let items: Vec<FactItem<'_>> = parsed
        .iter()
        .map(|(j, h, rows)| {
            (
                j.account.as_str(),
                j.message_id.as_str(),
                h.as_deref(),
                Some(j),
                rows.as_slice(),
            )
        })
        .collect();
    match w.put_facts_many(&items).await {
        Ok(()) => {
            t.done += items.len() as i64;
            t.items += parsed.iter().map(|p| p.2.len()).sum::<usize>() as i64;
        }
        Err(e) if lumbridge_mail_pg::is_refusal(&e) => {
            // One was refused (turned sensitive meanwhile): message by message.
            for item in &items {
                match w.put_facts_many(std::slice::from_ref(item)).await {
                    Ok(()) => {
                        t.done += 1;
                        t.items += item.4.len() as i64;
                    }
                    Err(e) if lumbridge_mail_pg::is_refusal(&e) => {
                        if let Some(job) = item.3 {
                            let parked = w
                                .retry(
                                    job,
                                    &e.to_string(),
                                    sec.retry.backoff(job.attempts),
                                    sec.retry.max_attempts.max(1) as i32,
                                )
                                .await?;
                            t.failed += 1;
                            t.parked += i64::from(parked);
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        Err(e) => return Err(e),
    }
    Ok((t, down))
}
