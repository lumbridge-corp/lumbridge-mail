//! `lbmail` commands for the workers and semantic search: `search --semantic`, `parsed`,
//! `pg status` (the workers' part), `pg reembed`, `pg reparse`, `pg requeue --parked`.

use super::search::SemanticResult;
use super::{
    METHOD_PARSED, METHOD_REQUEUE, METHOD_SEMANTIC, METHOD_STATUS, WorkStatus, WorkerLive,
};
use crate::cli::when;
use crate::client::Client;
use lumbridge_mail_core::*;
use lumbridge_mail_pg::work::{ParsedRow, Tally};
use serde_json::{Value, json};

fn print_json(v: &impl serde::Serialize) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

/// `YYYY-MM-DD` (UTC) to unix seconds.
pub(crate) fn day(s: &str) -> Result<i64> {
    lumbridge_mail_index::fact::day_of(s)
        .filter(|_| s.len() == 10)
        .map(|d| d * 86_400)
        .ok_or_else(|| Error::Invalid(format!("{s:?}: give a date as YYYY-MM-DD")))
}

/// `lbmail search --semantic`.
pub(crate) async fn semantic(
    c: &mut Client,
    json_out: bool,
    q: String,
    accounts: Option<Vec<AccountId>>,
    limit: u32,
) -> Result<()> {
    let r: SemanticResult = c
        .call(
            METHOD_SEMANTIC,
            json!({ "q": q, "accounts": accounts, "limit": limit }),
        )
        .await?;
    if json_out {
        print_json(&r);
        return Ok(());
    }
    if let Some(n) = &r.note {
        eprintln!(
            "lbmail: {}{n}",
            if r.mode == "local" {
                "searched the local store instead: "
            } else {
                ""
            }
        );
    }
    if r.mode == "local" {
        let threads = r.threads.map(|p| p.threads).unwrap_or_default();
        if threads.is_empty() {
            println!("(nothing)");
        }
        for t in threads {
            println!("  {}/{}  {}  {}", t.account, t.id, when(t.date), t.subject);
        }
        return Ok(());
    }
    if r.hits.is_empty() {
        println!("(nothing)");
        return Ok(());
    }
    // One line per matching thread (as `lbmail ls` / `search` print them), with how it matched; then
    // hits whose mail the local store no longer has.
    let threads = r.threads.map(|p| p.threads).unwrap_or_default();
    for t in &threads {
        let matched: std::collections::BTreeSet<&str> = r
            .hits
            .iter()
            .filter(|h| h.account == t.account.as_str() && h.thread_id == t.id.as_str())
            .flat_map(|h| h.matched.iter().map(String::as_str))
            .collect();
        let m = matched.into_iter().collect::<Vec<_>>().join(",");
        println!("{}  [{m}]", crate::cli::row(t));
    }
    for h in r.hits.iter().filter(|h| !threads.iter().any(|t| t.account.as_str() == h.account && t.id.as_str() == h.thread_id)) {
        println!(
            "  {}/{}  {}  {:<18.18}  {}  [{}] (not in the local store)",
            h.account,
            h.thread_id,
            h.sent_at.map(when).unwrap_or_else(|| "-".repeat(16)),
            h.from_email,
            h.subject,
            h.matched.join(",")
        );
    }
    Ok(())
}

/// One line per fact: the fields that matter for its kind.
fn describe(r: &ParsedRow) -> String {
    let d = &r.data;
    let s = |k: &str| {
        d.get(k)
            .and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| v.as_f64().map(|f| format!("{f:.2}")))
            })
            .unwrap_or_default()
    };
    let money = |a: &str, c: &str| {
        let (a, c) = (s(a), s(c));
        if a.is_empty() {
            String::new()
        } else {
            format!("{a} {c}").trim().to_string()
        }
    };
    let parts: Vec<String> = match r.kind.as_str() {
        "receipt" => vec![
            s("merchant"),
            money("total", "currency"),
            s("date"),
            s("order_id"),
        ],
        "travel" => vec![
            s("mode"),
            s("carrier"),
            s("number"),
            format!("{} -> {}", s("from"), s("to")),
            s("depart"),
            s("confirmation"),
        ],
        "package" => vec![s("carrier"), s("tracking_id"), s("status"), s("expected")],
        "subscription" => vec![
            s("service"),
            s("event"),
            money("amount", "currency"),
            s("period"),
            s("renews_on"),
        ],
        "recruiting" => vec![s("company"), s("role"), s("recruiter"), s("stage")],
        "bill" => vec![s("payee"), money("amount", "currency"), s("due_date")],
        "event" => vec![s("what"), s("when"), s("where")],
        "newsletter" => vec![
            s("list_id"),
            if d.get("one_click").and_then(Value::as_bool) == Some(true) {
                "one-click unsubscribe".into()
            } else {
                String::new()
            },
        ],
        _ => vec![d.to_string()],
    };
    parts
        .into_iter()
        .filter(|p| !p.trim().is_empty() && p.trim() != "->")
        .collect::<Vec<_>>()
        .join("  ")
}

