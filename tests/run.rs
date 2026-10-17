//! `run` against a wiremock API: staged commit, watermarks, crash safety
//! and overlap flagging. One test, because the failure injection uses a
//! process-wide variable. Fail points exist in debug builds only.
#![cfg(debug_assertions)]

use apisip::cmd::run::{RunOptions, run};
use apisip::state::{Manifest, State};
use apisip::workspace::Workspace;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn spec() -> Value {
    json!({
        "openapi": "3.0.3",
        "info": {"title": "Run Test", "version": "1"},
        "paths": {"/users": {"get": {
            "parameters": [{"name": "updated_since", "in": "query", "schema": {"type": "string"}}],
            "responses": {"200": {"description": "ok", "content": {"application/json": {"schema": {
                "type": "array",
                "items": {"type": "object", "properties": {
                    "id": {"type": "integer"},
                    "updated_at": {"type": "string", "format": "date-time"}
                }}
            }}}}}
        }},
        "/plans": {"get": {
            "responses": {"200": {"description": "ok", "content": {"application/json": {"schema": {
                "type": "array",
                "items": {"type": "object", "properties": {
                    "id": {"type": "integer"}, "name": {"type": "string"}
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

fn data_files(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in walk(&dir.join("out/users")) {
        if e.ends_with(".jsonl") || e.ends_with(".parquet") {
            out.push(e);
        }
    }
    out.sort();
    out
}

fn walk(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p.display().to_string());
            }
        }
    }
    out
}

fn lines(files: &[String]) -> usize {
    files
        .iter()
        .map(|f| std::fs::read_to_string(f).unwrap().lines().count())
        .sum()
}

async fn state(dir: &std::path::Path) -> State {
    serde_json::from_slice(&std::fs::read(dir.join("out/_apisip/state.json")).unwrap()).unwrap()
}

#[tokio::test]
async fn staged_commit_watermarks_and_crash_safety() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("spec.json"), spec().to_string()).unwrap();
    std::fs::write(
        dir.join("apisip.toml"),
        format!(
            r#"
[source]
spec = "spec.json"
base_url = "{}"
retry = {{ max_tries = 1 }}

[destination]
url = "file://out"

[[streams]]
name = "users"
path = "/users"
primary_key = "id"
incremental = {{ cursor = "updated_at", param = "updated_since" }}

[[streams]]
name = "plans"
path = "/plans"
primary_key = "id"
"#,
            server.uri()
        ),
    )
    .unwrap();
    let ws = Workspace::new(Some(dir.to_path_buf())).unwrap();

    // A full-refresh stream: plan 1 changes and plan 2 disappears after two calls.
    Mock::given(method("GET"))
        .and(path("/plans"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "name": "a"}, {"id": 2, "name": "b"}
        ])))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/plans"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "name": "a2"}, {"id": 3, "name": "x"}
        ])))
        .mount(&server)
        .await;

    // First run: no watermark, so no filter.
    Mock::given(method("GET"))
        .and(path("/users"))
        .and(query_param_is_missing("updated_since"))
        .respond_with(ResponseTemplate::new(200).set_body_json(users(&[
            (1, "2026-01-01T00:00:00Z"),
            (2, "2026-01-02T00:00:00Z"),
        ])))
        .mount(&server)
        .await;
    assert!(run(&ws, RunOptions::default()).await.unwrap());
    let files = data_files(dir);
    assert_eq!(lines(&files), 2);
    let st = state(dir).await;
    assert_eq!(
        st.streams["users"].watermark.as_deref(),
        Some("2026-01-02T00:00:00Z")
    );
    let first: Value = serde_json::from_str(
        std::fs::read_to_string(&files[0])
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert!(first["_apisip_run_id"].is_string() && first["_apisip_extracted_at"].is_string());

    // Second run sends the watermark; the boundary record comes back unchanged.
    Mock::given(method("GET"))
        .and(path("/users"))
        .and(query_param("updated_since", "2026-01-02T00:00:00Z"))
        .respond_with(ResponseTemplate::new(200).set_body_json(users(&[
            (2, "2026-01-02T00:00:00Z"),
            (3, "2026-01-03T00:00:00Z"),
        ])))
        .mount(&server)
        .await;

    // A crash after staging leaves the data path and the watermark unchanged.
    unsafe { std::env::set_var("APISIP_FAIL_POINT", "after_staging") };
    assert!(!run(&ws, RunOptions::default()).await.unwrap());
    unsafe { std::env::remove_var("APISIP_FAIL_POINT") };
    assert_eq!(data_files(dir), files);
    assert_eq!(lines(&data_files(dir)), 2);
    assert_eq!(
        state(dir).await.streams["users"].watermark.as_deref(),
        Some("2026-01-02T00:00:00Z")
    );
    assert!(
        walk(&dir.join("out/_apisip/staging")).is_empty(),
        "staging is cleaned up"
    );

    // The retry commits, merging into the existing local part, and skips the
    // boundary record it already wrote.
    assert!(run(&ws, RunOptions::default()).await.unwrap());
    assert_eq!(data_files(dir).len(), 1);
    assert_eq!(lines(&data_files(dir)), 3);
    let st = state(dir).await;
    assert_eq!(
        st.streams["users"].watermark.as_deref(),
        Some("2026-01-03T00:00:00Z")
    );
    let run_id = st.streams["users"].last_run_id.clone().unwrap();
    let m: Manifest = serde_json::from_slice(
        &std::fs::read(dir.join("out").join(Manifest::path(&run_id))).unwrap(),
    )
    .unwrap();
    let r = &m.streams["users"];
    assert_eq!(r.requested_from.as_deref(), Some("2026-01-02T00:00:00Z"));
    assert_eq!(r.unchanged, 1);
    assert!(r.overlap.is_none(), "an equal boundary is not an overlap");

    // A full refresh re-reads old records: the manifest flags the overlap.
    assert!(
        run(
            &ws,
            RunOptions {
                full_refresh: true,
                ..Default::default()
            }
        )
        .await
        .unwrap()
    );
    let st = state(dir).await;
    let run_id = st.streams["users"].last_run_id.clone().unwrap();
    let m: Manifest = serde_json::from_slice(
        &std::fs::read(dir.join("out").join(Manifest::path(&run_id))).unwrap(),
    )
    .unwrap();
    assert!(m.streams["users"].overlap.is_some());
    assert_eq!(
        st.streams["users"].watermark.as_deref(),
        Some("2026-01-03T00:00:00Z"),
        "the watermark never moves back"
    );

    // Five rows on disk (1, 2 | 2, 3 | 1, 2): three keys, three versions.
    let views = dir.join("out/_apisip/views");
    assert!(views.join("users_current.sql").exists());
    match duckdb(
        &views,
        "select count(*), count(distinct id), max(updated_at)::varchar from users_current",
    ) {
        Some(out) => assert_eq!(out, "3,3,2026-01-03 00:00:00"),
        None => eprintln!("duckdb not found: skipping the view queries"),
    }
    if let Some(out) = duckdb(
        &views,
        "select count(*), count(*) filter (where is_current), count(*) filter (where valid_to is null) from users_history",
    ) {
        assert_eq!(out, "3,3,3");
    }
    // Committed plan snapshots: run 1, the retry and the full refresh.
    if let Some(out) = duckdb(
        &views,
        "select string_agg(id || name, ' ' order by id) from plans_current",
    ) {
        assert_eq!(out, "1a2 3x");
    }
    if let Some(out) = duckdb(
        &views,
        "select string_agg(id || name || ':' || is_current || ':' || (valid_to is null), ' ' order by id, valid_from) from plans_history",
    ) {
        assert_eq!(
            out,
            "1a:false:false 1a2:true:true 2b:false:true 3x:true:true"
        );
    }
}

/// Runs a query through the DuckDB CLI after loading the generated views.
/// Returns `None` when DuckDB is not installed.
pub fn duckdb(views: &std::path::Path, query: &str) -> Option<String> {
    let out = std::process::Command::new("duckdb")
        .arg("-csv")
        .arg("-noheader")
        .arg("-c")
        .arg(format!(".read {}", views.join("all.sql").display()))
        .arg("-c")
        .arg(query)
        .output()
        .ok()?;
    assert!(
        out.status.success(),
        "duckdb failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8(out.stdout).unwrap().trim().to_string())
}
