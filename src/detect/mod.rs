//! Turns a spec into stream definitions: which endpoints return data, how they
//! page, how they can be fetched incrementally, how to authenticate, and how
//! fast they may be called.
//!
//! Detection is best-effort. It writes plain TOML that the user can correct,
//! and `apisip check` reports what does not work.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::config::{AuthConfig, Config, Destination, Format, RateLimit, Source, StreamConfig};
use crate::spec::Spec;

mod auth;
mod names;
mod rate_limit;
mod stream;

use auth::detect_auth;
pub use names::{env_prefix, snake, stream_name};
pub use rate_limit::detect_rate_limits;
use stream::detect_stream;
pub use stream::{records_location, stream_record_schema};

#[derive(Debug, Clone)]
pub struct Detection {
    pub title: String,
    pub env_prefix: String,
    pub base_url: Option<String>,
    pub auth: Vec<AuthOption>,
    pub rate_limits: RateLimitFinding,
    pub streams: Vec<DetectedStream>,
    pub skipped: Vec<Skipped>,
}

#[derive(Debug, Clone)]
pub struct AuthOption {
    /// Scheme name in the spec.
    pub scheme: String,
    pub description: String,
    /// `None` when apisip v1 cannot use this scheme.
    pub config: Option<AuthConfig>,
    pub unsupported: Option<String>,
    /// Variables the config references, with whether each one is a secret.
    pub vars: Vec<EnvVar>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub name: String,
    pub label: String,
    pub secret: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RateLimitFinding {
    pub tiers: Vec<Tier>,
    /// The spec declares rate-limit response headers: the API reports its budget at runtime.
    pub headers_declared: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tier {
    pub plan: Option<String>,
    pub limit: RateLimit,
    pub source: String,
}

impl RateLimitFinding {
    /// Distinct named plans, when the spec documents more than one.
    pub fn plans(&self) -> Vec<&Tier> {
        let mut seen = BTreeSet::new();
        let named: Vec<&Tier> = self
            .tiers
            .iter()
            .filter(|t| {
                t.plan
                    .as_ref()
                    .is_some_and(|p| seen.insert(p.to_lowercase()))
            })
            .collect();
        if named.len() >= 2 { named } else { Vec::new() }
    }

    /// The single limit to use when no plan choice is needed: the most conservative one.
    pub fn default_limit(&self) -> Option<RateLimit> {
        self.tiers
            .iter()
            .map(|t| t.limit.clone())
            .min_by(|a, b| a.per_second().total_cmp(&b.per_second()))
    }
}

#[derive(Debug, Clone)]
pub struct DetectedStream {
    pub config: StreamConfig,
    pub method: &'static str,
    /// Inlined schema of one record, when the spec declares it.
    pub record_schema: Option<Value>,
    /// Top-level record fields, for the primary-key prompt.
    pub fields: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Skipped {
    pub path: String,
    pub reason: String,
}

pub fn detect(spec: &Spec) -> Detection {
    let env_prefix = env_prefix(spec.title());
    let mut streams: Vec<DetectedStream> = Vec::new();
    let mut skipped = Vec::new();
    let mut names = BTreeSet::new();

    for op in spec.operations() {
        if op.method != "get" {
            continue;
        }
        if op.path.contains('{') {
            skipped.push(Skipped {
                path: op.path.to_string(),
                reason: "needs a parent stream (path parameters)".into(),
            });
            continue;
        }
        match detect_stream(spec, &op) {
            Ok(mut s) => {
                let mut name = stream_name(op.path);
                if !names.insert(name.clone()) {
                    name = op
                        .op
                        .get("operationId")
                        .and_then(Value::as_str)
                        .map(snake)
                        .filter(|n| !names.contains(n))
                        .unwrap_or_else(|| format!("{name}_{}", names.len()));
                    names.insert(name.clone());
                }
                s.config.name = name;
                streams.push(s);
            }
            Err(reason) => skipped.push(Skipped {
                path: op.path.to_string(),
                reason,
            }),
        }
    }

    Detection {
        title: spec.title().to_string(),
        auth: detect_auth(spec, &env_prefix),
        env_prefix,
        // `${VAR}` in apisip.toml is filled from the environment: never take one from the spec.
        base_url: spec.base_url().filter(|u| !u.contains("${")),
        rate_limits: detect_rate_limits(spec),
        streams,
        skipped,
    }
}

impl Detection {
    /// What detection alone produced, with the auth and source settings of the
    /// config: `update` treats every difference between it and apisip.toml as a
    /// user edit to preserve.
    pub fn baseline(&self, source: &Source) -> Config {
        Config {
            source: Source {
                base_url: self
                    .base_url
                    .clone()
                    .unwrap_or_else(|| source.base_url.clone()),
                // A plan picked at init is the user's choice, not detection's.
                rate_limit: if self.rate_limits.plans().is_empty() {
                    self.rate_limits.default_limit()
                } else {
                    None
                },
                ..source.clone()
            },
            destination: Destination {
                url: crate::config::DEFAULT_DESTINATION.into(),
                format: Format::Jsonl,
                rotate: Default::default(),
                endpoint: None,
                region: None,
            },
            streams: self.streams.iter().map(|s| s.config.clone()).collect(),
        }
    }
}

#[cfg(test)]
mod tests;
