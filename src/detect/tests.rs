use std::time::Duration;

use super::*;
use crate::config::{ApiKeyLocation, Mode, Pagination};

fn load(name: &str) -> Spec {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    Spec::parse(&std::fs::read_to_string(path).unwrap(), name).unwrap()
}

fn stream<'a>(d: &'a Detection, name: &str) -> &'a StreamConfig {
    &d.streams
        .iter()
        .find(|s| s.config.name == name)
        .unwrap_or_else(|| {
            panic!(
                "no stream {name}: {:?}",
                d.streams.iter().map(|s| &s.config.name).collect::<Vec<_>>()
            )
        })
        .config
}

#[test]
fn openapi30_streams_and_skips() {
    let d = detect(&load("openapi30.yaml"));
    let names: Vec<_> = d.streams.iter().map(|s| s.config.name.as_str()).collect();
    assert_eq!(names, ["users", "orders", "products", "events", "issues"]);
    let skipped: Vec<_> = d
        .skipped
        .iter()
        .map(|s| (s.path.as_str(), s.reason.as_str()))
        .collect();
    assert!(skipped.contains(&("/users/{id}", "needs a parent stream (path parameters)")));
    assert!(skipped.contains(&("/search", "requires query parameter `q`")));
    assert!(skipped.contains(&("/me", "returns a single object, not a list")));
    assert_eq!(d.base_url.as_deref(), Some("https://api.acme.test/v1"));
}

#[test]
fn openapi30_pagination_and_modes() {
    let d = detect(&load("openapi30.yaml"));

    let users = stream(&d, "users");
    assert_eq!(users.records, "/data");
    assert_eq!(users.primary_key, ["id"]);
    assert_eq!(
        users.pagination,
        Some(Pagination::PageNumber {
            param: "page".into(),
            size_param: Some("per_page".into()),
            size: Some(250),
            start: 1
        })
    );
    assert_eq!(users.mode(), Mode::Filter);
    assert_eq!(
        users.incremental.as_ref().unwrap().param.as_deref(),
        Some("updated_since")
    );
    // allOf through a $ref chain: fields of both parts are visible.
    let fields = &d.streams[0].fields;
    assert!(fields.contains(&"email".to_string()) && fields.contains(&"updated_at".to_string()));

    let orders = stream(&d, "orders");
    assert_eq!(orders.records, "/orders");
    assert!(
        orders.primary_key.is_empty(),
        "no `id` field: init must ask"
    );
    assert_eq!(
        orders.pagination,
        Some(Pagination::Cursor {
            param: "cursor".into(),
            cursor_path: Some("/meta/next_cursor".into()),
            last_record_field: None,
            size_param: Some("limit".into()),
            size: Some(100)
        })
    );
    assert_eq!(orders.mode(), Mode::EarlyStop);
    assert_eq!(
        orders
            .incremental
            .as_ref()
            .unwrap()
            .sort
            .as_ref()
            .unwrap()
            .value,
        "-updated_at"
    );

    let products = stream(&d, "products");
    assert_eq!(products.records, "");
    assert_eq!(
        products.pagination,
        Some(Pagination::Offset {
            param: "offset".into(),
            limit_param: "limit".into(),
            limit: 100
        })
    );
    assert_eq!(
        products.params.get("currency").map(String::as_str),
        Some("EUR")
    );
    assert_eq!(products.mode(), Mode::FullRefresh);

    assert_eq!(
        stream(&d, "events").pagination,
        Some(Pagination::LinkHeader)
    );

    let issues = stream(&d, "issues");
    let sort = issues.incremental.as_ref().unwrap().sort.as_ref().unwrap();
    assert_eq!(
        (sort.param.as_str(), sort.value.as_str()),
        ("sort", "updated")
    );
    assert_eq!(
        sort.params.get("direction").map(String::as_str),
        Some("desc")
    );
}

