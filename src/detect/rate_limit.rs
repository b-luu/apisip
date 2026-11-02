//! Rate limits the spec states: headers, vendor extensions, or prose in the
//! descriptions.

use std::collections::HashSet;
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
    let mut f = collect(spec);
    let mut seen = HashSet::new();
    f.tiers.retain(|t| {
        let plan = t.plan.as_ref().map(|p| p.to_lowercase());
        seen.insert((plan, t.limit.max_calls, t.limit.period))
    });
    f
}

/// Every tier the spec states, before deduplication. A text many operations
/// share through `$ref` or a YAML alias is parsed once, so its tiers appear once.
fn collect(spec: &Spec) -> RateLimitFinding {
    let mut f = RateLimitFinding::default();
    // Keyed by content: a YAML alias copies the text to a new address.
    let mut parsed: HashSet<&str> = HashSet::new();

    // 1. Vendor extensions anywhere in the document.
    walk_extensions(&spec.root, 0, &mut f.tiers);

    // 2. Declared rate-limit response headers, on success and 429 responses.
    for op in spec.operations() {
        let throttled = op.op.pointer("/responses/429").map(|r| spec.resolve(r));
        if let Some(d) = throttled
            .and_then(|r| r.get("description"))
            .and_then(Value::as_str)
            && parsed.insert(d)
        {
            parse_text(d, "429 response description", &mut f.tiers);
        }
        let headers = [spec.success_response(op.op), throttled]
            .into_iter()
            .flatten()
            .filter_map(|r| r.get("headers").and_then(Value::as_object))
            .flatten();
        for (name, h) in headers {
            let name = name.to_ascii_lowercase();
            if RATE_HEADERS.contains(&name.as_str()) {
                f.headers_declared = true;
            }
            if let Some(d) = spec.resolve(h).get("description").and_then(Value::as_str)
                && parsed.insert(d)
            {
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
        // Paths that share a path item through `$ref` share its operations.
        let mut ops: HashSet<*const Value> = HashSet::new();
        for op in spec.operations() {
            if ops.insert(op.op) {
                parse_text(
                    &op.description(),
                    &format!("{} {}", op.method.to_uppercase(), op.path),
                    &mut f.tiers,
                );
            }
        }
    }
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
    // A plan name is a few words; this bound keeps a long line with many matches linear.
    if prefix.len() > 256 {
        return None;
    }
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

    fn spec(paths: serde_json::Value, components: serde_json::Value) -> Spec {
        let doc = serde_json::json!({
            "openapi": "3.1.0",
            "info": {"title": "t", "version": "1"},
            "paths": paths,
            "components": components,
        });
        Spec::parse(&doc.to_string(), "spec.json").unwrap()
    }

    #[test]
    fn shared_response_text_is_parsed_once() {
        let op = serde_json::json!({"get": {"responses": {
            "200": {"description": "", "headers": {
                "X-RateLimit-Limit": {"$ref": "#/components/headers/L"}
            }},
            "429": {"$ref": "#/components/responses/R"},
        }}});
        let s = spec(
            serde_json::json!({"/a": op, "/b": op, "/c": op}),
            serde_json::json!({
                "responses": {"R": {"description": "10 requests per second"}},
                "headers": {"L": {"description": "100 requests per hour"}},
            }),
        );
        let f = collect(&s);
        assert!(f.headers_declared);
        assert_eq!(f.tiers.len(), 2, "{:?}", f.tiers);
        assert_eq!(f.tiers[0].source, "429 response description");
        assert_eq!(f.tiers[1].source, "header x-ratelimit-limit");
        assert_eq!(detect_rate_limits(&s).tiers, f.tiers);
    }

    #[test]
    fn yaml_aliased_response_text_is_parsed_once() {
        let paths: String = (0..50)
            .map(|i| format!("  /p{i}:\n    get:\n      responses:\n        '429': *throttled\n"))
            .collect();
        let s = Spec::parse(
            &format!(
                "openapi: 3.0.3\ninfo: {{title: t, version: '1'}}\n\
                 x-throttled: &throttled\n  description: 10 requests per second\n  \
                 headers: {{X-RateLimit-Limit: {{description: 100 requests per hour}}}}\n\
                 paths:\n{paths}"
            ),
            "spec.yaml",
        )
        .unwrap();
        let f = collect(&s);
        assert!(f.headers_declared);
        assert_eq!(f.tiers.len(), 2, "{:?}", f.tiers);
        assert_eq!(f.tiers[0].source, "429 response description");
        assert_eq!(f.tiers[1].source, "header x-ratelimit-limit");
    }

    #[test]
    fn shared_path_item_is_parsed_once() {
        let s = spec(
            serde_json::json!({
                "/a": {"$ref": "#/components/pathItems/P"},
                "/b": {"$ref": "#/components/pathItems/P"},
            }),
            serde_json::json!({"pathItems": {"P": {"get": {
                "description": "5 calls per minute",
                "responses": {"200": {"description": ""}},
            }}}}),
        );
        let f = collect(&s);
        assert_eq!(f.tiers.len(), 1, "{:?}", f.tiers);
        assert_eq!(f.tiers[0].source, "GET /a");
    }

    #[test]
    fn plan_name_reads_a_short_prefix_only() {
        let line = |pad: usize| format!("- Gold{} 10 requests per second", " ".repeat(pad));
        let plan = |line: &str| {
            let mut out = Vec::new();
            parse_text(line, "t", &mut out);
            out[0].plan.clone()
        };
        // "- Gold", 249 padding spaces and one space: 256 bytes before the match.
        assert_eq!(plan(&line(249)).as_deref(), Some("Gold"));
        assert_eq!(plan(&line(250)), None);
    }
}
