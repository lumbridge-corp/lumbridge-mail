//! The background body fill (`[daemon] body_fill`): fetches the bodies of stored mail that has none,
//! slowly and newest first, so embeddings, semantic search and parsed facts cover all mail and not only
//! what was opened or triaged.
//!
//! Polite by construction (a past run of per-message fetches overloaded a small gog host):
//! - One task per account, one fetch at a time, at most `body_fill_per_min` a minute, evenly spaced.
//! - Lowest priority. A fetch starts only when nothing else of the account is talking to its provider
//!   (no sync, open, triage fetch or action in flight, none for [`QUIET`]), the account is online and
//!   its first sync is done, and the provider's own budget is idle and less than half used this minute
//!   (gog: processes per host; Zoho: calls per sign-in), with no backoff or open circuit breaker. So
//!   whatever a person, sync or triage asks for finds the budget free.
//! - Every fetch still goes through the provider's own gate (gog's process budget, Zoho's calls per
//!   minute, Resend's requests per second, the IMAP connection) and the pause gate: `lbmail pause`
//!   stops it at once.
//! - The provider's thread bundle is used (gog fetches the whole thread in one call; the other bodies are
//!   cached too, and drop off the work list).
//! - What comes next is the store's (`Store::body_fill_next`): drafts, mail that has a body and mail set
//!   aside as not fetchable are left out, and the walk's marks are in the database, so a restart goes on
//!   where it stopped.
//!
//! Sensitive mail: bodies are fetched into the local store as when a person opens it (today's rule);
//! the mirror and the workers keep them out of Postgres and away from models (tests in
//! `tests/pg_work.rs`).

use crate::engine::{AccountRt, Shared, now};
use lumbridge_mail_core::rpc::{BodyFillParams, BodyFillStatus};
use lumbridge_mail_core::*;
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};

/// How long an account's provider must have been left alone before the fill uses it.
pub(crate) const QUIET: Duration = Duration::from_secs(2);
/// After the walk is done: look for new mail this often, and walk everything again once a day (mail
/// that arrived below the marks, e.g. backfilled older mail).
const IDLE: Duration = Duration::from_secs(600);
const REWALK_S: i64 = 86_400;

/// The fill's run-time state for one account (in `AccountRt`).
#[derive(Default)]
pub(crate) struct FillRt {
    pub wake: Notify,
    live: Mutex<Live>,
}

#[derive(Default)]
struct Live {
    /// Fetch starts in the last hour.
    recent: VecDeque<Instant>,
    waiting: Option<String>,
}

impl FillRt {
    fn started(&self) {
        let mut l = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let t = Instant::now();
        l.recent.push_back(t);
        while l.recent.front().is_some_and(|x| t.duration_since(*x) > Duration::from_secs(3600)) {
            l.recent.pop_front();
        }
    }
    fn waiting(&self, why: Option<String>) {
        self.live.lock().unwrap_or_else(|e| e.into_inner()).waiting = why;
    }
    fn last_hour(&self) -> (u32, Option<String>) {
        let l = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let t = Instant::now();
        (l.recent.iter().filter(|x| t.duration_since(**x) <= Duration::from_secs(3600)).count() as u32, l.waiting.clone())
    }
}

/// Marks the account's provider as in use by something that is not the fill (sync, an open, a triage
/// fetch, an action) while it lives.
pub(crate) struct Foreground<'a>(&'a AccountRt);

impl AccountRt {
    pub(crate) fn foreground(&self) -> Foreground<'_> {
        self.fg.fetch_add(1, Ordering::SeqCst);
        *self.fg_last.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        Foreground(self)
    }
}

