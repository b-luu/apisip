//! A far-future cursor must not become the watermark: later records would
//! never be fetched again.

use apisip::cmd::run::{RunOptions, run};
use apisip::state::{Manifest, StreamReport};
use apisip::workspace::Workspace;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FUTURE: &str = "2099-01-01T00:00:00Z";

fn spec() -> Value {
    json!({
        "openapi": "3.0.3",
        "info": {"title": "Cap Test", "version": "1"},
        "paths": {"/users": {"get": {
            "parameters": [
                {"name": "updated_since", "in": "query", "schema": {"type": "string"}},
                {"name": "sort", "in": "query", "schema": {"type": "string"}}
            ],
            "responses": {"200": {"description": "ok", "content": {"application/json": {"schema": {
                "type": "array",
                "items": {"type": "object", "properties": {
                    "id": {"type": "integer"},
                    "updated_at": {"type": "string", "format": "date-time"}
                }}
            }}}}}
        }}}
    })
}

fn users(ids: &[(i64, &str)]) -> Value {
    Value::Array(
        ids.iter()
            .map(|(id, t)| json!({"id": id, "updated_at": t}))
            .collect(),
    )
}

fn setup(dir: &std::path::Path, server: &MockServer, incremental: &str, extra: &str) -> Workspace {
    std::fs::write(dir.join("spec.json"), spec().to_string()).unwrap();
    std::fs::write(
        dir.join("apisip.toml"),
        format!(
            r#"
[source]
spec = "spec.json"
base_url = "{}"
retry = {{ max_tries = 1 }}
{extra}

[destination]
url = "file://out"

[[streams]]
name = "users"
path = "/users"
primary_key = "id"
incremental = {incremental}
"#,
            server.uri()
        ),
    )
    .unwrap();
    Workspace::new(Some(dir.to_path_buf())).unwrap()
}

fn watermark(dir: &std::path::Path) -> Option<String> {
    let st: apisip::state::State =
        serde_json::from_slice(&std::fs::read(dir.join("out/_apisip/state.json")).unwrap())
            .unwrap();
    st.streams["users"].watermark.clone()
}

fn last_report(dir: &std::path::Path) -> StreamReport {
    let st: apisip::state::State =
        serde_json::from_slice(&std::fs::read(dir.join("out/_apisip/state.json")).unwrap())
            .unwrap();
    let id = st.streams["users"].last_run_id.clone().unwrap();
    let m: Manifest =
        serde_json::from_slice(&std::fs::read(dir.join("out").join(Manifest::path(&id))).unwrap())
            .unwrap();
    m.streams["users"].clone()
}

#[tokio::test]
async fn filter_mode_keeps_fetching_after_a_far_future_cursor() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let ws = setup(
        dir,
        &server,
        r#"{ cursor = "updated_at", param = "updated_since" }"#,
        "",
    );

    Mock::given(method("GET"))
        .and(path("/users"))
        .and(query_param_is_missing("updated_since"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(users(&[(1, "2026-01-01T00:00:00Z"), (2, FUTURE)])),
        )
        .mount(&server)
        .await;
    assert!(run(&ws, RunOptions::default()).await.unwrap());
    assert_eq!(watermark(dir).as_deref(), Some("2026-01-01T00:00:00Z"));
    let r = last_report(dir);
    assert_eq!(r.records, 2, "the far-future record is still written");
    assert!(
        r.warnings.iter().any(|w| w
            .starts_with("1 record(s) have a `updated_at` value later than")
            && w.contains("Z (run start + 1day): they were written")),
        "{:?}",
        r.warnings
    );

    // The next run asks from the real watermark and gets the newer record.
    Mock::given(method("GET"))
        .and(path("/users"))
        .and(query_param("updated_since", "2026-01-01T00:00:00Z"))
        .respond_with(ResponseTemplate::new(200).set_body_json(users(&[
            (1, "2026-01-01T00:00:00Z"),
            (2, FUTURE),
            (3, "2026-01-02T00:00:00Z"),
        ])))
        .mount(&server)
        .await;
    assert!(run(&ws, RunOptions::default()).await.unwrap());
    assert_eq!(watermark(dir).as_deref(), Some("2026-01-02T00:00:00Z"));
    let r = last_report(dir);
    assert_eq!((r.records, r.unchanged), (2, 1));
}

#[tokio::test]
async fn early_stop_keeps_fetching_after_a_far_future_cursor() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let ws = setup(
        dir,
        &server,
        r#"{ cursor = "updated_at", sort = { param = "sort", value = "-updated_at" } }"#,
        "",
    );

    Mock::given(method("GET"))
        .and(path("/users"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(users(&[(2, FUTURE), (1, "2026-01-01T00:00:00Z")])),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    assert!(run(&ws, RunOptions::default()).await.unwrap());
    assert_eq!(watermark(dir).as_deref(), Some("2026-01-01T00:00:00Z"));

    Mock::given(method("GET"))
        .and(path("/users"))
        .respond_with(ResponseTemplate::new(200).set_body_json(users(&[
            (2, FUTURE),
            (3, "2026-01-02T00:00:00Z"),
            (1, "2026-01-01T00:00:00Z"),
        ])))
        .mount(&server)
        .await;
    assert!(run(&ws, RunOptions::default()).await.unwrap());
    assert_eq!(watermark(dir).as_deref(), Some("2026-01-02T00:00:00Z"));
    assert_eq!(last_report(dir).records, 2, "records 2 and 3");
}

#[tokio::test]
async fn full_refresh_lowers_a_far_future_watermark() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let incremental = r#"{ cursor = "updated_at", param = "updated_since" }"#;
    Mock::given(method("GET"))
        .and(path("/users"))
        .and(query_param_is_missing("updated_since"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(users(&[(1, "2026-01-01T00:00:00Z"), (2, FUTURE)])),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/users"))
        .and(query_param("updated_since", FUTURE))
        .respond_with(ResponseTemplate::new(200).set_body_json(users(&[(2, FUTURE)])))
        .mount(&server)
        .await;

    // A skew this large lets the far-future value in, as before the cap existed.
    let ws = setup(dir, &server, incremental, r#"max_cursor_skew = "200y""#);
    assert!(run(&ws, RunOptions::default()).await.unwrap());
    assert_eq!(watermark(dir).as_deref(), Some(FUTURE));

    // With the default skew, an incremental run warns but does not lower it.
    let ws = setup(dir, &server, incremental, "");
    assert!(run(&ws, RunOptions::default()).await.unwrap());
    assert_eq!(watermark(dir).as_deref(), Some(FUTURE));
    let r = last_report(dir);
    assert!(
        r.warnings.iter().any(|w| w
            .starts_with(&format!("the stored watermark ({FUTURE}) is later than"))
            && w.contains("apisip run --full-refresh --stream users")),
        "{:?}",
        r.warnings
    );

    // A full refresh saw every record: it resets the watermark.
    let full = RunOptions {
        full_refresh: true,
        ..Default::default()
    };
    assert!(run(&ws, full).await.unwrap());
    assert_eq!(watermark(dir).as_deref(), Some("2026-01-01T00:00:00Z"));
}
