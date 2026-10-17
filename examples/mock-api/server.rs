//! A small fake API with its own OpenAPI spec, for the end-to-end tests and
//! the README quickstart. Everything here is synthetic.
//!
//! - `GET /openapi.json` (v1) and `GET /v2/openapi.json` (v2, for `update`).
//! - Auth: `Authorization: Bearer mock-token`, or a token from
//!   `POST /oauth/token` (client credentials `mock-client` / `mock-secret`).
//! - Streams, one per pagination style:
//!   - `/users`: page number, filter param `updated_since`; one undeclared
//!     field and one mistyped value; the first request gets a 429;
//!   - `/orders`: offset, sortable by `-updated_at` (early stop);
//!   - `/events`: cursor, claims to sort by `-updated_at` but does not;
//!   - `/plans`: next URL, no cursor (full refresh).
//! - Not in the spec: `POST /_admin/touch/{resource}/{id}` updates a record,
//!   `POST /_admin/add/users` adds one (both take `?at=<updated_at>` to reuse
//!   a timestamp instead of advancing the clock), `POST /_admin/stray/{resource}/{id}`
//!   sets a record's `updated_at` to `"N/A"`, `POST /_admin/clear/{resource}`
//!   empties a list, `GET /_admin/requests` counts calls.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde_json::{Value, json};

pub const TOKEN: &str = "mock-token";
pub const CLIENT_ID: &str = "mock-client";
pub const CLIENT_SECRET: &str = "mock-secret";

#[derive(Default)]
struct Data {
    clock: u64,
    users: Vec<Value>,
    orders: Vec<Value>,
    events: Vec<Value>,
    plans: Vec<Value>,
    requests: BTreeMap<String, u64>,
}

impl Data {
    /// A logical clock: each tick is one minute after 2026-01-01T00:00:00Z.
    fn tick(&mut self) -> String {
        self.clock += 1;
        let t = chrono::DateTime::<chrono::Utc>::from_timestamp(1_767_225_600, 0).unwrap()
            + chrono::Duration::minutes(self.clock as i64);
        t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    fn seed() -> Data {
        let mut d = Data::default();
        for id in 1..=25 {
            let mut u = json!({
                "id": id,
                "name": format!("user {id}"),
                "email": format!("user{id}@example.test"),
                "age": 20 + id,
                "address": {"city": if id % 2 == 0 { "Lyon" } else { "Nantes" }},
                "updated_at": d.tick(),
            });
            if id == 7 {
                u["nickname"] = json!("seven");
            }
            if id == 13 {
                u["age"] = json!("41");
            }
            d.users.push(u);
        }
        for id in 1..=30 {
            let t = d.tick();
            d.orders.push(json!({
                "id": id,
                "user_id": 1 + id % 25,
                "amount": id as f64 * 9.5,
                "status": "paid",
                "created_on": "2026-01-01",
                "updated_at": t,
            }));
        }
        for id in 1..=20 {
            let t = d.tick();
            d.events
                .push(json!({"id": id, "kind": "click", "updated_at": t}));
        }
        for id in 1..=6 {
            d.plans
                .push(json!({"id": id, "name": format!("plan {id}"), "price": id * 10}));
        }
        d
    }
}

#[derive(Clone)]
struct App {
    data: Arc<Mutex<Data>>,
    base: String,
    throttle_users_once: Arc<AtomicBool>,
    issued: Arc<AtomicU64>,
}

/// Serves on `addr` (port 0 picks a free one). Returns the bound address.
pub async fn serve(addr: SocketAddr) -> std::io::Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let app = App {
        data: Arc::new(Mutex::new(Data::seed())),
        base: format!("http://{local}"),
        throttle_users_once: Arc::new(AtomicBool::new(true)),
        issued: Arc::new(AtomicU64::new(0)),
    };
    let api = Router::new()
        .route("/users", get(users))
        .route("/orders", get(orders))
        .route("/events", get(events))
        .route("/plans", get(plans))
        .route("/projects", get(projects))
        .route_layer(middleware::from_fn_with_state(app.clone(), authorize));
    let router = Router::new()
        .route("/openapi.json", get(spec_v1))
        .route("/v2/openapi.json", get(spec_v2))
        .route("/oauth/token", post(token))
        .route("/_admin/touch/{resource}/{id}", post(touch))
        .route("/_admin/add/users", post(add_user))
        .route("/_admin/stray/{resource}/{id}", post(stray))
        .route("/_admin/clear/{resource}", post(clear))
        .route("/_admin/requests", get(requests))
        .merge(api)
        .with_state(app);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Ok(local)
}

