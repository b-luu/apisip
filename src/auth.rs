//! Request authentication: bearer, API key, HTTP basic, and OAuth2 client
//! credentials with a cached token refreshed on expiry or on a 401.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::RequestBuilder;
use tokio::sync::Mutex;

use crate::config::{ApiKeyLocation, AuthConfig, interpolate};

pub struct Authenticator {
    kind: Kind,
}

enum Kind {
    None,
    Bearer(String),
    ApiKey {
        name: String,
        location: ApiKeyLocation,
        value: String,
    },
    Basic {
        username: String,
        password: String,
    },
    OAuth2 {
        http: reqwest::Client,
        token_url: String,
        client_id: String,
        client_secret: String,
        scopes: Vec<String>,
        token: Mutex<Option<Token>>,
        /// Every access token fetched, masked in messages like the secret.
        issued: std::sync::Mutex<Vec<String>>,
    },
}

struct Token {
    access: String,
    expires_at: Instant,
}

impl std::fmt::Debug for Authenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print credentials.
        f.write_str(match self.kind {
            Kind::None => "Authenticator(none)",
            Kind::Bearer(_) => "Authenticator(bearer)",
            Kind::ApiKey { .. } => "Authenticator(api_key)",
            Kind::Basic { .. } => "Authenticator(basic)",
            Kind::OAuth2 { .. } => "Authenticator(oauth2_client_credentials)",
        })
    }
}

impl Authenticator {
    /// Resolves `${VAR}` references. Fails with the variable names when some are unset.
    pub fn new(
        config: Option<&AuthConfig>,
        http: reqwest::Client,
        base_url: &str,
    ) -> Result<Authenticator> {
        let kind = match config {
            None => Kind::None,
            Some(AuthConfig::Bearer { token, .. }) => {
                Kind::Bearer(interpolate(token).context("auth token")?)
            }
            Some(AuthConfig::ApiKey {
                name,
                location,
                value,
                ..
            }) => Kind::ApiKey {
                name: name.clone(),
                location: *location,
                value: interpolate(value).context("API key")?,
            },
            Some(AuthConfig::Basic {
                username, password, ..
            }) => Kind::Basic {
                username: interpolate(username).context("basic auth username")?,
                password: interpolate(password).context("basic auth password")?,
            },
            Some(AuthConfig::Oauth2ClientCredentials {
                token_url,
                client_id,
                client_secret,
                scopes,
                ..
            }) => {
                let token_url = interpolate(token_url)?;
                let token_url = if token_url.starts_with("http") {
                    token_url
                } else {
                    url::Url::parse(&format!("{}/", base_url.trim_end_matches('/')))
                        .and_then(|b| b.join(token_url.trim_start_matches('/')))
                        .map(|u| u.to_string())
                        .context("OAuth2 token_url")?
                };
                Kind::OAuth2 {
                    http,
                    token_url,
                    client_id: interpolate(client_id).context("OAuth2 client id")?,
                    client_secret: interpolate(client_secret).context("OAuth2 client secret")?,
                    scopes: scopes.clone(),
                    token: Mutex::new(None),
                    issued: Default::default(),
                }
            }
        };
        Ok(Authenticator { kind })
    }

    pub fn none() -> Authenticator {
        Authenticator { kind: Kind::None }
    }

    /// The resolved secret values, so callers can mask them in messages.
    /// They include the encoded Basic credential and the OAuth2 access
    /// tokens fetched so far: a server can echo either back in an error.
    pub fn secrets(&self) -> Vec<String> {
        let v = match &self.kind {
            Kind::None => vec![],
            Kind::Bearer(t) => vec![t.clone()],
            Kind::ApiKey { value, .. } => vec![value.clone()],
            Kind::Basic { username, password } => {
                use base64::Engine;
                let encoded = base64::engine::general_purpose::STANDARD
                    .encode(format!("{username}:{password}"));
                vec![password.clone(), encoded]
            }
            Kind::OAuth2 {
                client_secret,
                issued,
                ..
            } => {
                let mut v = vec![client_secret.clone()];
                v.extend(
                    issued
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .iter()
                        .cloned(),
                );
                v
            }
        };
        v.into_iter().filter(|s| !s.is_empty()).collect()
    }

    pub async fn apply(&self, rb: RequestBuilder) -> Result<RequestBuilder> {
        Ok(match &self.kind {
            Kind::None => rb,
            Kind::Bearer(t) => rb.bearer_auth(t),
            Kind::ApiKey {
                name,
                location,
                value,
            } => match location {
                ApiKeyLocation::Header => rb.header(name.as_str(), value.as_str()),
                ApiKeyLocation::Query => rb.query(&[(name, value)]),
                ApiKeyLocation::Cookie => {
                    rb.header(reqwest::header::COOKIE, format!("{name}={value}"))
                }
            },
            Kind::Basic { username, password } => rb.basic_auth(username, Some(password)),
            Kind::OAuth2 { .. } => rb.bearer_auth(self.oauth_token().await?),
        })
    }

    /// Called after a 401. Returns true when retrying with fresh credentials may help.
    pub async fn invalidate(&self) -> bool {
        match &self.kind {
            Kind::OAuth2 { token, .. } => {
                *token.lock().await = None;
                true
            }
            _ => false,
        }
    }

