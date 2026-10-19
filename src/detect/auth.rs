//! The auth options a spec offers, from its security schemes.

use std::collections::BTreeMap;

use serde_json::Value;

use super::names::snake;
use super::{AuthOption, EnvVar};
use crate::config::{ApiKeyLocation, AuthConfig};
use crate::spec::Spec;

pub(super) fn detect_auth(spec: &Spec, prefix: &str) -> Vec<AuthOption> {
    let mut schemes = spec.security_schemes();
    let preferred = spec.default_security();
    schemes.sort_by_key(|(name, _)| {
        preferred
            .iter()
            .position(|p| p == name)
            .unwrap_or(usize::MAX)
    });

    let kind_of = |s: &Value| -> &'static str {
        let t = s.get("type").and_then(Value::as_str).unwrap_or("");
        let scheme = s
            .get("scheme")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        match (t, scheme.as_str()) {
            ("http", "bearer") => "bearer",
            ("http", "basic") | ("basic", _) => "basic",
            ("apiKey", _) => "api_key",
            ("oauth2", _) => "oauth2",
            _ => "other",
        }
    };
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, s) in &schemes {
        *counts.entry(kind_of(s)).or_default() += 1;
    }

    schemes
        .iter()
        .map(|(name, s)| {
            let kind = kind_of(s);
            let p = if counts.get(kind).copied().unwrap_or(0) > 1 {
                format!("{prefix}_{}", snake(name).to_ascii_uppercase())
            } else {
                prefix.to_string()
            };
            let var = |suffix: &str, label: &str, secret: bool| EnvVar {
                name: format!("{p}_{suffix}"),
                label: label.to_string(),
                secret,
            };
            let reference = |v: &EnvVar| format!("${{{}}}", v.name);
            let description = s
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            let scheme = Some(name.clone());
            let mut opt = AuthOption {
                scheme: name.clone(),
                description,
                config: None,
                unsupported: None,
                vars: Vec::new(),
            };
            match kind {
                "bearer" => {
                    let t = var("TOKEN", "Bearer token", true);
                    opt.config = Some(AuthConfig::Bearer {
                        scheme,
                        token: reference(&t),
                    });
                    opt.vars = vec![t];
                }
                "basic" => {
                    let u = var("USERNAME", "Username", false);
                    let pw = var("PASSWORD", "Password", true);
                    opt.config = Some(AuthConfig::Basic {
                        scheme,
                        username: reference(&u),
                        password: reference(&pw),
                    });
                    opt.vars = vec![u, pw];
                }
                "api_key" => {
                    let location = match s.get("in").and_then(Value::as_str) {
                        Some("query") => ApiKeyLocation::Query,
                        Some("cookie") => ApiKeyLocation::Cookie,
                        _ => ApiKeyLocation::Header,
                    };
                    let k = var("API_KEY", "API key", true);
                    opt.config = Some(AuthConfig::ApiKey {
                        scheme,
                        name: s
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("X-API-Key")
                            .to_string(),
                        location,
                        value: reference(&k),
                    });
                    opt.vars = vec![k];
                }
                "oauth2" => {
                    let flow = s.pointer("/flows/clientCredentials").or_else(|| {
                        (s.get("flow").and_then(Value::as_str) == Some("application")).then_some(s)
                    });
                    match flow
                        .and_then(|f| f.get("tokenUrl").and_then(Value::as_str).map(|u| (f, u)))
                    {
                        Some((flow, token_url)) => {
                            let id = var("CLIENT_ID", "OAuth2 client id", false);
                            let secret = var("CLIENT_SECRET", "OAuth2 client secret", true);
                            let scopes = flow
                                .get("scopes")
                                .and_then(Value::as_object)
                                .map(|m| m.keys().cloned().collect())
                                .unwrap_or_default();
                            opt.config = Some(AuthConfig::Oauth2ClientCredentials {
                                scheme,
                                token_url: token_url.to_string(),
                                client_id: reference(&id),
                                client_secret: reference(&secret),
                                scopes,
                            });
                            opt.vars = vec![id, secret];
                        }
                        None => {
                            opt.unsupported = Some(
                                "OAuth2 without a client-credentials flow is not supported yet"
                                    .into(),
                            );
                        }
                    }
                }
                _ => {
                    opt.unsupported = Some(format!(
                        "scheme type `{}` is not supported yet",
                        s.get("type").and_then(Value::as_str).unwrap_or("?")
                    ));
                }
            }
            opt
        })
        .collect()
}
