//! A hard budget on gog processes. gog is a fresh ~40 MB Go process per call that decrypts its keyring on
//! start (about a CPU-second), and over ssh every call is also a connection. Left alone, a first sync
//! of two accounts started hundreds of them on a small shared host and sshd began dropping connections.
//!
//! [`GatedRunner`] wraps any [`Runner`] and puts every call through two gates:
//!
//! - **per account**: at most `account_in_flight` of this account's calls run at once;
//! - **per host** ([`HostGate`], shared by every account whose gog runs on the same ssh host, or by all
//!   local ones): at most `max_in_flight` processes at once, process starts spaced by a token bucket
//!   (`max_starts_per_min`, bursts of `burst`), a pause after transport failures (exponential backoff
//!   with jitter), and a circuit breaker: `breaker_failures` transport failures in a row and every call
//!   for that host fails at once as `Offline` for `breaker_cool` (then one probe call is let through).
//!
//! A transport failure is the runner's own `Err(Offline)`: ssh exit 255 (connection refused or reset,
//! `kex_exchange_identification`), a timeout. gog's own failures (exit codes) are answers, not transport
//! failures. Failed calls are retried here, up to `retries` times, only after the backoff.
//!
//! A call that has started is never killed because its caller went away (an account restarting on a
//! config reload, a cancelled sync): it runs in its own task, holding its slot, until it ends or hits its
//! own timeout. So the slot count is always the real number of processes.
//!
//! Time is `tokio::time`, so tests drive all of this with a paused clock.

use crate::runner::{GogCall, GogOutput, Runner};
use lumbridge_mail_core::{Error, ProviderLoad, Result};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, Semaphore};
use tokio::time::Instant;

/// Limits for one host (or for local gog).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostLimits {
    /// gog processes at once on this host, across every account using it.
    pub max_in_flight: u32,
    /// Process starts per minute (token bucket refill), across every account.
    pub max_starts_per_min: u32,
    /// Starts allowed back to back before the rate applies.
    pub burst: u32,
    /// Transport failures in a row that open the circuit breaker.
    pub breaker_failures: u32,
    /// How long the breaker stays open.
    pub breaker_cool: Duration,
    /// First wait after a transport failure; doubles per failure in a row, with jitter, up to `backoff_max`.
    pub backoff_base: Duration,
    pub backoff_max: Duration,
}

impl HostLimits {
    /// For gog over ssh: 2 at once, 2 starts per second.
    pub fn ssh() -> Self {
        Self {
            max_in_flight: 2,
            max_starts_per_min: 120,
            burst: 2,
            breaker_failures: 5,
            breaker_cool: Duration::from_secs(300),
            backoff_base: Duration::from_secs(2),
            backoff_max: Duration::from_secs(120),
        }
    }
    /// For gog on this machine: 3 at once, 2 starts per second.
    pub fn local() -> Self {
        Self { max_in_flight: 3, ..Self::ssh() }
    }

    fn sane(mut self) -> Self {
        self.max_in_flight = self.max_in_flight.max(1);
        self.max_starts_per_min = self.max_starts_per_min.max(1);
        self.burst = self.burst.max(1);
        self.breaker_failures = self.breaker_failures.max(1);
        self
    }
}

/// Limits for one account's calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountLimits {
    /// This account's calls at once (still inside the host's limit).
    pub max_in_flight: u32,
    /// Extra tries for a call that failed on transport (after the host's backoff).
    pub retries: u32,
}

impl Default for AccountLimits {
    fn default() -> Self {
        Self { max_in_flight: 2, retries: 3 }
    }
}

#[derive(Debug)]
struct State {
    limits: HostLimits,
    in_flight: u32,
    tokens: f64,
    refilled: Instant,
    starts: VecDeque<Instant>,
    total: u64,
    failures_in_row: u32,
    backoff_until: Option<Instant>,
    breaker_until: Option<Instant>,
}

/// The shared gate for one host. Get it from a [`Gates`] registry so accounts on one host share it.
#[derive(Debug)]
pub struct HostGate {
    scope: String,
    state: Mutex<State>,
    freed: Notify,
    seed: AtomicU64,
}

