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
    assert!(err.contains("on another host (localhost)"), "{err}");

    let err = c.get(&c.endpoint("/moved").unwrap()).await.unwrap_err();
    let err = format!("{err:#}");
    assert!(err.contains("refused a redirect to another host"), "{err}");
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
