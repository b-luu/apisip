use std::time::Duration;

use futures::TryStreamExt;
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::config::{AuthConfig, Pagination, RetryConfig};

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
async fn a_retry_after_past_max_wait_fails_at_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "21600"))
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server);
    let url = c.endpoint("/users").unwrap();
    let get = c.get(&url);
    let err = tokio::time::timeout(Duration::from_secs(5), get)
        .await
        .expect("fails at once, without waiting")
        .unwrap_err()
        .to_string();
    assert!(err.contains("HTTP 429"), "{err}");
    assert!(err.contains("wait 6h"), "{err}");
    assert!(err.contains("source.retry.max_wait (10m)"), "{err}");
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
    let url = c.endpoint("users").unwrap();
    let err = tokio::time::timeout(Duration::from_secs(5), c.get(&url))
        .await
        .expect("stops after max_tries, without waiting for max_wait")
        .unwrap_err();
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

#[tokio::test]
async fn response_snippets_drop_control_characters() {
    let hostile = "\x1b]0;owned\x07\x1b[2Jfake\r\n$ \u{9b}31m";
    for status in [200, 400] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status).set_body_string(hostile))
            .mount(&server)
            .await;
        let c = client(&server);
        let err = format!(
            "{:#}",
            c.get(&c.endpoint("users").unwrap()).await.unwrap_err()
        );
        assert!(!err.contains(char::is_control), "{err:?}");
        assert!(err.contains("[2Jfake"), "{err:?}");
    }
}

#[tokio::test]
async fn response_snippets_stop_after_300_characters() {
    for (len, cut) in [(300, false), (301, true)] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(400).set_body_string("é".repeat(len)))
            .mount(&server)
            .await;
        let c = client(&server);
        let err = format!(
            "{:#}",
            c.get(&c.endpoint("users").unwrap()).await.unwrap_err()
        );
        assert_eq!(err.matches('é').count(), 300, "{err}");
        assert_eq!(err.ends_with('…'), cut, "{err}");
    }
}

#[test]
fn masked_url_hides_keys_by_name_and_by_value() {
    let url =
        Url::parse("https://x.test/items?page=2&api_key=k123&q=abc-s3cr3t-def&access_token=t")
            .unwrap();
    let shown = super::masked_url(&url, &["s3cr3t".to_string()]);
    assert_eq!(
        shown,
        "https://x.test/items?page=2&api_key=****&q=****&access_token=****"
    );
}

#[tokio::test]
async fn only_values_from_the_environment_are_masked() {
    let server = MockServer::start().await;
    let mut src = source(&server);
    // SAFETY: test-only variable with a unique name, read by this test only.
    unsafe { std::env::set_var("APISIP_TEST_MASK_HEADER", "hdr-s3cr3t") };
    src.headers = [
        ("X-Api-Version".to_string(), "1".to_string()),
        (
            "X-Secret".to_string(),
            "${APISIP_TEST_MASK_HEADER}".to_string(),
        ),
    ]
    .into();
    let c = ApiClient::new(&src).unwrap();
    let url = Url::parse("http://127.0.0.1:8100/v1/plans?x=1&y=hdr-s3cr3t").unwrap();
    assert_eq!(c.shown(&url), "http://127.0.0.1:8100/v1/plans?x=1&y=****");
    // A short secret is hidden as a whole query value, never inside text.
    let short = super::masked_url(&url, &["1".to_string()]);
    assert_eq!(short, "http://127.0.0.1:8100/v1/plans?x=****&y=hdr-s3cr3t");
}

/// A 400 whose body echoes the Authorization header back.
fn echo_authorization(req: &wiremock::Request) -> ResponseTemplate {
    let auth = req
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    ResponseTemplate::new(400).set_body_string(format!("bad request, you sent {auth}"))
}

#[tokio::test]
async fn echoed_basic_and_oauth_credentials_are_masked() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"access_token": "at-9f8e7d6c5b"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(echo_authorization)
        .mount(&server)
        .await;
    let basic = AuthConfig::Basic {
        scheme: None,
        username: "reporting-user".into(),
        password: "pw-4b3c2d1e".into(),
    };
    let oauth = AuthConfig::Oauth2ClientCredentials {
        scheme: None,
        token_url: format!("{}/oauth/token", server.uri()),
        client_id: "client-1".into(),
        client_secret: "cs-1a2b3c4d".into(),
        scopes: vec![],
    };
    for (auth, leaked) in [(basic, "cmVwb3J0aW5n"), (oauth, "at-9f8e7d6c5b")] {
        let mut src = source(&server);
        src.auth = Some(auth);
        let c = ApiClient::new(&src).unwrap();
        let err = c.get(&c.endpoint("/users").unwrap()).await.unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("you sent"), "{err}");
        assert!(!err.contains(leaked), "a credential leaked: {err}");
    }
}