#[test]
fn openapi30_auth_and_plan_tiers() {
    let d = detect(&load("openapi30.yaml"));
    assert_eq!(d.env_prefix, "ACME_STORE");
    assert_eq!(d.auth.len(), 1);
    assert_eq!(
        d.auth[0].config,
        Some(AuthConfig::Bearer {
            scheme: Some("bearerAuth".into()),
            token: "${ACME_STORE_TOKEN}".into()
        })
    );
    assert!(d.auth[0].vars[0].secret);

    let plans = d.rate_limits.plans();
    let got: Vec<_> = plans
        .iter()
        .map(|t| (t.plan.as_deref().unwrap(), t.limit.max_calls))
        .collect();
    assert_eq!(got, [("Free", 60), ("Pro", 600)]);
    assert_eq!(plans[0].limit.period, Duration::from_secs(60));
    assert!(d.rate_limits.headers_declared);
}

#[test]
fn openapi31_next_url_filter_and_extension_limit() {
    let spec = load("openapi31.json");
    let d = detect(&spec);
    let readings = stream(&d, "readings");
    assert_eq!(readings.records, "/items");
    assert_eq!(
        readings.pagination,
        Some(Pagination::NextUrl {
            next_path: "/links/next".into()
        })
    );
    let inc = readings.incremental.as_ref().unwrap();
    assert_eq!(
        (inc.cursor.as_str(), inc.param.as_deref()),
        ("modified", Some("modified_after"))
    );

    let devices = d
        .streams
        .iter()
        .find(|s| s.config.name == "devices")
        .unwrap();
    assert!(
        stream_record_schema(&spec, &devices.config).is_none(),
        "no response schema: falls back to inference"
    );
    let skipped: Vec<_> = d
        .skipped
        .iter()
        .map(|s| (s.path.as_str(), s.reason.as_str()))
        .collect();
    assert!(skipped.contains(&("/logout", "declares no response body")));
    assert!(skipped.contains(&("/report.csv", "does not return JSON")));

    assert!(d.rate_limits.plans().is_empty());
    let limit = d.rate_limits.default_limit().unwrap();
    assert_eq!(
        (limit.max_calls, limit.period),
        (10, Duration::from_secs(1))
    );
}

