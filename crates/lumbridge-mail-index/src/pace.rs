//! A gentle pace for calls to a shared endpoint: at most `per_minute` starts per minute, spread
//! evenly (no bursts), and at most `concurrency` in flight. The embedding and chat servers are
//! shared GPU boxes; a backlog of mail must not crowd out everyone else.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

pub struct Pacer {
    gap: Duration,
    next: Mutex<Option<Instant>>,
    slots: Arc<Semaphore>,
}

impl Pacer {
    /// `per_minute` 0 means no rate limit; `concurrency` is at least 1.
    pub fn new(per_minute: u32, concurrency: usize) -> Pacer {
        Pacer {
            gap: if per_minute == 0 {
                Duration::ZERO
            } else {
                Duration::from_secs_f64(60.0 / f64::from(per_minute))
            },
            next: Mutex::new(None),
            slots: Arc::new(Semaphore::new(concurrency.max(1))),
        }
    }

    /// Waits for a slot and for this call's turn. Hold the permit while the call runs.
    pub async fn start(&self) -> OwnedSemaphorePermit {
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");
        let wait = {
            let mut next = self.next.lock().await;
            let now = Instant::now();
            let at = next.map_or(now, |n| n.max(now));
            *next = Some(at + self.gap);
            at - now
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        permit
    }

    /// The time between two starts.
    pub fn gap(&self) -> Duration {
        self.gap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn starts_are_spread_and_bounded() {
        let p = Arc::new(Pacer::new(120, 1)); // one every 500 ms
        let t0 = Instant::now();
        let mut at = Vec::new();
        for _ in 0..4 {
            let _g = p.start().await;
            at.push(t0.elapsed());
        }
        assert_eq!(at, [0, 500, 1000, 1500].map(Duration::from_millis).to_vec());
        // Concurrency 1: a second caller waits for the first permit.
        let p = Arc::new(Pacer::new(0, 1));
        let g = p.start().await;
        let p2 = p.clone();
        let h = tokio::spawn(async move {
            let _g = p2.start().await;
            Instant::now()
        });
        tokio::time::sleep(Duration::from_secs(3)).await;
        let released = Instant::now();
        drop(g);
        assert!(h.await.unwrap() >= released);
    }
}