#[tokio::test]
async fn credentials_never_go_to_another_host() {
    let server = MockServer::start().await;
    let elsewhere = server.uri().replace("127.0.0.1", "localhost");
    Mock::given(method("GET"))
        .and(path("/v1/paged"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"id": 1}]))
                .insert_header(
                    "link",
                    format!("<{elsewhere}/v1/paged?page=2>; rel=\"next\""),
                ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/moved"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", format!("{elsewhere}/v1/paged")),
        )
        .mount(&server)
        .await;
    let c = client(&server);

    let first = c.endpoint("/paged").unwrap();
    let pages: Result<Vec<_>> = pages(c.clone(), first, None, String::new())
        .try_collect()
        .await;
    let err = format!("{:#}", pages.unwrap_err());
    assert!(err.contains("is on http://localhost:"), "{err}");

    let err = c.get(&c.endpoint("/moved").unwrap()).await.unwrap_err();
    let err = format!("{err:#}");
    assert!(
        err.contains("refused a redirect to http://localhost:"),
        "{err}"
    );
    // Neither the next page nor the redirect target was requested.
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[test]
fn env_values_in_stream_params_are_masked() {
    // SAFETY: test-only variable with a unique name, read by this test only.
    unsafe { std::env::set_var("APISIP_TEST_MASK_PARAM", "prm-7a6b5c4d") };
    let config = crate::config::Config::parse(
        r#"
[source]
spec = "spec.json"
base_url = "http://127.0.0.1:8100"

[destination]

[[streams]]
name = "plans"
path = "/plans"
params = { tok = "${APISIP_TEST_MASK_PARAM}" }
"#,
    )
    .unwrap();
    let c = ApiClient::for_config(&config).unwrap();
    let url = Url::parse("http://127.0.0.1:8100/plans?tok=prm-7a6b5c4d").unwrap();
    assert_eq!(c.shown(&url), "http://127.0.0.1:8100/plans?tok=****");
}

#[tokio::test]
async fn a_redirect_on_the_same_host_is_followed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/old"))
        .respond_with(ResponseTemplate::new(301).insert_header("location", "/v1/new"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/new"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id": 1}])))
        .mount(&server)
        .await;
    let c = client(&server);
    let page = c.get(&c.endpoint("/old").unwrap()).await.unwrap();
    assert_eq!(page.body, json!([{"id": 1}]));
}

#[test]
fn credentials_keep_to_the_scheme_and_port_or_upgrade() {
    let ok =
        |a: &str, b: &str| may_send_credentials(&Url::parse(a).unwrap(), &Url::parse(b).unwrap());
    assert!(ok("https://api.x/a", "https://api.x:443/b"));
    assert!(ok("http://api.x/a", "https://api.x/b"));
    assert!(
        !ok("https://api.x/a", "http://api.x/b"),
        "a downgrade to http"
    );
    assert!(
        !ok("https://api.x/a", "https://api.x:8443/b"),
        "another port"
    );
    assert!(!ok("https://api.x/a", "https://evil.y/b"), "another host");
    assert!(
        !ok("https://api.x/a", "https://api.x@evil.y/b"),
        "userinfo trick"
    );
}

#[tokio::test]
async fn a_budget_reset_on_a_retried_response_counts_toward_max_wait() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", "3600"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let c = client(&server);
    let url = c.endpoint("/users").unwrap();
    let err = tokio::time::timeout(Duration::from_secs(5), c.get(&url))
        .await
        .expect("fails at once, without waiting")
        .unwrap_err()
        .to_string();
    assert!(err.contains("wait 1h"), "{err}");
    assert!(err.contains("source.retry.max_wait (10m)"), "{err}");
}

#[tokio::test]
async fn the_waits_of_several_retries_add_up_to_max_wait() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let mut src = source(&server);
    src.retry.max_tries = 8;
    // Every delay is capped at 1s, so each one fits alone; only the total does not.
    src.retry.max_backoff = Duration::from_secs(1);
    src.retry.max_wait = Duration::from_millis(2500);
    let c = ApiClient::new(&src).unwrap();
    let err = c
        .get(&c.endpoint("/users").unwrap())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("HTTP 503"), "{err}");
    assert!(err.contains("source.retry.max_wait (3s)"), "{err}");
    // Retries after 1s and 2s fit; the third would reach 3s.
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn a_spent_budget_on_a_success_is_pacing_not_a_retry() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", "1")
                .set_body_json(json!([{"id": 1}])),
        )
        .mount(&server)
        .await;
    let mut src = source(&server);
    src.retry.max_wait = Duration::ZERO;
    let c = ApiClient::new(&src).unwrap();
    let page = c.get(&c.endpoint("/users").unwrap()).await.unwrap();
    assert_eq!(page.body, json!([{"id": 1}]));
}