/// `lbmail parsed`.
pub(crate) async fn parsed(
    c: &mut Client,
    json_out: bool,
    kind: Option<String>,
    since: Option<String>,
    accounts: Option<Vec<AccountId>>,
    limit: i64,
) -> Result<()> {
    let since = since.as_deref().map(day).transpose()?;
    let rows: Vec<ParsedRow> = c
        .call(
            METHOD_PARSED,
            json!({ "kind": kind, "since": since, "accounts": accounts, "limit": limit }),
        )
        .await?;
    if json_out {
        print_json(&rows);
        return Ok(());
    }
    if rows.is_empty() {
        println!("(nothing)");
    }
    for r in rows {
        println!(
            "{:<12} {}/{}  {}  {}  ({}, {:.2})",
            r.kind,
            r.account,
            r.thread_id,
            r.sent_at.map(when).unwrap_or_else(|| "-".repeat(16)),
            describe(&r),
            r.parser,
            r.confidence.unwrap_or(0.0)
        );
    }
    Ok(())
}

fn worker_line(name: &str, w: &WorkerLive) -> String {
    if !w.enabled {
        return format!("{name}: off");
    }
    let mut s = format!(
        "{name}: on, {}{}",
        if w.running { "running" } else { "not running" },
        if w.connected {
            ", connected"
        } else {
            ", not connected"
        }
    );
    if let Some(e) = &w.endpoint {
        s.push_str(&format!(" ({e})"));
    }
    if w.jobs > 0 || w.calls > 0 {
        s.push_str(&format!(
            "; since start {} jobs, {} calls, {} rows",
            w.jobs, w.calls, w.items
        ));
        if w.rejected > 0 {
            s.push_str(&format!(", {} model answers rejected", w.rejected));
        }
    }
    if let Some(t) = w.last_batch_at {
        s.push_str(&format!(
            "; last round {} jobs / {} calls in {} ms at {}",
            w.last_batch_jobs,
            w.last_batch_calls,
            w.last_batch_ms,
            when(t)
        ));
    }
    if let Some(e) = &w.last_error {
        s.push_str(&format!(
            "\n  last error ({}): {e}",
            w.last_error_at.map(when).unwrap_or_default()
        ));
    }
    s
}

fn tally_line(t: &Tally) -> String {
    let rate = if t.busy_ms > 0 {
        format!(
            ", {:.1} jobs/s busy",
            t.done as f64 * 1000.0 / t.busy_ms as f64
        )
    } else {
        String::new()
    };
    format!(
        "{} done ({} unchanged, {} skipped by a privacy rule), {} failed, {} parked, {} calls, {} rows{rate}",
        t.done, t.unchanged, t.skipped, t.failed, t.parked, t.calls, t.items
    )
}

/// The workers' part of `lbmail pg status` (nothing when the daemon does not know the method).
pub(crate) async fn status(c: &mut Client, json_out: bool) -> Result<Option<WorkStatus>> {
    let st: WorkStatus = match c.call(METHOD_STATUS, json!({})).await {
        Ok(s) => s,
        Err(Error::Unsupported(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    if json_out {
        return Ok(Some(st));
    }
    println!("{}", worker_line("embedding worker", &st.embed));
    println!("{}", worker_line("parsing worker", &st.parse));
    if let Some(m) = &st.embed_model {
        println!("embedding model: {m}");
    }
    match (&st.queues, &st.queues_error) {
        (Some(qs), _) => {
            for q in qs {
                println!(
                    "queue {}: {} ready, {} leased or waiting to retry, {} parked{}",
                    q.kind,
                    q.ready,
                    q.waiting,
                    q.parked,
                    q.oldest_s
                        .map(|s| format!(" (oldest {s} s)"))
                        .unwrap_or_default()
                );
                if let Some(e) = &q.last_error {
                    println!("  last job error: {e}");
                }
                println!("  this hour: {}", tally_line(&q.last_hour));
                println!("  24 hours:  {}", tally_line(&q.last_day));
                println!("  all time:  {}", tally_line(&q.total));
            }
        }
        (None, Some(e)) => println!("queues: {e}"),
        _ => {}
    }
    Ok(Some(st))
}

/// `lbmail pg reembed | reparse | requeue --parked`.
pub(crate) async fn requeue(
    c: &mut Client,
    json_out: bool,
    kind: &str,
    account: Option<String>,
    all: bool,
    parked_only: bool,
) -> Result<()> {
    let r: Value = c
        .call(
            METHOD_REQUEUE,
            json!({ "kind": kind, "account": account, "all": all, "parked_only": parked_only }),
        )
        .await?;
    if json_out {
        print_json(&r);
    } else {
        println!("queued {} {kind} job(s)", r["queued"]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_read_as_one_line() {
        let r = ParsedRow {
            account: "personal".into(),
            message_id: "m1".into(),
            thread_id: "t1".into(),
            sent_at: Some(0),
            subject: "Your receipt".into(),
            from_email: "receipts@beanco.example".into(),
            kind: "receipt".into(),
            data: json!({"merchant": "Bean Co", "total": 4.2, "currency": "USD", "order_id": "A-1042"}),
            parser: "rules/jsonld".into(),
            confidence: Some(0.95),
        };
        assert_eq!(describe(&r), "Bean Co  4.20 USD  A-1042");
        assert_eq!(day("2026-11-10").unwrap(), 20767 * 86_400);
        assert!(day("2026-11-1").is_err() && day("soon").is_err());
    }
}
