use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    rate_per_sec: u64,
    tokens: f64,
    last_update: Instant,
}

impl RateLimiter {
    pub fn new(rate_per_sec: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                rate_per_sec,
                tokens: 0.0, // Start with 0 tokens to prevent initial burst
                last_update: Instant::now(),
            })),
        }
    }

    pub async fn acquire(&self, amount: u64) {
        if amount == 0 {
            return;
        }
        let mut inner = self.inner.lock().await;
        loop {
            let now = Instant::now();
            let elapsed = now.duration_since(inner.last_update).as_secs_f64();
            
            // Refill tokens
            inner.tokens += elapsed * inner.rate_per_sec as f64;
            // Cap at burst size (1 second worth of tokens)
            if inner.tokens > inner.rate_per_sec as f64 {
                inner.tokens = inner.rate_per_sec as f64;
            }
            inner.last_update = now;

            if inner.tokens >= amount as f64 {
                inner.tokens -= amount as f64;
                break;
            } else {
                // Calculate time to wait
                let missing = amount as f64 - inner.tokens;
                // avoid division by zero
                let rate = if inner.rate_per_sec > 0 { inner.rate_per_sec as f64 } else { 1.0 };
                let wait_secs = missing / rate;
                let wait = Duration::from_secs_f64(wait_secs);
                
                // Release lock and sleep
                drop(inner);
                tokio::time::sleep(wait).await;
                // Re-acquire
                inner = self.inner.lock().await;
            }
        }
    }

    pub async fn set_rate(&self, new_rate: u64) {
        let mut inner = self.inner.lock().await;
        inner.rate_per_sec = new_rate;
        // Adjust current tokens if needed? No, just keep them.
    }
}
