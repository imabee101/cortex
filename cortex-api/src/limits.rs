//! In-process limiters that sit in front of the upstreams: a fixed-window rate
//! limiter, a circuit breaker, and a bounded queue in front of the inference slots.
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

const GC_EVERY: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct Limiter {
    inner: Mutex<LimiterInner>,
}

#[derive(Default)]
struct LimiterInner {
    windows: HashMap<String, (Instant, u32)>,
    last_gc: Option<Instant>,
}

impl Limiter {
    /// Count one hit for `key`. `Err(seconds)` is how long until the window resets.
    pub fn hit(&self, key: &str, limit: u32, window: Duration) -> Result<(), u64> {
        let now = Instant::now();
        let Ok(mut inner) = self.inner.lock() else {
            return Ok(());
        };
        if inner
            .last_gc
            .is_none_or(|at| now.duration_since(at) >= GC_EVERY)
        {
            inner
                .windows
                .retain(|_, (start, _)| now.duration_since(*start) < window.max(GC_EVERY));
            inner.last_gc = Some(now);
        }
        let entry = inner.windows.entry(key.to_owned()).or_insert((now, 0));
        if now.duration_since(entry.0) >= window {
            *entry = (now, 0);
        }
        entry.1 += 1;
        if entry.1 > limit {
            let left = window.saturating_sub(now.duration_since(entry.0));
            return Err(left.as_secs().max(1));
        }
        Ok(())
    }
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Opens after `fail_limit` consecutive failures and stays open for `open_ms`.
pub struct Breaker {
    failures: AtomicU64,
    open_until: AtomicU64,
    fail_limit: u64,
    open_ms: u64,
}

impl Breaker {
    pub fn new(fail_limit: u64, open_ms: u64) -> Self {
        Self {
            failures: AtomicU64::new(0),
            open_until: AtomicU64::new(0),
            fail_limit,
            open_ms,
        }
    }

    /// `Err(seconds)` while the breaker is open.
    pub fn check(&self) -> Result<(), u64> {
        let until = self.open_until.load(Ordering::Relaxed);
        let now = unix_ms();
        if until > now {
            return Err(((until - now) / 1000).max(1));
        }
        Ok(())
    }

    pub fn is_open(&self) -> bool {
        self.check().is_err()
    }

    pub fn failure(&self) {
        let failures = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        if failures >= self.fail_limit {
            self.open_until
                .store(unix_ms() + self.open_ms, Ordering::Relaxed);
            self.failures.store(0, Ordering::Relaxed);
        }
    }

    pub fn success(&self) {
        self.failures.store(0, Ordering::Relaxed);
    }
}

pub enum Busy {
    /// The wait queue is full.
    Full,
    /// A queued request waited the whole timeout.
    Timeout,
}

/// A bounded wait in front of the `llama-server --parallel` slots.
pub struct SlotQueue {
    slots: Arc<Semaphore>,
    capacity: usize,
    waiting: AtomicUsize,
    max_waiting: usize,
    wait: Duration,
    pub rejected: AtomicU64,
}

impl SlotQueue {
    pub fn new(capacity: usize, max_waiting: usize, wait: Duration) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(capacity)),
            capacity,
            waiting: AtomicUsize::new(0),
            max_waiting,
            wait,
            rejected: AtomicU64::new(0),
        }
    }

    pub fn in_flight(&self) -> usize {
        self.capacity.saturating_sub(self.slots.available_permits())
    }

    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::Relaxed)
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub async fn acquire(&self) -> Result<OwnedSemaphorePermit, Busy> {
        if let Ok(permit) = self.slots.clone().try_acquire_owned() {
            return Ok(permit);
        }
        if self.waiting.fetch_add(1, Ordering::Relaxed) >= self.max_waiting {
            self.waiting.fetch_sub(1, Ordering::Relaxed);
            self.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(Busy::Full);
        }
        let result = tokio::time::timeout(self.wait, self.slots.clone().acquire_owned()).await;
        self.waiting.fetch_sub(1, Ordering::Relaxed);
        match result {
            Ok(Ok(permit)) => Ok(permit),
            _ => {
                self.rejected.fetch_add(1, Ordering::Relaxed);
                Err(Busy::Timeout)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_blocks_after_the_limit_and_names_the_wait() {
        let limiter = Limiter::default();
        let window = Duration::from_secs(60);
        assert!(limiter.hit("a", 2, window).is_ok());
        assert!(limiter.hit("a", 2, window).is_ok());
        let retry = limiter
            .hit("a", 2, window)
            .expect_err("third hit is over the limit");
        assert!((1..=60).contains(&retry));
        assert!(limiter.hit("b", 2, window).is_ok(), "keys are independent");
    }

    #[test]
    fn breaker_opens_after_the_limit_and_closes_on_success_path() {
        let breaker = Breaker::new(2, 60_000);
        assert!(breaker.check().is_ok());
        breaker.failure();
        breaker.success();
        breaker.failure();
        assert!(breaker.check().is_ok(), "success resets the count");
        breaker.failure();
        assert!(breaker.is_open());
    }

    #[tokio::test]
    async fn queue_waits_then_rejects_when_full() {
        let queue = SlotQueue::new(1, 1, Duration::from_millis(200));
        let held = queue
            .acquire()
            .await
            .ok()
            .expect("first request takes the slot");
        let queue = Arc::new(queue);
        let waiter = {
            let queue = queue.clone();
            tokio::spawn(async move { queue.acquire().await.is_ok() })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(queue.waiting(), 1);
        assert!(
            matches!(queue.acquire().await, Err(Busy::Full)),
            "queue holds one waiter"
        );
        drop(held);
        assert!(
            waiter.await.expect("waiter task"),
            "waiter gets the freed slot"
        );
    }

    #[tokio::test]
    async fn queued_request_times_out() {
        let queue = SlotQueue::new(1, 4, Duration::from_millis(50));
        let _held = queue.acquire().await.ok().expect("slot");
        assert!(matches!(queue.acquire().await, Err(Busy::Timeout)));
    }
}