async fn authorize(State(app): State<App>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| {
            t == TOKEN
                || t.strip_prefix("oauth-")
                    .and_then(|n| n.parse::<u64>().ok())
                    .is_some_and(|n| n >= 1 && n <= app.issued.load(Ordering::SeqCst))
        });
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "missing or invalid token"})),
        )
            .into_response();
    }
    *app.data
        .lock()
        .unwrap()
        .requests
        .entry(req.uri().path().to_string())
        .or_default() += 1;
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert("x-ratelimit-limit", "1000".parse().unwrap());
    h.insert("x-ratelimit-remaining", "999".parse().unwrap());
    h.insert("x-ratelimit-reset", "60".parse().unwrap());
    resp
}

async fn token(
    State(app): State<App>,
    headers: HeaderMap,
    Form(form): Form<BTreeMap<String, String>>,
) -> Response {
    use base64_lite::decode;
    let basic = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(decode);
    let posted = form
        .get("client_id")
        .zip(form.get("client_secret"))
        .map(|(a, b)| format!("{a}:{b}"));
    let expected = format!("{CLIENT_ID}:{CLIENT_SECRET}");
    if form.get("grant_type").map(String::as_str) != Some("client_credentials")
        || (basic.as_deref() != Some(expected.as_str())
            && posted.as_deref() != Some(expected.as_str()))
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid_client"})),
        )
            .into_response();
    }
    let n = app.issued.fetch_add(1, Ordering::SeqCst) + 1;
    Json(json!({"access_token": format!("oauth-{n}"), "token_type": "Bearer", "expires_in": 3600}))
        .into_response()
}

fn page(items: &[Value], start: usize, size: usize) -> Vec<Value> {
    items.iter().skip(start).take(size).cloned().collect()
}

fn param<T: std::str::FromStr>(q: &BTreeMap<String, String>, k: &str, default: T) -> T {
    q.get(k).and_then(|v| v.parse().ok()).unwrap_or(default)
}

async fn users(State(app): State<App>, Query(q): Query<BTreeMap<String, String>>) -> Response {
    if app.throttle_users_once.swap(false, Ordering::SeqCst) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "1")],
            Json(json!({"error": "slow down"})),
        )
            .into_response();
    }
    let d = app.data.lock().unwrap();
    let since = q.get("updated_since").cloned().unwrap_or_default();
    let matching: Vec<Value> = d
        .users
        .iter()
        .filter(|u| u["updated_at"].as_str().unwrap() >= since.as_str())
        .cloned()
        .collect();
    let size = param(&q, "per_page", 10usize).min(20);
    let p = param(&q, "page", 1usize).max(1);
    Json(json!({"data": page(&matching, (p - 1) * size, size), "total": matching.len()}))
        .into_response()
}

fn by_updated(items: &[Value], desc: bool) -> Vec<Value> {
    let mut v = items.to_vec();
    v.sort_by(|a, b| {
        a["updated_at"]
            .as_str()
            .cmp(&b["updated_at"].as_str())
            .then(a["id"].as_i64().cmp(&b["id"].as_i64()))
    });
    if desc {
        v.reverse();
    }
    v
}

async fn orders(State(app): State<App>, Query(q): Query<BTreeMap<String, String>>) -> Response {
    let d = app.data.lock().unwrap();
    let sorted = match q.get("sort").map(String::as_str) {
        Some("-updated_at") => by_updated(&d.orders, true),
        Some("updated_at") => by_updated(&d.orders, false),
        _ => d.orders.clone(),
    };
    let limit = param(&q, "limit", 10usize).min(25);
    let offset = param(&q, "offset", 0usize);
    Json(page(&sorted, offset, limit)).into_response()
}

async fn events(State(app): State<App>, Query(q): Query<BTreeMap<String, String>>) -> Response {
    let d = app.data.lock().unwrap();
    // Ignores `sort`: always ascending, although the spec says otherwise.
    let sorted = by_updated(&d.events, false);
    let limit = param(&q, "limit", 5usize).min(5);
    let start = param(&q, "cursor", 0usize);
    let items = page(&sorted, start, limit);
    let next = (start + limit < sorted.len()).then(|| (start + limit).to_string());
    Json(json!({"items": items, "next_cursor": next})).into_response()
}