    async fn oauth_token(&self) -> Result<String> {
        let Kind::OAuth2 {
            http,
            token_url,
            client_id,
            client_secret,
            scopes,
            token,
            issued,
        } = &self.kind
        else {
            unreachable!()
        };
        let mut guard = token.lock().await;
        if let Some(t) = guard.as_ref()
            && t.expires_at > Instant::now()
        {
            return Ok(t.access.clone());
        }
        let mut form = vec![("grant_type", "client_credentials".to_string())];
        if !scopes.is_empty() {
            form.push(("scope", scopes.join(" ")));
        }
        // client_secret_basic first (the RFC 6749 default), then client_secret_post.
        let mut resp = http
            .post(token_url)
            .basic_auth(client_id, Some(client_secret))
            .form(&form)
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .context("requesting OAuth2 token")?;
        if matches!(resp.status().as_u16(), 400 | 401) {
            let mut body = form.clone();
            body.push(("client_id", client_id.clone()));
            body.push(("client_secret", client_secret.clone()));
            resp = http
                .post(token_url)
                .form(&body)
                .send()
                .await
                .map_err(reqwest::Error::without_url)
                .context("requesting OAuth2 token")?;
        }
        let status = resp.status();
        if !status.is_success() {
            bail!("OAuth2 token request to {token_url} failed: HTTP {status}");
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(reqwest::Error::without_url)
            .context("OAuth2 token response is not JSON")?;
        let access = body
            .get("access_token")
            .and_then(|v| v.as_str())
            .context("OAuth2 token response has no access_token")?
            .to_string();
        issued
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(access.clone());
        let ttl = body
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600);
        // Refresh a little early so a token never expires mid-request.
        let margin = (ttl / 10).min(30);
        *guard = Some(Token {
            access: access.clone(),
            expires_at: Instant::now() + Duration::from_secs(ttl.saturating_sub(margin)),
        });
        Ok(access)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn oauth(server: &MockServer) -> Authenticator {
        // SAFETY: test-only variables with names unique to this module.
        unsafe {
            std::env::set_var("APISIP_AUTH_TEST_ID", "client-1");
            std::env::set_var("APISIP_AUTH_TEST_SECRET", "s3cret");
        }
        Authenticator::new(
            Some(&AuthConfig::Oauth2ClientCredentials {
                scheme: None,
                token_url: "/oauth/token".into(),
                client_id: "${APISIP_AUTH_TEST_ID}".into(),
                client_secret: "${APISIP_AUTH_TEST_SECRET}".into(),
                scopes: vec!["read".into()],
            }),
            reqwest::Client::new(),
            &server.uri(),
        )
        .unwrap()
    }

    async fn bearer_of(a: &Authenticator) -> String {
        let req = a
            .apply(reqwest::Client::new().get("http://x.test/"))
            .await
            .unwrap()
            .build()
            .unwrap();
        req.headers()["authorization"].to_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn oauth_token_is_fetched_once_and_cached() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(header("authorization", "Basic Y2xpZW50LTE6czNjcmV0"))
            .and(body_string_contains("grant_type=client_credentials"))
            .and(body_string_contains("scope=read"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token": "t1", "expires_in": 3600})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let a = oauth(&server);
        assert_eq!(bearer_of(&a).await, "Bearer t1");
        assert_eq!(bearer_of(&a).await, "Bearer t1");
    }

    #[tokio::test]
    async fn oauth_token_is_refreshed_after_a_401() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"access_token": "t1"})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"access_token": "t2"})),
            )
            .mount(&server)
            .await;
        let a = oauth(&server);
        assert_eq!(bearer_of(&a).await, "Bearer t1");
        assert!(a.invalidate().await);
        assert_eq!(bearer_of(&a).await, "Bearer t2");
    }

    #[tokio::test]
    async fn oauth_falls_back_to_client_secret_post() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("client_secret=s3cret"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token": "posted"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        assert_eq!(bearer_of(&oauth(&server)).await, "Bearer posted");
    }

    #[tokio::test]
    async fn api_key_locations() {
        unsafe { std::env::set_var("APISIP_AUTH_TEST_KEY", "k1") };
        let build = |location| {
            Authenticator::new(
                Some(&AuthConfig::ApiKey {
                    scheme: None,
                    name: "api_key".into(),
                    location,
                    value: "${APISIP_AUTH_TEST_KEY}".into(),
                }),
                reqwest::Client::new(),
                "http://x.test",
            )
            .unwrap()
        };
        let req = |a: Authenticator| async move {
            a.apply(reqwest::Client::new().get("http://x.test/a"))
                .await
                .unwrap()
                .build()
                .unwrap()
        };
        assert_eq!(
            req(build(ApiKeyLocation::Header)).await.headers()["api_key"],
            "k1"
        );
        assert_eq!(
            req(build(ApiKeyLocation::Query)).await.url().query(),
            Some("api_key=k1")
        );
        assert_eq!(
            req(build(ApiKeyLocation::Cookie)).await.headers()["cookie"],
            "api_key=k1"
        );
    }

    #[test]
    fn missing_variable_names_it_and_debug_hides_secrets() {
        let err = Authenticator::new(
            Some(&AuthConfig::Bearer {
                scheme: None,
                token: "${APISIP_AUTH_TEST_UNSET}".into(),
            }),
            reqwest::Client::new(),
            "http://x.test",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("APISIP_AUTH_TEST_UNSET"));

        unsafe { std::env::set_var("APISIP_AUTH_TEST_TOKEN", "do-not-print") };
        let a = Authenticator::new(
            Some(&AuthConfig::Bearer {
                scheme: None,
                token: "${APISIP_AUTH_TEST_TOKEN}".into(),
            }),
            reqwest::Client::new(),
            "http://x.test",
        )
        .unwrap();
        assert!(!format!("{a:?}").contains("do-not-print"));
    }
}
