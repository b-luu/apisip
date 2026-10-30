//! Rate limits the spec states: headers, vendor extensions, or prose in the
//! descriptions.

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

use super::names::norm;
use super::{RateLimitFinding, Tier};
use crate::config::RateLimit;
use crate::spec::Spec;

static RATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(\d[\d,]*)\s*(?:api\s+)?(?:requests?|calls?|reqs?|hits?)\s*(?:per|/|an?|each|every)\s*(\d+\s*)?(seconds?|secs?|minutes?|mins?|hours?|hrs?|days?|s|m|h|d)\b",
    )
    .unwrap()
});

const RATE_HEADERS: [&str; 8] = [
    "x-ratelimit-limit",
    "x-rate-limit-limit",
    "ratelimit-limit",
    "ratelimit-policy",
    "ratelimit",
    "x-ratelimit-remaining",
    "ratelimit-remaining",
    "x-rate-limit-remaining",
];

pub fn detect_rate_limits(spec: &Spec) -> RateLimitFinding {
    let mut f = RateLimitFinding::default();

    // 1. Vendor extensions anywhere in the document.
    walk_extensions(&spec.root, 0, &mut f.tiers);

    // 2. Declared rate-limit response headers, on success and 429 responses.
    for op in spec.operations() {
        let mut headers = spec.response_headers(op.op);
        if let Some(r) = op.op.pointer("/responses/429") {
            let r = spec.resolve(r);
            if let Some(h) = r.get("headers").and_then(Value::as_object) {
                headers.extend(
                    h.iter()
                        .map(|(k, v)| (k.to_ascii_lowercase(), spec.shallow(v))),
                );
            }
            if let Some(d) = r.get("description").and_then(Value::as_str) {
                parse_text(d, "429 response description", &mut f.tiers);
            }
        }
        for (name, h) in &headers {
            if RATE_HEADERS.contains(&name.as_str()) {
                f.headers_declared = true;
            }
            if let Some(d) = h.get("description").and_then(Value::as_str) {
                parse_text(d, &format!("header {name}"), &mut f.tiers);
            }
        }
    }

    // 3. Description text.
    if let Some(d) = spec
        .root
        .pointer("/info/description")
        .and_then(Value::as_str)
    {
        parse_text(d, "info.description", &mut f.tiers);
    }
    if f.tiers.is_empty() {
        for op in spec.operations() {
            parse_text(
                &op.description(),
                &format!("{} {}", op.method.to_uppercase(), op.path),
                &mut f.tiers,
            );
        }
    }

    let mut seen = Vec::new();
    f.tiers.retain(|t| {
        let key = (t.plan.clone().map(|p| p.to_lowercase()), t.limit.clone());
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    f
}

fn walk_extensions(v: &Value, depth: usize, out: &mut Vec<Tier>) {
    if depth > 6 {
        return;
    }
    match v {
        Value::Object(m) => {
            for (k, child) in m {
                let n = norm(k);
                if n.starts_with('x') && (n.contains("ratelimit") || n.contains("throttl")) {
                    parse_extension(child, None, k, out);
                } else {
                    walk_extensions(child, depth + 1, out);
                }
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|i| walk_extensions(i, depth + 1, out)),
        _ => {}
    }
}

fn parse_extension(v: &Value, plan: Option<String>, source: &str, out: &mut Vec<Tier>) {
    match v {
        Value::String(s) => {
            let before = out.len();
            parse_text(s, source, out);
            if let Some(p) = plan {
                for t in &mut out[before..] {
                    t.plan.get_or_insert(p.clone());
                }
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|i| parse_extension(i, plan.clone(), source, out)),
        Value::Object(m) => {
            let get = |keys: &[&str]| {
                m.iter()
                    .find(|(k, _)| keys.contains(&norm(k).as_str()))
                    .map(|(_, v)| v)
            };
            let plan = get(&["plan", "tier", "name", "label"])
                .and_then(Value::as_str)
                .map(str::to_string)
                .or(plan);
            for (k, child) in m {
                let n = norm(k);
                if let Some(unit) = n
                    .strip_prefix("requestsper")
                    .or_else(|| n.strip_prefix("callsper"))
                    .or_else(|| n.strip_prefix("per"))
                    && let (Some(period), Some(calls)) = (parse_period_word(unit), child.as_u64())
                {
                    out.push(tier(plan.clone(), calls, period, source));
                }
            }
            let calls = get(&[
                "limit",
                "max",
                "maxcalls",
                "maxrequests",
                "requests",
                "calls",
                "rate",
                "count",
                "quota",
            ])
            .and_then(Value::as_u64);
            let period = get(&[
                "period",
                "window",
                "interval",
                "per",
                "unit",
                "duration",
                "timewindow",
                "windowseconds",
                "seconds",
            ])
            .and_then(|p| match p {
                Value::String(s) => {
                    parse_period_word(&norm(s)).or_else(|| humantime::parse_duration(s).ok())
                }
                Value::Number(n) => n.as_u64().map(Duration::from_secs),
                _ => None,
            });
            if let (Some(c), Some(p)) = (calls, period) {
                out.push(tier(plan.clone(), c, p, source));
            }
            for (k, child) in m {
                if child.is_object() || child.is_array() {
                    let child_plan = if ["tiers", "plans", "limits"].contains(&norm(k).as_str()) {
                        plan.clone()
                    } else if child.is_object() && !k.starts_with('x') {
                        Some(k.clone())
                    } else {
                        plan.clone()
                    };
                    parse_extension(child, child_plan, source, out);
                }
            }
        }
        _ => {}
    }
}

fn tier(plan: Option<String>, calls: u64, period: Duration, source: &str) -> Tier {
    Tier {
        plan,
        limit: RateLimit {
            max_calls: calls.min(u32::MAX as u64) as u32,
            period,
        },
        source: source.to_string(),
    }
}

fn parse_period_word(w: &str) -> Option<Duration> {
    let w = w.trim().to_ascii_lowercase();
    let secs = match w.as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86400,
        _ => return None,
    };
    Some(Duration::from_secs(secs))
}

/// Finds "N requests per <unit>" in prose, with a plan name when the line names one.
fn parse_text(text: &str, source: &str, out: &mut Vec<Tier>) {
    for line in text.lines() {
        for m in RATE_RE.captures_iter(line) {
            let Ok(calls) = m[1].replace(',', "").parse::<u64>() else {
                continue;
            };
            let mult: u64 = m
                .get(2)
                .and_then(|g| g.as_str().trim().parse().ok())
                .unwrap_or(1);
            let Some(unit) = parse_period_word(&m[3]) else {
                continue;
            };
            let prefix = &line[..m.get(0).unwrap().start()];
            out.push(tier(
                plan_name(line, prefix),
                calls,
                unit * mult as u32,
                source,
            ));
        }
    }
}

fn plan_name(line: &str, prefix: &str) -> Option<String> {
    let cleaned: String = prefix
        .chars()
        .map(|c| {
            if matches!(c, '*' | '_' | '|' | '#' | '`' | ':' | '-' | '(' | ')') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    let lower = cleaned.to_lowercase();
    let listed = {
        let t = line.trim_start();
        t.starts_with(['-', '*', '|', '+'])
            || t.chars().next().is_some_and(|c| c.is_ascii_digit()) && t.contains(". ")
    };
    let named = lower.contains("plan") || lower.contains("tier");
    if words.is_empty() || words.len() > 4 || !(named || listed) {
        return None;
    }
    let name: Vec<&str> = words
        .into_iter()
        .filter(|w| {
            !matches!(
                w.to_lowercase().as_str(),
                "plan" | "tier" | "plans" | "tiers" | "the" | "on"
            )
        })
        .collect();
    if name.is_empty() || name.iter().any(|w| w.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    Some(name.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prose_rate_limits() {
        let mut out = Vec::new();
        parse_text("You can make up to 5,000 requests per hour.", "t", &mut out);
        parse_text("| Enterprise | 100 calls per 10 seconds |", "t", &mut out);
        assert_eq!(out[0].limit.max_calls, 5000);
        assert_eq!(out[0].plan, None);
        assert_eq!(out[1].plan.as_deref(), Some("Enterprise"));
        assert_eq!(out[1].limit.period, Duration::from_secs(10));
    }
}