async fn plans(State(app): State<App>, Query(q): Query<BTreeMap<String, String>>) -> Response {
    let d = app.data.lock().unwrap();
    let p = param(&q, "page", 1usize).max(1);
    let size = 4;
    let next = (p * size < d.plans.len()).then(|| format!("{}/plans?page={}", app.base, p + 1));
    Json(json!({"results": page(&d.plans, (p - 1) * size, size), "next": next})).into_response()
}

async fn projects() -> Response {
    Json(json!([{"id": 1, "title": "alpha"}, {"id": 2, "title": "beta"}])).into_response()
}

async fn touch(
    State(app): State<App>,
    Path((resource, id)): Path<(String, i64)>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Response {
    let mut d = app.data.lock().unwrap();
    let now = q.get("at").cloned().unwrap_or_else(|| d.tick());
    let list = match resource.as_str() {
        "users" => &mut d.users,
        "orders" => &mut d.orders,
        "events" => &mut d.events,
        "plans" => &mut d.plans,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let Some(r) = list.iter_mut().find(|r| r["id"] == id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if r.get("updated_at").is_some() {
        r["updated_at"] = json!(now);
    }
    let name = r
        .get("name")
        .and_then(Value::as_str)
        .map(|n| format!("{n} (edited)"));
    if let Some(n) = name {
        r["name"] = json!(n);
    } else if let Some(a) = r.get("amount").and_then(Value::as_f64) {
        r["amount"] = json!(a + 1.0);
    }
    Json(r.clone()).into_response()
}

async fn stray(State(app): State<App>, Path((resource, id)): Path<(String, i64)>) -> Response {
    let mut d = app.data.lock().unwrap();
    let list = match resource.as_str() {
        "users" => &mut d.users,
        "orders" => &mut d.orders,
        "events" => &mut d.events,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let Some(r) = list.iter_mut().find(|r| r["id"] == id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    r["updated_at"] = json!("N/A");
    Json(r.clone()).into_response()
}

async fn clear(State(app): State<App>, Path(resource): Path<String>) -> Response {
    let mut d = app.data.lock().unwrap();
    match resource.as_str() {
        "users" => d.users.clear(),
        "orders" => d.orders.clear(),
        "events" => d.events.clear(),
        "plans" => d.plans.clear(),
        _ => return StatusCode::NOT_FOUND.into_response(),
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn add_user(State(app): State<App>, Query(q): Query<BTreeMap<String, String>>) -> Response {
    let mut d = app.data.lock().unwrap();
    let id = d.users.len() as i64 + 1;
    let now = q.get("at").cloned().unwrap_or_else(|| d.tick());
    let u = json!({
        "id": id, "name": format!("user {id}"), "email": format!("user{id}@example.test"),
        "age": 30, "address": {"city": "Lille"}, "updated_at": now,
    });
    d.users.push(u.clone());
    Json(u).into_response()
}

async fn requests(State(app): State<App>) -> Response {
    Json(app.data.lock().unwrap().requests.clone()).into_response()
}

async fn spec_v1(State(app): State<App>) -> Response {
    Json(spec(&app.base, false)).into_response()
}

async fn spec_v2(State(app): State<App>) -> Response {
    Json(spec(&app.base, true)).into_response()
}

fn list_of(item: &str) -> Value {
    json!({"type": "array", "items": {"$ref": format!("#/components/schemas/{item}")}})
}

fn ok(schema: Value) -> Value {
    json!({"200": {"description": "OK", "content": {"application/json": {"schema": schema}}}})
}

fn q(name: &str, schema: Value) -> Value {
    json!({"name": name, "in": "query", "required": false, "schema": schema})
}

/// The spec. v2 renames nothing but: raises the users and orders page sizes, declares
/// `nickname` and a new `phone` field, widens `age` to a number, changes the
/// type of two order fields, removes `/plans`, adds `/projects`, and adds an
/// API key scheme.
pub fn spec(base: &str, v2: bool) -> Value {
    let mut user_props = json!({
        "id": {"type": "integer"},
        "name": {"type": "string"},
        "email": {"type": "string"},
        "age": {"type": "integer"},
        "address": {"type": "object", "properties": {"city": {"type": "string"}}},
        "updated_at": {"type": "string", "format": "date-time"}
    });
    if v2 {
        user_props["nickname"] = json!({"type": "string"});
        user_props["phone"] = json!({"type": "string"});
        user_props["age"] = json!({"type": "number"});
    }
    let mut order_props = json!({
        "id": {"type": "integer"},
        "user_id": {"type": "integer"},
        "amount": {"type": "number"},
        "status": {"type": "string", "enum": ["paid", "refunded"]},
        "created_on": {"type": "string", "format": "date"},
        "updated_at": {"type": "string", "format": "date-time"}
    });
    if v2 {
        order_props["user_id"] = json!({"type": "string"});
        order_props["created_on"] = json!({"type": "string", "format": "date-time"});
    }
    let mut paths = json!({
        "/users": {"get": {
            "summary": "List users",
            "parameters": [
                q("page", json!({"type": "integer", "minimum": 1})),
                q("per_page", json!({"type": "integer", "maximum": if v2 { 20 } else { 10 }})),
                q("updated_since", json!({"type": "string", "format": "date-time"}))
            ],
            "responses": ok(json!({"type": "object", "properties": {
                "data": list_of("User"), "total": {"type": "integer"}
            }}))
        }},
        "/orders": {"get": {
            "summary": "List orders",
            "parameters": [
                q("offset", json!({"type": "integer"})),
                q("limit", json!({"type": "integer", "maximum": if v2 { 20 } else { 10 }})),
                q("sort", json!({"type": "string", "enum": ["updated_at", "-updated_at"]}))
            ],
            "responses": ok(list_of("Order"))
        }},
        "/events": {"get": {
            "summary": "List events",
            "parameters": [
                q("cursor", json!({"type": "string"})),
                q("limit", json!({"type": "integer", "maximum": 5})),
                q("sort", json!({"type": "string", "enum": ["-updated_at"]}))
            ],
            "responses": ok(json!({"type": "object", "properties": {
                "items": list_of("Event"), "next_cursor": {"type": "string", "nullable": true}
            }}))
        }},
        "/users/{id}": {"get": {
            "parameters": [{"name": "id", "in": "path", "required": true, "schema": {"type": "integer"}}],
            "responses": ok(json!({"$ref": "#/components/schemas/User"}))
        }}
    });
    if v2 {
        paths["/projects"] = json!({"get": {
            "summary": "List projects",
            "responses": ok(list_of("Project"))
        }});
    } else {
        paths["/plans"] = json!({"get": {
            "summary": "List plans",
            "responses": ok(json!({"type": "object", "properties": {
                "results": list_of("Plan"), "next": {"type": "string", "format": "uri", "nullable": true}
            }}))
        }});
    }
    let mut schemes = json!({
        "bearerAuth": {"type": "http", "scheme": "bearer"},
        "oauth": {"type": "oauth2", "flows": {"clientCredentials": {
            "tokenUrl": "/oauth/token", "scopes": {}
        }}}
    });
    let mut security = json!([{"bearerAuth": []}, {"oauth": []}]);
    if v2 {
        schemes["apiKey"] = json!({"type": "apiKey", "in": "header", "name": "X-API-Key"});
        security.as_array_mut().unwrap().push(json!({"apiKey": []}));
    }
    json!({
        "openapi": "3.0.3",
        "info": {"title": "Mockshop API", "version": if v2 { "2.0.0" } else { "1.0.0" }},
        "servers": [{"url": base}],
        "x-ratelimit": {"limit": 1000, "period": "1m"},
        "security": security,
        "paths": paths,
        "components": {
            "securitySchemes": schemes,
            "schemas": {
                "User": {"type": "object", "properties": user_props},
                "Order": {"type": "object", "properties": order_props},
                "Event": {"type": "object", "properties": {
                    "id": {"type": "integer"},
                    "kind": {"type": "string"},
                    "updated_at": {"type": "string", "format": "date-time"}
                }},
                "Plan": {"type": "object", "properties": {
                    "id": {"type": "integer"}, "name": {"type": "string"}, "price": {"type": "integer"}
                }},
                "Project": {"type": "object", "properties": {
                    "id": {"type": "integer"}, "title": {"type": "string"}
                }}
            }
        }
    })
}

/// Just enough base64 decoding for the token endpoint's Basic header.
mod base64_lite {
    pub fn decode(s: &str) -> Option<String> {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut bits = 0u32;
        let mut n = 0;
        let mut out = Vec::new();
        for c in s.trim_end_matches('=').bytes() {
            bits = (bits << 6) | A.iter().position(|&a| a == c)? as u32;
            n += 6;
            if n >= 8 {
                n -= 8;
                out.push((bits >> n) as u8);
                bits &= (1 << n) - 1;
            }
        }
        String::from_utf8(out).ok()
    }
}