#[test]
fn openapi31_auth_schemes() {
    let d = detect(&load("openapi31.json"));
    let schemes: Vec<_> = d.auth.iter().map(|a| a.scheme.as_str()).collect();
    assert_eq!(schemes, ["apiKeyQuery", "machine", "browser"]);
    assert!(matches!(
        &d.auth[0].config,
        Some(AuthConfig::ApiKey { name, location: ApiKeyLocation::Query, .. }) if name == "api_key"
    ));
    match &d.auth[1].config {
        Some(AuthConfig::Oauth2ClientCredentials {
            token_url,
            client_id,
            scopes,
            ..
        }) => {
            assert_eq!(token_url, "https://telemetry.northwind.test/oauth/token");
            assert_eq!(client_id, "${NORTHWIND_TELEMETRY_MACHINE_CLIENT_ID}");
            assert_eq!(scopes, &["read"]);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(d.auth[2].config.is_none() && d.auth[2].unsupported.is_some());
}

#[test]
fn swagger20_offset_basic_and_prose_limit() {
    let d = detect(&load("swagger20.json"));
    assert_eq!(
        d.base_url.as_deref(),
        Some("https://ledger.example.test/api")
    );
    let entries = stream(&d, "entries");
    assert_eq!(
        entries.pagination,
        Some(Pagination::Offset {
            param: "skip".into(),
            limit_param: "top".into(),
            limit: 500
        })
    );
    assert!(matches!(d.auth[0].config, Some(AuthConfig::Basic { .. })));
    assert_eq!(d.auth[0].vars[0].name, "LEGACY_LEDGER_USERNAME");
    let t = &d.rate_limits.tiers;
    assert_eq!(t.len(), 1);
    assert_eq!(
        (t[0].plan.as_ref(), t[0].limit.max_calls, t[0].limit.period),
        (None, 1000, Duration::from_secs(3600))
    );
}

#[test]
fn record_schema_lookup_matches_detection() {
    let spec = load("openapi30.yaml");
    let d = detect(&spec);
    for s in &d.streams {
        let fields: Vec<String> = stream_record_schema(&spec, &s.config)
            .and_then(|r| {
                r["properties"]
                    .as_object()
                    .map(|p| p.keys().cloned().collect())
            })
            .unwrap_or_default();
        assert_eq!(fields, s.fields, "stream {}", s.config.name);
    }
}

#[test]
fn spec_values_never_reference_the_environment() {
    // A `${VAR}` copied from the spec into apisip.toml would be filled from the
    // environment and sent to the API, e.g. `${AWS_SECRET_ACCESS_KEY}`.
    let doc = serde_json::json!({
        "openapi": "3.0.0",
        "info": {"title": "t", "version": "1"},
        "servers": [{"url": "https://api.test/${AWS_SECRET_ACCESS_KEY}"}],
        "components": {"securitySchemes": {"oauth": {"type": "oauth2", "flows": {"clientCredentials": {
            "tokenUrl": "https://auth.test/token?k=${GITHUB_TOKEN}", "scopes": {}}}}}},
        "paths": {
            "/items": {"get": {
                "parameters": [{"name": "q", "in": "query", "required": true,
                    "schema": {"type": "string", "default": "${AWS_SECRET_ACCESS_KEY}"}}],
                "responses": {"200": {"description": "ok", "content": {"application/json": {
                    "schema": {"type": "array", "items": {"type": "object"}}}}}}}},
            "/plain": {"get": {
                "parameters": [{"name": "q", "in": "query", "required": true,
                    "schema": {"type": "string", "default": "all"}}],
                "responses": {"200": {"description": "ok", "content": {"application/json": {
                    "schema": {"type": "array", "items": {"type": "object"}}}}}}}}
        }
    });
    let d = detect(&Spec::parse(&doc.to_string(), "spec.json").unwrap());

    assert_eq!(d.base_url, None);
    assert_eq!(
        stream(&d, "plain").params.get("q").map(String::as_str),
        Some("all")
    );
    assert!(
        d.streams
            .iter()
            .all(|s| s.config.params.values().all(|v| !v.contains("${")))
    );
    assert!(
        d.skipped
            .iter()
            .any(|s| s.path == "/items" && s.reason.contains("`q`")),
        "{:?}",
        d.skipped
    );
    let oauth = d.auth.iter().find(|a| a.scheme == "oauth").unwrap();
    assert!(oauth.config.is_none());
    assert!(oauth.unsupported.as_deref().unwrap().contains("tokenUrl"));
}

#[test]
fn parameter_and_header_refs_are_resolved_where_read() {
    let doc = serde_json::json!({
        "openapi": "3.0.3",
        "paths": {"/issues": {"get": {
            "parameters": [
                {"name": "state", "in": "query", "required": true,
                 "schema": {"$ref": "#/components/schemas/State"}},
                {"name": "sort", "in": "query", "schema": {"$ref": "#/components/schemas/Sort"}},
                {"name": "direction", "in": "query",
                 "schema": {"type": "array", "items": {"$ref": "#/components/schemas/Dir"}}}
            ],
            "responses": {
                "200": {"description": "ok", "content": {"application/json": {"schema": {
                    "type": "array", "items": {"type": "object", "properties": {
                        "id": {"type": "integer"},
                        "updated_at": {"type": "string", "format": "date-time"}}}}}}},
                "429": {"description": "slow down",
                        "headers": {"Retry-After": {"$ref": "#/components/headers/Limit"}}}
            }
        }}},
        "components": {
            "schemas": {
                "State": {"type": "string", "default": "open"},
                "Sort": {"type": "string", "enum": ["created", "updated"]},
                "Dir": {"type": "string", "enum": ["asc", "desc"]}
            },
            "headers": {"Limit": {"description": "Clients may send up to 1,000 requests per hour.",
                                  "schema": {"type": "integer"}}}
        }
    });
    let d = detect(&Spec::parse(&doc.to_string(), "spec.json").unwrap());

    let issues = stream(&d, "issues");
    assert_eq!(issues.params.get("state").map(String::as_str), Some("open"));
    let sort = issues.incremental.as_ref().unwrap().sort.as_ref().unwrap();
    assert_eq!(
        (sort.param.as_str(), sort.value.as_str()),
        ("sort", "updated")
    );
    assert_eq!(
        sort.params.get("direction").map(String::as_str),
        Some("desc")
    );
    let t = &d.rate_limits.tiers;
    assert_eq!(t.len(), 1, "{t:?}");
    assert_eq!(t[0].limit.max_calls, 1000);
}
