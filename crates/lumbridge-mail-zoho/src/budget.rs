//! The connection's budget, shared by every account on it: at most `max_in_flight` calls at once, call
//! starts spaced by a token bucket (`max_calls_per_min`, bursts of `burst`), a pause after transport
//! failures (exponential backoff with jitter, or the server's Retry-After) and a circuit breaker
//! (`breaker_failures` transport failures in a row and every call fails at once as `Offline` for
//! `breaker_cool`; then one probe call at a time until one succeeds). The same shape as the gog budget.
//!
//! Time is `tokio::time`, so tests drive it with a paused clock.

use crate::config::Limits;
use lumbridge_mail_core::{Error, ProviderLoad, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::Instant;

#[derive(Debug)]
struct State {
    limits: Limits,
    in_flight: u32,
    tokens: f64,
    refilled: Instant,
    starts: VecDeque<Instant>,
    total: u64,
    failures_in_row: u32,
    backoff_until: Option<Instant>,
    breaker_until: Option<Instant>,
}

#[derive(Debug)]
pub struct Gate {
    scope: String,
    state: Mutex<State>,
    freed: Notify,
    seed: AtomicU64,
}

/// A running call's slot; frees it when dropped.
pub(crate) struct Slot(Arc<Gate>);

impl Drop for Slot {
    fn drop(&mut self) {
        {
            let mut st = self.0.lock();
            st.in_flight = st.in_flight.saturating_sub(1);
        }
        self.0.freed.notify_waiters();
    }
}

fn sane(mut l: Limits) -> Limits {
    l.max_in_flight = l.max_in_flight.max(1);
    l.max_calls_per_min = l.max_calls_per_min.max(1);
    l.burst = l.burst.max(1);
    l.breaker_failures = l.breaker_failures.max(1);
    l
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Gate {
    pub fn new(scope: impl Into<String>, limits: Limits) -> Arc<Self> {
        let limits = sane(limits);
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(7)
            | 1;
        Arc::new(Self {
            scope: scope.into(),
            state: Mutex::new(State {
                tokens: f64::from(limits.burst),
                limits,
                in_flight: 0,
                refilled: Instant::now(),
                starts: VecDeque::new(),
                total: 0,
                failures_in_row: 0,
                backoff_until: None,
                breaker_until: None,
            }),
            freed: Notify::new(),
            seed: AtomicU64::new(seed),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn limits(&self) -> Limits {
        self.lock().limits.clone()
    }

    /// New limits (a config reload). Running calls keep their slots.
    pub fn set_limits(&self, limits: Limits) {
        let limits = sane(limits);
        {
            let mut st = self.lock();
            st.tokens = st.tokens.min(f64::from(limits.burst));
            st.limits = limits;
        }
        self.freed.notify_waiters();
    }

    fn jitter(&self) -> f64 {
        let mut x = self.seed.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.seed.store(x, Ordering::Relaxed);
        (x >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Waits for a slot and a start token. `Offline` at once while the breaker is open.
    pub(crate) async fn acquire(self: &Arc<Self>) -> Result<Slot> {
        loop {
            let freed = self.freed.notified();
            let wait = {
                let mut st = self.lock();
                let now = Instant::now();
                if let Some(until) = st.breaker_until
                    && now < until
                {
                    return Err(Error::Offline(format!(
                        "Zoho: paused for {} s after {} connection failures in a row (circuit breaker)",
                        (until - now).as_secs().max(1),
                        st.failures_in_row
                    )));
                }
                let rate = f64::from(st.limits.max_calls_per_min) / 60.0;
                let dt = now.saturating_duration_since(st.refilled).as_secs_f64();
                st.tokens = (st.tokens + dt * rate).min(f64::from(st.limits.burst));
                st.refilled = now;
                let probing = st.failures_in_row >= st.limits.breaker_failures;
                let cap = if probing { 1 } else { st.limits.max_in_flight };
                if st.in_flight >= cap {
                    None
                } else if let Some(until) = st.backoff_until.filter(|u| now < *u) {
                    Some(until - now)
                } else if st.tokens < 1.0 {
                    Some(Duration::from_secs_f64(
                        ((1.0 - st.tokens) / rate).max(0.001),
                    ))
                } else {
                    st.tokens -= 1.0;
                    st.in_flight += 1;
                    st.total += 1;
                    st.starts.push_back(now);
                    while st.starts.front().is_some_and(|t| {
                        now.saturating_duration_since(*t) > Duration::from_secs(60)
                    }) {
                        st.starts.pop_front();
                    }
                    return Ok(Slot(self.clone()));
                }
            };
            match wait {
                None => {
                    let _ = tokio::time::timeout(Duration::from_secs(5), freed).await;
                }
                Some(d) => tokio::time::sleep(d).await,
            }
        }
    }

    pub(crate) fn succeeded(&self) {
        let mut st = self.lock();
        st.failures_in_row = 0;
        st.backoff_until = None;
        st.breaker_until = None;
    }

    /// A transport failure: back off (doubling, jittered), and open the breaker at the threshold.
    pub(crate) fn failed(&self) {
        let j = self.jitter();
        let mut st = self.lock();
        st.failures_in_row += 1;
        let now = Instant::now();
        let exp = st.failures_in_row.saturating_sub(1).min(16);
        let base = st.limits.backoff_base.as_secs_f64() * f64::from(1u32 << exp);
        let d = base.min(st.limits.backoff_max.as_secs_f64()) * (0.5 + 0.5 * j);
        st.backoff_until = Some(now + Duration::from_secs_f64(d));
        if st.failures_in_row >= st.limits.breaker_failures {
            st.breaker_until = Some(now + st.limits.breaker_cool);
        }
    }

    /// The server asked us to wait (429 Retry-After): nothing starts before then. Not a failure.
    pub(crate) fn hold(&self, d: Duration) {
        let mut st = self.lock();
        let until = Instant::now() + d.min(st.limits.backoff_max);
        if st.backoff_until.is_none_or(|u| u < until) {
            st.backoff_until = Some(until);
        }
    }

    pub fn load(&self) -> ProviderLoad {
        let mut st = self.lock();
        let now = Instant::now();
        while st
            .starts
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) > Duration::from_secs(60))
        {
            st.starts.pop_front();
        }
        let unix = |t: Option<Instant>| {
            t.filter(|t| *t > now)
                .map(|t| unix_now() + (t - now).as_secs().max(1) as i64)
        };
        ProviderLoad {
            scope: self.scope.clone(),
            in_flight: st.in_flight,
            starts_last_min: st.starts.len() as u32,
            starts_total: st.total,
            backoff_until: unix(st.backoff_until),
            breaker_open_until: unix(st.breaker_until),
            max_in_flight: st.limits.max_in_flight,
            max_starts_per_min: st.limits.max_calls_per_min,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            max_in_flight: 2,
            max_calls_per_min: 60,
            burst: 2,
            breaker_failures: 3,
            breaker_cool: Duration::from_secs(300),
            backoff_base: Duration::from_secs(2),
            backoff_max: Duration::from_secs(60),
            retries: 0,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn in_flight_rate_backoff_and_breaker() {
        let g = Gate::new("zoho:test", limits());
        let a = g.acquire().await.unwrap();
        let b = g.acquire().await.unwrap();
        assert_eq!(g.load().in_flight, 2);
        // A third waits for a slot.
        let g2 = g.clone();
        let third = tokio::spawn(async move { g2.acquire().await.map(|_| Instant::now()) });
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!third.is_finished());
        drop(a);
        third.await.unwrap().unwrap();
        drop(b);
        // Failures back off, then open the breaker.
        g.failed();
        assert!(g.load().backoff_until.is_some());
        g.failed();
        g.failed();
        assert!(g.load().breaker_open_until.is_some());
        assert!(matches!(g.acquire().await, Err(Error::Offline(_))));
        tokio::time::sleep(Duration::from_secs(301)).await;
        let probe = g.acquire().await.unwrap();
        g.succeeded();
        drop(probe);
        assert!(g.load().breaker_open_until.is_none());
        assert_eq!(g.load().max_starts_per_min, 60);
        assert!(g.load().starts_total >= 4);
    }

    #[tokio::test(start_paused = true)]
    async fn the_rate_is_kept() {
        let g = Gate::new(
            "zoho:test",
            Limits {
                max_calls_per_min: 6,
                burst: 1,
                ..limits()
            },
        );
        let t0 = Instant::now();
        for _ in 0..3 {
            drop(g.acquire().await.unwrap());
        }
        // 1 from the burst, then one per 10 s.
        assert!(
            Instant::now() - t0 >= Duration::from_secs(20),
            "{:?}",
            Instant::now() - t0
        );
        g.hold(Duration::from_secs(30));
        let t1 = Instant::now();
        drop(g.acquire().await.unwrap());
        assert!(Instant::now() - t1 >= Duration::from_secs(30));
    }
}
