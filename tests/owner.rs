//! A run whose destination is taken by another folder while it fetches
//! does not commit. On a store without conditional writes, two folders that
//! start at once on a fresh destination can both pass the claim; the last
//! owner written wins. The test rewrites the owner mid-run to stand in for
//! that lost race.

use std::time::Duration;

use apisip::cmd::run::{RunOptions, run};
use apisip::state::State;
use apisip::workspace::Workspace;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn a_run_does_not_commit_when_another_folder_took_the_destination() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"id": 1}]))
                .set_delay(Duration::from_millis(500)),
        )
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let spec = json!({
        "openapi": "3.0.3",
        "info": {"title": "Owner Test", "version": "1"},
        "paths": {"/users": {"get": {"responses": {"200": {"description": "ok"}}}}}
    });
    std::fs::write(dir.join("spec.json"), spec.to_string()).unwrap();
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
"#,
            server.uri()
        ),
    )
    .unwrap();
    let ws = Workspace::new(Some(dir.clone())).unwrap();
    let running = tokio::spawn(async move { run(&ws, RunOptions::default()).await });

    // Once this run has claimed the destination, another folder overwrites the owner.
    let state_file = dir.join("out/_apisip/state.json");
    let mut state = loop {
        if let Ok(b) = std::fs::read(&state_file)
            && let Ok(s) = serde_json::from_slice::<State>(&b)
            && s.owner.is_some()
        {
            break s;
        }
        assert!(!running.is_finished(), "the run ended before claiming");
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    state.owner = Some("another-folder".into());
    std::fs::write(&state_file, serde_json::to_vec(&state).unwrap()).unwrap();

    assert!(!running.await.unwrap().unwrap(), "the stream must fail");
    let data: Vec<_> = std::fs::read_dir(dir.join("out/users"))
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(data.is_empty(), "nothing committed: {data:?}");
    let state: State = serde_json::from_slice(&std::fs::read(&state_file).unwrap()).unwrap();
    assert_eq!(state.owner.as_deref(), Some("another-folder"));
    assert!(state.streams.is_empty(), "{state:?}");
}
