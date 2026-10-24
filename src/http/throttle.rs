//! Rolling-window throttle: at most N calls in any window of length T,
//! shared by every stream of one API. It also pauses when live rate-limit
//! headers say the budget is spent.

use std::collections::VecDeque;
use std::time::Duration;

use reqwest::header::HeaderMap;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::config::RateLimit;

/// Longest pause apisip accepts from a header before it caps the wait.
const MAX_HEADER_WAIT: Duration = Duration::from_secs(3600);

#[derive(Debug)]
pub struct Throttle {
    limit: Option<(usize, Duration)>,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    calls: VecDeque<Instant>,
    blocked_until: Option<Instant>,
}

impl Throttle {
    pub fn new(limit: Option<&RateLimit>) -> Throttle {
        Throttle {
            limit: limit.map(|l| (l.max_calls.max(1) as usize, l.period)),
            state: Mutex::new(State::default()),
        }
    }

    /// Waits until one more call fits in the window, then records it.
    pub async fn acquire(&self) {
        loop {
            let wait_until = {
                let mut s = self.state.lock().await;
                let now = Instant::now();
                match s.blocked_until {
                    Some(t) if t > now => Some(t),
                    _ => {
                        s.blocked_until = None;
                        match self.limit {
                            None => {
                                return;
                            }
                            Some((max, period)) => {
                                while s
                                    .calls
                                    .front()
                                    .is_some_and(|t| now.duration_since(*t) >= period)
                                {
                                    s.calls.pop_front();
                                }
                                if s.calls.len() < max {
                                    s.calls.push_back(now);
                                    return;
                                }
                                s.calls.front().map(|t| *t + period)
                            }
                        }
                    }
                }
            };
            if let Some(t) = wait_until {
                tokio::time::sleep_until(t).await;
            }
        }
    }

    /// Pauses every caller until `delay` has passed (from a 429 Retry-After).
    pub async fn block_for(&self, delay: Duration) {
        let until = Instant::now() + delay.min(MAX_HEADER_WAIT);
        let mut s = self.state.lock().await;
        if s.blocked_until.is_none_or(|t| t < until) {
            s.blocked_until = Some(until);
        }
    }

    /// Reads budget headers from a response. When the remaining budget is
    /// zero, pauses until the advertised reset.
    pub async fn observe(&self, headers: &HeaderMap) {
        if let Some(reset) = budget_reset(headers) {
            tracing::info!(
                "rate-limit budget spent, waiting {}s for the reset",
                reset.as_secs()
            );
            self.block_for(reset).await;
        }
    }
}

/// The pause the headers ask for when the remaining budget is zero, capped
/// like `block_for`; `None` when calls remain.
pub fn budget_reset(headers: &HeaderMap) -> Option<Duration> {
    let (remaining, reset) = budget(headers)?;
    (remaining == 0).then(|| reset.unwrap_or(Duration::from_secs(1)).min(MAX_HEADER_WAIT))
}

fn header_u64(headers: &HeaderMap, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|n| {
        headers
            .get(*n)?
            .to_str()
            .ok()?
            .split([',', ';'])
            .next()?
            .trim()
            .parse::<f64>()
            .ok()
            .map(|v| v.max(0.0) as u64)
    })
}

/// Remaining calls and time to reset, from `X-RateLimit-*`, `RateLimit-*`,
/// or the combined `RateLimit: limit=.., remaining=.., reset=..` header.
pub fn budget(headers: &HeaderMap) -> Option<(u64, Option<Duration>)> {
    let mut remaining = header_u64(
        headers,
        &[
            "x-ratelimit-remaining",
            "ratelimit-remaining",
            "x-rate-limit-remaining",
        ],
    );
    let mut reset = header_u64(
        headers,
        &["x-ratelimit-reset", "ratelimit-reset", "x-rate-limit-reset"],
    );
    if let Some(combined) = headers.get("ratelimit").and_then(|v| v.to_str().ok()) {
        for part in combined.split([',', ';']) {
            if let Some((k, v)) = part.trim().split_once('=') {
                let v = v.trim().trim_matches('"').parse::<u64>().ok();
                match k.trim() {
                    "remaining" | "r" => remaining = remaining.or(v),
                    "reset" | "t" => reset = reset.or(v),
                    _ => {}
                }
            }
        }
    }
    let reset = reset.map(|r| {
        // Large values are Unix timestamps, small ones are seconds from now.
        if r > 1_000_000_000 {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            Duration::from_secs(r.saturating_sub(now).max(1))
        } else {
            Duration::from_secs(r.max(1))
        }
    });
    remaining.map(|r| (r, reset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[tokio::test(start_paused = true)]
    async fn window_spreads_calls() {
        let t = Throttle::new(Some(&RateLimit {
            max_calls: 2,
            period: Duration::from_secs(10),
        }));
        let start = Instant::now();
        for _ in 0..5 {
            t.acquire().await;
        }
        // Calls 1-2 at 0s, 3-4 at 10s, 5 at 20s.
        assert_eq!(start.elapsed().as_secs(), 20);
    }

    #[tokio::test(start_paused = true)]
    async fn no_limit_never_waits() {
        let t = Throttle::new(None);
        let start = Instant::now();
        for _ in 0..1000 {
            t.acquire().await;
        }
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn spent_budget_pauses_until_reset() {
        let t = Throttle::new(None);
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        h.insert("x-ratelimit-reset", HeaderValue::from_static("30"));
        t.observe(&h).await;
        let start = Instant::now();
        t.acquire().await;
        assert_eq!(start.elapsed().as_secs(), 30);
        // A budget that is not spent does not pause.
        h.insert("x-ratelimit-remaining", HeaderValue::from_static("5"));
        t.observe(&h).await;
        let start = Instant::now();
        t.acquire().await;
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[test]
    fn budget_header_shapes() {
        let mut h = HeaderMap::new();
        h.insert(
            "ratelimit",
            HeaderValue::from_static("limit=100, remaining=0, reset=7"),
        );
        assert_eq!(budget(&h), Some((0, Some(Duration::from_secs(7)))));
        let mut h = HeaderMap::new();
        h.insert("ratelimit-remaining", HeaderValue::from_static("12"));
        assert_eq!(budget(&h), Some((12, None)));
        assert_eq!(budget(&HeaderMap::new()), None);
    }
}
