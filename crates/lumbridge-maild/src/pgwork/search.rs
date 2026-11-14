//! `search.semantic`: embed the question with `[pg.embed]`'s endpoint and run `mail.search` with the
//! vector, fused with full text and fuzzy matching. When it cannot (the mirror is off, Postgres or
//! the embedding endpoint does not answer, or a named account is not in Postgres: anon accounts and
//! `mirror = false`), it says why and the caller searches the local store instead.

use super::Db;
use crate::engine::Shared;
use lumbridge_mail_core::*;
use lumbridge_mail_index::{Embedder, InputKind};
use lumbridge_mail_pg::work::{SearchArgs, SearchRow};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SemanticParams {
    pub q: String,
    #[serde(default)]
    pub accounts: Option<Vec<AccountId>>,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Unix seconds.
    #[serde(default)]
    pub since: Option<i64>,
    #[serde(default)]
    pub until: Option<i64>,
}

/// The answer: hits from Postgres, or the local store's threads with why.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SemanticResult {
    /// `postgres` (hybrid, with vectors when `vector` is true) or `local` (the store's full text).
    pub mode: String,
    pub vector: bool,
    /// Why it is not a full semantic search, if it is not.
    pub note: Option<String>,
    pub hits: Vec<SearchRow>,
    /// The matching threads from the local store, best first, each once: in `local` mode the store's
    /// own search; in `postgres` mode the hits' threads (placed by message, so a thread the store has
    /// regrouped since is the current one), as the caller may see them (sensitive mail as its one line
    /// for agents).
    pub threads: Option<lumbridge_mail_core::rpc::ThreadPage>,
}

const TIMEOUT: Duration = Duration::from_secs(10);

/// The Postgres search, or (Err) why the local store must answer instead.
pub(crate) async fn semantic(
    shared: &Arc<Shared>,
    p: &SemanticParams,
) -> Result<std::result::Result<SemanticResult, String>> {
    if p.q.trim().is_empty() {
        return Err(Error::Invalid("give a question to search for".into()));
    }
    let cfg = shared
        .config
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(db) = Db::from_config(&cfg).filter(|_| cfg.pg.enabled) else {
        return Ok(Err(
            "the Postgres mirror is off ([pg] enabled = false)".into()
        ));
    };
    if let Some(named) = &p.accounts {
        let named = named.clone();
        let excluded: Vec<AccountId> = cfg
            .accounts
            .iter()
            .filter(|a| !a.mirror)
            .map(|a| a.account_id())
            .collect();
        let missing: Vec<String> = shared
            .db(move |s| {
                let mut out = Vec::new();
                for id in &named {
                    match s.account(id)? {
                        Some(a) if a.persona != Persona::Anon && !excluded.contains(id) => {}
                        _ => out.push(id.0.clone()),
                    }
                }
                Ok(out)
            })
            .await?;
        if !missing.is_empty() {
            return Ok(Err(format!(
                "{} not in Postgres (anon, mirror = false or unknown)",
                missing.join(", ")
            )));
        }
    }
    let mut note = None;
    let mut model = None;
    let vector = if cfg.pg.embed.endpoint.is_configured() {
        let e = super::embed::embedder(&cfg.pg.embed)?;
        let v = tokio::time::timeout(
            TIMEOUT,
            e.embed(std::slice::from_ref(&p.q), InputKind::Query),
        )
        .await;
        match v {
            Ok(Ok(mut v)) if v.len() == 1 => {
                model = Some(e.model().to_string());
                v.pop()
            }
            Ok(Ok(_)) => return Ok(Err("the embedding endpoint gave no vector".into())),
            Ok(Err(e)) => {
                return Ok(Err(format!(
                    "the embedding endpoint is not answering ({e})"
                )));
            }
            Err(_) => return Ok(Err("the embedding endpoint timed out".into())),
        }
    } else {
        note = Some(
            "no embedding endpoint configured ([pg.embed] base_url, model): text match only"
                .to_string(),
        );
        None
    };
    let args = SearchArgs {
        q: p.q.clone(),
        accounts: p
            .accounts
            .as_ref()
            .map(|v| v.iter().map(|a| a.0.clone()).collect()),
        limit: p.limit.unwrap_or(20).clamp(1, 500) as i32,
        since: p.since,
        until: p.until,
        vector,
        model,
    };
    let has_vector = args.vector.is_some();
    let hits = tokio::time::timeout(TIMEOUT, async {
        let c = db.connect("lumbridge-maild search").await?;
        lumbridge_mail_pg::work::search(&c, &args).await
    })
    .await;
    match hits {
        Ok(Ok(hits)) => Ok(Ok(SemanticResult {
            mode: "postgres".into(),
            vector: has_vector,
            note,
            hits,
            threads: None,
        })),
        Ok(Err(e)) => Ok(Err(format!("the Postgres search failed ({e})"))),
        Err(_) => Ok(Err("Postgres timed out".into())),
    }
}