impl Drop for Foreground<'_> {
    fn drop(&mut self) {
        *self.0.fg_last.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        self.0.fg.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whether the fill runs for this account, and how fast (fetches per minute).
fn setting(shared: &Shared, rt: &AccountRt, asked: bool) -> (bool, u32) {
    let d = shared.config.read().unwrap_or_else(|e| e.into_inner()).daemon.clone();
    let per_min = rt.config.body_fill_per_min.unwrap_or(d.body_fill_per_min);
    let on = match rt.config.body_fill {
        Some(false) => false,
        Some(true) => true,
        None => d.body_fill || asked,
    };
    (on && per_min > 0, per_min)
}

/// Why the fill should not fetch for this account right now, if it should not, and when to look again.
fn not_now(shared: &Shared, rt: &AccountRt) -> Option<(String, Duration)> {
    let poll = Duration::from_millis(500);
    if shared.is_paused() {
        return Some((crate::engine::PAUSED.into(), Duration::from_secs(2)));
    }
    if rt.auth_blocked.load(Ordering::SeqCst) {
        return Some(("the account needs signing in again".into(), Duration::from_secs(30)));
    }
    {
        let st = rt.status.lock().unwrap_or_else(|e| e.into_inner());
        if !st.online {
            let why = match &st.error {
                Some(e) => format!("offline ({e})"),
                None => "waiting for the first sync".into(),
            };
            return Some((why, Duration::from_secs(2)));
        }
        if st.syncing {
            return Some(("the first sync is still running".into(), Duration::from_secs(2)));
        }
    }
    let busy = "the account is busy (sync, opening mail, triage or an action)";
    if rt.fg.load(Ordering::SeqCst) > 0 {
        return Some((busy.into(), poll));
    }
    let since = rt.fg_last.lock().unwrap_or_else(|e| e.into_inner()).elapsed();
    if since < QUIET {
        return Some((busy.into(), QUIET - since + Duration::from_millis(10)));
    }
    if let Some(l) = rt.provider.load() {
        if l.breaker_open_until.is_some() {
            return Some((format!("{}: circuit breaker open", l.scope), Duration::from_secs(10)));
        }
        if l.backoff_until.is_some() {
            return Some((format!("{}: backing off after failures", l.scope), Duration::from_secs(5)));
        }
        if l.in_flight > 0 {
            return Some((format!("{}: calls running", l.scope), poll));
        }
        if u64::from(l.starts_last_min) * 2 >= u64::from(l.max_starts_per_min) {
            return Some((format!("{}: half of the budget used this minute", l.scope), Duration::from_secs(5)));
        }
    }
    None
}

/// Waits `d`, or until the fill is woken (`sync.bodies`) or the account stops. True when stopped.
async fn rest(rt: &AccountRt, stop: &mut watch::Receiver<bool>, d: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = rt.fill.wake.notified() => false,
        _ = stop.changed() => true,
    }
}

/// Sleeps `d` unless the account stops (a wake does not shorten the spacing between fetches).
async fn spaced(stop: &mut watch::Receiver<bool>, d: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = stop.changed() => true,
    }
}

