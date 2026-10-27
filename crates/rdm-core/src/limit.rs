use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A token bucket shared by any number of connections. Rate 0 = unlimited.
#[derive(Debug)]
pub struct RateLimiter {
    rate: AtomicU64,
    bucket: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    /// May go negative: callers take what they need and sleep off the debt.
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            rate: AtomicU64::new(bytes_per_sec),
            bucket: Mutex::new(Bucket { tokens: 0.0, last: Instant::now() }),
        }
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    pub fn set_rate(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Ordering::Relaxed);
    }

    /// Waits until `n` more bytes fit within the rate.
    pub async fn acquire(&self, n: u64) {
        let wait = self.reserve(n);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    fn reserve(&self, n: u64) -> Duration {
        let rate = self.rate() as f64;
        let mut b = self.bucket.lock().unwrap();
        let now = Instant::now();
        if rate == 0.0 {
            b.last = now;
            return Duration::ZERO;
        }
        // Allow at most one second of burst.
        b.tokens = (b.tokens + now.duration_since(b.last).as_secs_f64() * rate).min(rate);
        b.last = now;
        b.tokens -= n as f64;
        if b.tokens >= 0.0 { Duration::ZERO } else { Duration::from_secs_f64(-b.tokens / rate) }
    }
}

/// Parses `500K`, `2M`, `1.5MB`, `800` (bytes per second). `0` = unlimited.
pub fn parse_rate(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_uppercase();
    let s = s.trim_end_matches("/S").trim_end_matches('B');
    let (num, mult) = match s.chars().last()? {
        'K' => (&s[..s.len() - 1], 1024.0),
        'M' => (&s[..s.len() - 1], 1024.0 * 1024.0),
        'G' => (&s[..s.len() - 1], 1024.0 * 1024.0 * 1024.0),
        _ => (s, 1.0),
    };
    let value: f64 = num.trim().parse().ok()?;
    (value >= 0.0).then_some((value * mult) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rates() {
        assert_eq!(parse_rate("800"), Some(800));
        assert_eq!(parse_rate("500K"), Some(500 * 1024));
        assert_eq!(parse_rate("2m"), Some(2 * 1024 * 1024));
        assert_eq!(parse_rate("1.5MB/s"), Some(1_572_864));
        assert_eq!(parse_rate("0"), Some(0));
        assert_eq!(parse_rate("fast"), None);
    }

    #[test]
    fn unlimited_never_waits() {
        let l = RateLimiter::new(0);
        assert!(l.reserve(u64::MAX / 2).is_zero());
    }

    #[test]
    fn debt_turns_into_wait() {
        let l = RateLimiter::new(1000);
        // Bucket starts empty: 500 bytes at 1000 B/s is ~0.5 s.
        let wait = l.reserve(500);
        assert!(wait > Duration::from_millis(450) && wait <= Duration::from_millis(500), "{wait:?}");
    }

    #[tokio::test]
    async fn limits_throughput() {
        let l = RateLimiter::new(100_000);
        let start = Instant::now();
        for _ in 0..10 {
            l.acquire(10_000).await;
        }
        // 100 KB at 100 KB/s, starting from an empty bucket.
        assert!(start.elapsed() >= Duration::from_millis(900), "{:?}", start.elapsed());
    }
}