/// A running process's slot; frees it when dropped.
struct Slot(Arc<HostGate>);

impl Drop for Slot {
    fn drop(&mut self) {
        {
            let mut st = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
            st.in_flight = st.in_flight.saturating_sub(1);
        }
        self.0.freed.notify_waiters();
    }
}

impl HostGate {
    pub fn new(scope: impl Into<String>, limits: HostLimits) -> Arc<Self> {
        let limits = limits.sane();
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(7) | 1;
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

    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// New limits (a config reload). Running processes keep their slots.
    pub fn set_limits(&self, limits: HostLimits) {
        let limits = limits.sane();
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.tokens = st.tokens.min(f64::from(limits.burst));
        st.limits = limits;
        drop(st);
        self.freed.notify_waiters();
    }

    pub fn limits(&self) -> HostLimits {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).limits.clone()
    }

    /// A number in [0, 1) for jitter (xorshift; not for secrets).
    fn jitter(&self) -> f64 {
        let mut x = self.seed.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.seed.store(x, Ordering::Relaxed);
        (x >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Wait for a slot and a start token. `Offline` at once while the breaker is open.
    async fn acquire(self: &Arc<Self>) -> Result<Slot> {
        loop {
            let freed = self.freed.notified();
            let wait = {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                if let Some(until) = st.breaker_until
                    && now < until
                {
                    return Err(Error::Offline(format!(
                        "gog on {}: paused for {} s after {} connection failures in a row (circuit breaker)",
                        self.scope,
                        (until - now).as_secs().max(1),
                        st.failures_in_row
                    )));
                }
                // Refill the bucket.
                let rate = f64::from(st.limits.max_starts_per_min) / 60.0;
                let dt = now.saturating_duration_since(st.refilled).as_secs_f64();
                st.tokens = (st.tokens + dt * rate).min(f64::from(st.limits.burst));
                st.refilled = now;
                // After the breaker's cooling time, one probe at a time until a call succeeds.
                let probing = st.failures_in_row >= st.limits.breaker_failures;
                let cap = if probing { 1 } else { st.limits.max_in_flight };
                if st.in_flight >= cap {
                    None
                } else if let Some(until) = st.backoff_until.filter(|u| now < *u) {
                    Some(until - now)
                } else if st.tokens < 1.0 {
                    Some(Duration::from_secs_f64(((1.0 - st.tokens) / rate).max(0.001)))
                } else {
                    st.tokens -= 1.0;
                    st.in_flight += 1;
                    st.total += 1;
                    st.starts.push_back(now);
                    while st.starts.front().is_some_and(|t| now.saturating_duration_since(*t) > Duration::from_secs(60)) {
                        st.starts.pop_front();
                    }
                    return Ok(Slot(self.clone()));
                }
            };
            match wait {
                // Full: wait for a slot to free (or a limit change); look again now and then anyway.
                None => {
                    let _ = tokio::time::timeout(Duration::from_secs(5), freed).await;
                }
                Some(d) => tokio::time::sleep(d).await,
            }
        }
    }

    fn succeeded(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.failures_in_row = 0;
        st.backoff_until = None;
        st.breaker_until = None;
    }

    /// A transport failure: back off (doubling, jittered), and open the breaker at the threshold.
    fn failed(&self) {
        let j = self.jitter();
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.failures_in_row += 1;
        let now = Instant::now();
        let exp = st.failures_in_row.saturating_sub(1).min(16);
        let base = st.limits.backoff_base.as_secs_f64() * f64::from(1u32 << exp);
        // "Equal jitter": half fixed, half random, so waits never collapse to zero.
        let d = base.min(st.limits.backoff_max.as_secs_f64()) * (0.5 + 0.5 * j);
        st.backoff_until = Some(now + Duration::from_secs_f64(d));
        if st.failures_in_row >= st.limits.breaker_failures {
            st.breaker_until = Some(now + st.limits.breaker_cool);
        }
    }

    /// True while the breaker refuses calls.
    pub fn breaker_open(&self) -> bool {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.breaker_until.is_some_and(|u| Instant::now() < u)
    }

    pub fn load(&self) -> ProviderLoad {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        while st.starts.front().is_some_and(|t| now.saturating_duration_since(*t) > Duration::from_secs(60)) {
            st.starts.pop_front();
        }
        let unix = |t: Option<Instant>| {
            t.filter(|t| *t > now).map(|t| unix_now() + (t - now).as_secs().max(1) as i64)
        };
        ProviderLoad {
            scope: self.scope.clone(),
            in_flight: st.in_flight,
            starts_last_min: st.starts.len() as u32,
            starts_total: st.total,
            backoff_until: unix(st.backoff_until),
            breaker_open_until: unix(st.breaker_until),
            max_in_flight: st.limits.max_in_flight,
            max_starts_per_min: st.limits.max_starts_per_min,
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Host gates by scope (`ssh:<host>` or `local`), so every account on one host shares one budget.
#[derive(Debug, Default)]
pub struct Gates {
    gates: Mutex<HashMap<String, Arc<HostGate>>>,
}

impl Gates {
    pub fn new() -> Self {
        Self::default()
    }

    /// The gate for `scope`, made with `limits` the first time; later calls update its limits.
    pub fn gate(&self, scope: &str, limits: HostLimits) -> Arc<HostGate> {
        let mut g = self.gates.lock().unwrap_or_else(|e| e.into_inner());
        match g.get(scope) {
            Some(gate) => {
                if gate.limits() != limits.clone().sane() {
                    gate.set_limits(limits);
                }
                gate.clone()
            }
            None => {
                let gate = HostGate::new(scope, limits);
                g.insert(scope.to_owned(), gate.clone());
                gate
            }
        }
    }

    pub fn all(&self) -> Vec<Arc<HostGate>> {
        let g = self.gates.lock().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<_> = g.values().cloned().collect();
        v.sort_by(|a, b| a.scope.cmp(&b.scope));
        v
    }
}

/// A [`Runner`] inside the budget: one account's gate, then its host's.
pub struct GatedRunner {
    inner: Arc<dyn Runner>,
    host: Arc<HostGate>,
    account: Arc<Semaphore>,
    limits: AccountLimits,
}

impl GatedRunner {
    pub fn new(inner: Arc<dyn Runner>, host: Arc<HostGate>, limits: AccountLimits) -> Self {
        let n = limits.max_in_flight.max(1) as usize;
        Self { inner, host, account: Arc::new(Semaphore::new(n)), limits }
    }

    pub fn host(&self) -> &Arc<HostGate> {
        &self.host
    }
}

#[async_trait::async_trait]
impl Runner for GatedRunner {
    async fn run(&self, call: GogCall) -> Result<GogOutput> {
        let mut attempt = 0u32;
        loop {
            let mine = self.account.clone().acquire_owned().await.map_err(|_| Error::Other("gog budget closed".into()))?;
            let slot = self.host.acquire().await?;
            let inner = self.inner.clone();
            let host = self.host.clone();
            let c = call.clone();
            // Its own task: if our caller goes away, the process still ends on its own (or at its
            // timeout) and keeps its slots until then.
            let task = tokio::spawn(async move {
                let out = inner.run(c).await;
                match &out {
                    Err(Error::Offline(_)) => host.failed(),
                    // gog answered (whatever it said): the transport is fine.
                    Ok(_) => host.succeeded(),
                    Err(_) => {}
                }
                drop(slot);
                drop(mine);
                out
            });
            let out = task.await.map_err(|e| Error::Other(format!("gog call task: {e}")))?;
            match out {
                Err(Error::Offline(_)) if attempt < self.limits.retries && !self.host.breaker_open() => attempt += 1,
                other => return other,
            }
        }
    }

    fn load(&self) -> Option<ProviderLoad> {
        Some(self.host.load())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeRunner;
    use crate::runner::Arg;
    use std::sync::atomic::AtomicU32;

    fn call() -> GogCall {
        GogCall::new(vec![Arg::from("--enable-commands-exact"), Arg::from("gmail.history")], Duration::from_secs(60))
    }

    /// A runner that takes `work` of (virtual) time per call and records how many run at once.
    struct Slow {
        work: Duration,
        now: AtomicU32,
        peak: AtomicU32,
        starts: Mutex<Vec<Instant>>,
        fail_first: AtomicU32,
    }

    impl Slow {
        fn new(work: Duration) -> Arc<Self> {
            Arc::new(Self { work, now: AtomicU32::new(0), peak: AtomicU32::new(0), starts: Mutex::new(Vec::new()), fail_first: AtomicU32::new(0) })
        }
    }

    #[async_trait::async_trait]
    impl Runner for Slow {
        async fn run(&self, _call: GogCall) -> Result<GogOutput> {
            self.starts.lock().unwrap().push(Instant::now());
            let n = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(n, Ordering::SeqCst);
            tokio::time::sleep(self.work).await;
            self.now.fetch_sub(1, Ordering::SeqCst);
            if self.fail_first.load(Ordering::SeqCst) > 0 {
                self.fail_first.fetch_sub(1, Ordering::SeqCst);
                return Err(Error::Offline("ssh: kex_exchange_identification: Connection reset by peer".into()));
            }
            Ok(GogOutput::ok_json(serde_json::json!({})))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_host_cap_holds_across_accounts_and_starts_are_spaced() {
        let gates = Gates::new();
        let slow = Slow::new(Duration::from_millis(1500));
        let host = gates.gate("ssh:box", HostLimits::ssh());
        // Two accounts on one host, each allowed 2: the host's 2 is the real cap.
        let a = Arc::new(GatedRunner::new(slow.clone(), host.clone(), AccountLimits::default()));
        let b = Arc::new(GatedRunner::new(slow.clone(), gates.gate("ssh:box", HostLimits::ssh()), AccountLimits::default()));
        let mut jobs = Vec::new();
        for i in 0..20 {
            let r = if i % 2 == 0 { a.clone() } else { b.clone() };
            jobs.push(tokio::spawn(async move { r.run(call()).await }));
        }
        for j in jobs {
            j.await.unwrap().unwrap();
        }
        assert_eq!(slow.peak.load(Ordering::SeqCst), 2, "never more than 2 gog processes on the host");
        let starts = slow.starts.lock().unwrap().clone();
        assert_eq!(starts.len(), 20);
        // 2 starts per second after a burst of 2: any 10 consecutive starts span at least ~4 s.
        for w in starts.windows(11) {
            assert!(w[10] - w[0] >= Duration::from_millis(4900), "{:?}", w[10] - w[0]);
        }
        assert_eq!(host.load().starts_total, 20);
        assert_eq!(host.load().in_flight, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn the_rate_limit_spaces_starts_even_when_calls_are_instant() {
        let slow = Slow::new(Duration::ZERO);
        let host = HostGate::new("local", HostLimits { max_starts_per_min: 60, burst: 1, ..HostLimits::local() });
        let r = GatedRunner::new(slow.clone(), host, AccountLimits { max_in_flight: 3, retries: 0 });
        let t0 = Instant::now();
        for _ in 0..5 {
            r.run(call()).await.unwrap();
        }
        let starts = slow.starts.lock().unwrap().clone();
        for w in starts.windows(2) {
            assert!(w[1] - w[0] >= Duration::from_millis(999), "{:?}", w[1] - w[0]);
        }
        assert!(Instant::now() - t0 >= Duration::from_secs(4));
    }

    #[tokio::test(start_paused = true)]
    async fn an_account_cap_below_the_host_cap_holds() {
        let slow = Slow::new(Duration::from_secs(1));
        let host = HostGate::new("local", HostLimits { max_in_flight: 5, max_starts_per_min: 6000, burst: 10, ..HostLimits::local() });
        let r = Arc::new(GatedRunner::new(slow.clone(), host, AccountLimits { max_in_flight: 1, retries: 0 }));
        let jobs: Vec<_> = (0..4).map(|_| { let r = r.clone(); tokio::spawn(async move { r.run(call()).await }) }).collect();
        for j in jobs {
            j.await.unwrap().unwrap();
        }
        assert_eq!(slow.peak.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn transport_failures_back_off_exponentially_then_succeed() {
        let slow = Slow::new(Duration::ZERO);
        slow.fail_first.store(3, Ordering::SeqCst);
        let host = HostGate::new("ssh:box", HostLimits { max_starts_per_min: 6000, burst: 10, ..HostLimits::ssh() });
        let r = GatedRunner::new(slow.clone(), host.clone(), AccountLimits { max_in_flight: 1, retries: 3 });
        r.run(call()).await.expect("the fourth try works");
        let s = slow.starts.lock().unwrap().clone();
        assert_eq!(s.len(), 4);
        let gaps: Vec<Duration> = s.windows(2).map(|w| w[1] - w[0]).collect();
        // base 2 s, doubling, with jitter in [50%, 100%]: >= 1, 2, 4 s and <= 2, 4, 8 s.
        for (i, g) in gaps.iter().enumerate() {
            let full = Duration::from_secs(2 << i);
            assert!(*g >= full / 2 && *g <= full + Duration::from_millis(10), "gap {i}: {g:?}");
        }
        assert_eq!(host.load().breaker_open_until, None);
        assert_eq!(host.load().backoff_until, None, "a success clears the backoff");
    }

    #[tokio::test(start_paused = true)]
    async fn the_breaker_opens_after_n_failures_fails_fast_then_probes() {
        let f = Arc::new(FakeRunner::new());
        f.on_err("gmail.history", &[], Error::Offline("ssh box: Connection reset by peer".into()));
        let limits = HostLimits { breaker_failures: 3, breaker_cool: Duration::from_secs(300), max_starts_per_min: 6000, burst: 10, ..HostLimits::ssh() };
        let host = HostGate::new("ssh:box", limits);
        let r = GatedRunner::new(f.clone(), host.clone(), AccountLimits { max_in_flight: 1, retries: 5 });
        // Retries stop as soon as the breaker opens: 3 tries, not 6.
        assert!(matches!(r.run(call()).await, Err(Error::Offline(_))));
        assert_eq!(f.calls().len(), 3);
        assert!(host.breaker_open());
        assert!(host.load().breaker_open_until.is_some());
        // While open: refused at once, nothing started.
        let t = Instant::now();
        let e = r.run(call()).await.unwrap_err();
        assert!(e.to_string().contains("circuit breaker"), "{e}");
        assert_eq!(f.calls().len(), 3);
        assert!(Instant::now() - t < Duration::from_secs(1));
        // After the cooling time one probe goes out; it fails, so the breaker opens again at once.
        tokio::time::advance(Duration::from_secs(301)).await;
        assert!(r.run(call()).await.is_err());
        assert_eq!(f.calls().len(), 4, "one probe, no retries while the breaker is open");
        assert!(host.breaker_open());
        // The host recovers: the next probe works and everything is normal again.
        tokio::time::advance(Duration::from_secs(301)).await;
        f.on_json("gmail.history", &[], serde_json::json!({}));
        r.run(call()).await.unwrap();
        assert!(!host.breaker_open());
        r.run(call()).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn gog_errors_are_answers_not_transport_failures() {
        let f = Arc::new(FakeRunner::new());
        f.on("gmail.history", &[], GogOutput::fail(8, "backend error"));
        let host = HostGate::new("local", HostLimits { breaker_failures: 1, ..HostLimits::local() });
        let r = GatedRunner::new(f.clone(), host.clone(), AccountLimits::default());
        let out = r.run(call()).await.unwrap();
        assert_eq!(out.code, 8);
        assert_eq!(f.calls().len(), 1, "not retried here (the provider maps it)");
        assert!(!host.breaker_open());
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_caller_does_not_kill_the_process_or_free_its_slot_early() {
        let slow = Slow::new(Duration::from_secs(10));
        let host = HostGate::new("ssh:box", HostLimits { max_in_flight: 1, ..HostLimits::ssh() });
        let r = Arc::new(GatedRunner::new(slow.clone(), host.clone(), AccountLimits::default()));
        let r2 = r.clone();
        let first = tokio::spawn(async move { r2.run(call()).await });
        tokio::time::sleep(Duration::from_secs(1)).await;
        first.abort();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(host.load().in_flight, 1, "still running, still counted");
        let t = Instant::now();
        r.run(call()).await.unwrap();
        assert!(Instant::now() - t >= Duration::from_secs(8), "the second waited for the first to finish");
        assert_eq!(slow.starts.lock().unwrap().len(), 2);
    }
}