/// One account's fill loop (spawned with its sync and queue loops; stopped with them).
pub(crate) async fn fill_loop(shared: Arc<Shared>, rt: Arc<AccountRt>) {
    let mut stop = rt.stop.subscribe();
    let id = rt.account.id.clone();
    // When the last fetch started: fetches are at least 60 / per_min seconds apart.
    let mut last: Option<Instant> = None;
    let mut backoff = Duration::from_secs(30);
    loop {
        if *stop.borrow() || shared.is_shutting_down() {
            return;
        }
        let a = id.clone();
        let progress = match shared.db(move |s| s.body_fill_progress(&a)).await {
            Ok(p) => p,
            Err(_) => {
                if rest(&rt, &mut stop, Duration::from_secs(60)).await {
                    return;
                }
                continue;
            }
        };
        let (on, per_min) = setting(&shared, &rt, progress.asked);
        if !on {
            rt.fill.waiting(None);
            if rest(&rt, &mut stop, Duration::from_secs(300)).await {
                return;
            }
            continue;
        }
        let gap = Duration::from_secs_f64(60.0 / f64::from(per_min));
        if let Some(t) = last {
            let left = gap.saturating_sub(t.elapsed());
            if !left.is_zero() && spaced(&mut stop, left).await {
                return;
            }
        }
        if let Some((why, again)) = not_now(&shared, &rt) {
            rt.fill.waiting(Some(why));
            if rest(&rt, &mut stop, again).await {
                return;
            }
            continue;
        }
        if progress.done_at.is_some_and(|d| now() - d > REWALK_S) {
            let a = id.clone();
            let _ = shared.db(move |s| s.body_fill_rewind(&a, None, false)).await;
        }
        let a = id.clone();
        let item = match shared.db(move |s| s.body_fill_next(&a)).await {
            Ok(Some(item)) => item,
            Ok(None) => {
                rt.fill.waiting(None);
                if rest(&rt, &mut stop, IDLE).await {
                    return;
                }
                continue;
            }
            Err(_) => {
                if rest(&rt, &mut stop, Duration::from_secs(60)).await {
                    return;
                }
                continue;
            }
        };
        // The store read took a moment: someone may have started in the meantime.
        if let Some((why, again)) = not_now(&shared, &rt) {
            rt.fill.waiting(Some(why));
            if rest(&rt, &mut stop, again).await {
                return;
            }
            continue;
        }
        rt.fill.waiting(None);
        rt.fill.started();
        last = Some(Instant::now());
        let result = tokio::select! {
            r = crate::api::fetch_body_quietly(&shared, &id, &item.message) => r,
            _ = stop.changed() => return,
        };
        let a = id.clone();
        let it = item.clone();
        match result {
            Ok(_) => {
                backoff = Duration::from_secs(30);
                let _ = shared.db(move |s| s.body_fill_passed(&a, &it)).await;
            }
            // The server does not have it (any more), or cannot give a body: never again.
            Err(e @ (Error::NotFound(_) | Error::Unsupported(_) | Error::Invalid(_) | Error::Refused(_))) => {
                let reason = match e {
                    Error::NotFound(_) => "not_found",
                    Error::Unsupported(_) => "unsupported",
                    Error::Invalid(_) => "invalid",
                    _ => "refused",
                };
                let _ = shared
                    .db(move |s| {
                        s.body_fill_unfetchable(&a, &it.message, reason)?;
                        s.body_fill_passed(&a, &it)
                    })
                    .await;
            }
            // Offline, paused, sign-in, anything else: the same message later, after a growing wait.
            Err(e) => {
                rt.fill.waiting(Some(format!("last fetch failed ({e}); trying again in {} s", backoff.as_secs())));
                if rest(&rt, &mut stop, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(1800));
            }
        }
    }
}

/// `status`'s `body_fill` for one account.
pub(crate) async fn status(shared: &Arc<Shared>, rt: &AccountRt) -> Option<BodyFillStatus> {
    let a = rt.account.id.clone();
    let p = shared.db(move |s| s.body_fill_progress(&a)).await.ok()?;
    let (on, per_min) = setting(shared, rt, p.asked);
    let (last_hour, waiting) = rt.fill.last_hour();
    Some(BodyFillStatus { on, have: p.have, total: p.total, per_min, last_hour, done: p.done_at.is_some(), waiting: waiting.filter(|_| on) })
}

/// `sync.bodies`: wake the fill (and run it until done even when the config has it off); with `since`,
/// look again from the newest message down at everything without a body from then on.
pub(crate) async fn ask(shared: &Arc<Shared>, p: BodyFillParams) -> Result<u32> {
    let rts = match &p.account {
        Some(id) => vec![shared.rt(id)?],
        None => shared.rts(),
    };
    let mut n = 0;
    for rt in rts {
        if rt.config.body_fill == Some(false) {
            if p.account.is_some() {
                return Err(Error::Refused(format!("account {} has body_fill = false in the config", rt.account.id)));
            }
            continue;
        }
        let (a, since) = (rt.account.id.clone(), p.since);
        shared
            .db(move |s| {
                let done = s.body_fill_progress(&a)?.done_at.is_some();
                if since.is_some() || done {
                    s.body_fill_rewind(&a, since, true)
                } else {
                    // Mid-walk: keep the marks, just ask.
                    s.body_fill_ask(&a)
                }
            })
            .await?;
        rt.fill.wake.notify_one();
        n += 1;
    }
    Ok(n)
}
