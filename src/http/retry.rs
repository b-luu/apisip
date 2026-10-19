//! Retry decisions: which responses succeed, which are worth retrying, and
//! how long to wait (`Retry-After` first, then `2^n` seconds plus jitter).

use std::time::Duration;

use rand::RngExt;
use reqwest::header::HeaderMap;

use crate::config::RetryConfig;

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_tries: u32,
    pub max_backoff: Duration,
    /// Total wait allowed between the tries of one request.
    pub max_wait: Duration,
    pub on_status: Vec<u16>,
    /// First backoff step; 1s in production, shorter in tests.
    pub base: Duration,
}

impl From<&RetryConfig> for RetryPolicy {
    fn from(c: &RetryConfig) -> Self {
        RetryPolicy {
            max_tries: c.max_tries.max(1),
            max_backoff: c.max_backoff,
            max_wait: c.max_wait,
            on_status: c.on_status.clone(),
            base: Duration::from_secs(1),
        }
    }
}

impl RetryPolicy {
    pub fn is_success(&self, status: u16) -> bool {
        (200..300).contains(&status)
    }

    pub fn is_retryable(&self, status: u16) -> bool {
        self.on_status.contains(&status)
    }

    /// An error when waiting `delay` more, after `waited`, goes past
    /// `max_wait`. `what` says why the request is retried.
    pub fn check_wait(&self, waited: Duration, delay: Duration, what: &str) -> anyhow::Result<()> {
        if waited.saturating_add(delay) <= self.max_wait {
            return Ok(());
        }
        let secs = |d: Duration| {
            humantime::format_duration(Duration::from_secs(d.as_secs_f64().ceil() as u64))
        };
        anyhow::bail!(
            "{what}, and the next retry would wait {} after {} already waited, past \
             source.retry.max_wait ({}); keep it well under the time between two scheduled runs",
            secs(delay),
            secs(waited),
            secs(self.max_wait)
        )
    }

    /// Delay before try number `attempt + 1` (attempt counts from 0).
    pub fn delay(&self, attempt: u32, headers: Option<&HeaderMap>) -> Duration {
        if let Some(d) = headers.and_then(retry_after) {
            return d;
        }
        let exp = self
            .base
            .saturating_mul(2u32.saturating_pow(attempt.min(20)));
        let jitter = self.base.mul_f64(rand::rng().random_range(0.0..1.0));
        exp.saturating_add(jitter).min(self.max_backoff)
    }
}

/// `Retry-After` in seconds or as an HTTP date.
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let v = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(secs) = v.parse::<f64>() {
        // `inf` or `1e30` is a wait too long to represent: max_wait refuses it.
        return Some(Duration::try_from_secs_f64(secs.max(0.0)).unwrap_or(Duration::MAX));
    }
    let when = chrono::DateTime::parse_from_rfc2822(v).ok()?;
    let secs = (when.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_milliseconds();
    Some(Duration::from_millis(secs.max(0) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn policy() -> RetryPolicy {
        RetryPolicy::from(&RetryConfig::default())
    }

    #[test]
    fn retry_after_wins_over_backoff() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("120"));
        assert_eq!(policy().delay(0, Some(&h)), Duration::from_secs(120));
        let date = (chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc2822();
        h.insert("retry-after", HeaderValue::from_str(&date).unwrap());
        let d = policy().delay(0, Some(&h)).as_secs();
        assert!((28..=30).contains(&d), "{d}");
    }

    #[test]
    fn a_retry_after_too_long_to_represent_is_refused_not_a_panic() {
        let p = policy();
        for v in ["inf", "1e30"] {
            let mut h = HeaderMap::new();
            h.insert("retry-after", HeaderValue::from_static(v));
            let d = p.delay(0, Some(&h));
            assert_eq!(d, Duration::MAX, "{v}");
            assert!(p.check_wait(Duration::ZERO, d, "HTTP 429").is_err(), "{v}");
        }
    }

    #[test]
    fn backoff_doubles_with_jitter_and_caps() {
        let p = policy();
        for (attempt, low) in [(0, 1), (1, 2), (2, 4), (3, 8)] {
            let d = p.delay(attempt, None).as_secs_f64();
            assert!(
                d >= low as f64 && d < low as f64 + 1.0,
                "attempt {attempt}: {d}"
            );
        }
        assert_eq!(p.delay(10, None), Duration::from_secs(60));
    }

    #[test]
    fn predicates() {
        let p = policy();
        assert!(p.is_success(204) && !p.is_success(304));
        assert!(p.is_retryable(429) && p.is_retryable(503) && !p.is_retryable(404));
        assert!(p.is_retryable(408) && p.is_retryable(522));
        assert!(!p.is_retryable(501) && !p.is_retryable(505) && !p.is_retryable(400));
    }
}
