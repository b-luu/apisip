use std::time::Duration;

use futures::TryStreamExt;
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::config::{Pagination, RetryConfig};

fn source(server: &MockServer) -> Source {
    Source {
        spec: "spec.json".into(),
        base_url: format!("{}/v1", server.uri()),
        auth: None,
        rate_limit: None,
        retry: RetryConfig {
            max_tries: 3,
            ..RetryConfig::default()
        },
        concurrency: 4,
        headers: [("X-Client".to_string(), "apisip-test".to_string())].into(),
    }
}

fn client(server: &MockServer) -> Arc<ApiClient> {
    ApiClient::new(&source(server))
        .unwrap()
        .with_retry_base(Duration::from_millis(1))
}

#[tokio::test]
async fn retries_429_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/users"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/users"))
        .and(wiremock::matchers::header("x-client", "apisip-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id": 1}])))
        .mount(&server)
        .await;
    let c = client(&server);
    let page = c.get(&c.endpoint("/users").unwrap()).await.unwrap();
    assert_eq!(page.body, json!([{"id": 1}]));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn gives_up_after_max_tries() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .expect(3)
        .mount(&server)
        .await;
    let c = client(&server);
    let err = c.get(&c.endpoint("users").unwrap()).await.unwrap_err();
    assert!(format!("{err:#}").contains("HTTP 503"), "{err:#}");
}

#[tokio::test]
async fn client_errors_fail_fast_with_a_hint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server);
    let err = format!(
        "{:#}",
        c.get(&c.endpoint("users").unwrap()).await.unwrap_err()
    );
    assert!(
        err.contains("HTTP 403") && err.contains("credentials in .env"),
        "{err}"
    );
}

#[tokio::test]
async fn follows_pages_until_short_page() {
    let server = MockServer::start().await;
    for (page, ids) in [("1", vec![1, 2]), ("2", vec![3, 4]), ("3", vec![5])] {
        Mock::given(method("GET"))
            .and(path("/v1/users"))
            .and(query_param("page", page))
            .and(query_param("per_page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": ids.iter().map(|i| json!({"id": i})).collect::<Vec<_>>()
            })))
            .expect(1)
            .mount(&server)
            .await;
    }
    let c = client(&server);
    let first = c.endpoint("/users").unwrap();
    let pagination = Some(Pagination::PageNumber {
        param: "page".into(),
        size_param: Some("per_page".into()),
        size: Some(2),
        start: 1,
    });
    let all: Vec<Vec<Value>> = pages(c, first, pagination, "/data".into())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(all.iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 2, 1]);
}

#[tokio::test]
async fn non_json_body_is_a_clear_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>login</html>"))
        .mount(&server)
        .await;
    let c = client(&server);
    let err = format!(
        "{:#}",
        c.get(&c.endpoint("users").unwrap()).await.unwrap_err()
    );
    assert!(err.contains("not JSON") && err.contains("<html>"), "{err}");
}
