//! The HTTP side of a run: one shared client per API with auth, throttle
//! and retry, plus pagination that turns each response into the next request.

pub mod paginate;
pub mod retry;
pub mod throttle;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use url::Url;

use crate::auth::Authenticator;
use crate::config::{Source, interpolate};

pub use paginate::{Paginator, extract_records};
pub use retry::RetryPolicy;
pub use throttle::Throttle;

/// One response, parsed.
#[derive(Debug, Clone)]
pub struct Page {
    pub url: Url,
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Value,
}

#[derive(Debug)]
pub struct ApiClient {
    http: reqwest::Client,
    base_url: Url,
    auth: Authenticator,
    throttle: Throttle,
    retry: RetryPolicy,
    headers: HeaderMap,
}

pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("apisip/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(300))
        .build()
        .context("building the HTTP client")
}

impl ApiClient {
    /// Builds the client for a `[source]`. Fails when a `${VAR}` it needs is unset.
    pub fn new(source: &Source) -> Result<Arc<ApiClient>> {
        let http = http_client()?;
        let base_url = parse_base(&source.base_url)?;
        let auth = Authenticator::new(source.auth.as_ref(), http.clone(), &source.base_url)?;
        let mut headers = HeaderMap::new();
        for (k, v) in &source.headers {
            let v = interpolate(v).with_context(|| format!("header {k}"))?;
            headers.insert(
                HeaderName::from_bytes(k.as_bytes()).with_context(|| format!("header name {k}"))?,
                HeaderValue::from_str(&v).with_context(|| format!("header {k}"))?,
            );
        }
        Ok(Arc::new(ApiClient {
            http,
            base_url,
            auth,
            throttle: Throttle::new(source.rate_limit.as_ref()),
            retry: RetryPolicy::from(&source.retry),
            headers,
        }))
    }

    #[cfg(test)]
    pub fn with_retry_base(mut self: Arc<Self>, base: Duration) -> Arc<Self> {
        Arc::get_mut(&mut self).expect("unshared client").retry.base = base;
        self
    }

    /// `base_url` joined with a stream path such as `/users`.
    pub fn endpoint(&self, path: &str) -> Result<Url> {
        let joined = format!(
            "{}/{}",
            self.base_url.as_str().trim_end_matches('/'),
            path.trim_start_matches('/')
        );
        Url::parse(&joined).with_context(|| format!("invalid endpoint URL {joined}"))
    }

    /// GETs `url` with auth, throttling and retries, and parses the JSON body.
    pub async fn get(&self, url: &Url) -> Result<Page> {
        let mut refreshed_auth = false;
        let mut attempt = 0;
        loop {
            self.throttle.acquire().await;
            let rb = self.http.get(url.clone()).headers(self.headers.clone());
            let rb = self.auth.apply(rb).await?;
            tracing::debug!("GET {url}");
            let last = attempt + 1 >= self.retry.max_tries;
            let resp = match rb.send().await {
                Ok(r) => r,
                Err(e) if !last && (e.is_connect() || e.is_timeout() || e.is_request()) => {
                    let delay = self.retry.delay(attempt, None);
                    tracing::warn!("GET {url} failed ({e}), retrying in {delay:?}");
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    continue;
                }
                Err(e) => return Err(e).with_context(|| format!("GET {url}")),
            };
            let status = resp.status().as_u16();
            let headers = resp.headers().clone();
            self.throttle.observe(&headers).await;

            if status == 401 && !refreshed_auth && self.auth.invalidate().await {
                refreshed_auth = true;
                continue;
            }
            if self.retry.is_success(status) {
                let bytes = resp.bytes().await.with_context(|| format!("GET {url}"))?;
                let body = if bytes.iter().all(u8::is_ascii_whitespace) {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).with_context(|| {
                        format!("GET {url}: the response is not JSON: {}", snippet(&bytes))
                    })?
                };
                return Ok(Page {
                    url: url.clone(),
                    status,
                    headers,
                    body,
                });
            }
            if self.retry.is_retryable(status) && !last {
                let delay = self.retry.delay(attempt, Some(&headers));
                if status == 429 {
                    self.throttle.block_for(delay).await;
                }
                tracing::warn!(
                    "GET {url}: HTTP {status}, retry {}/{} in {delay:?}",
                    attempt + 1,
                    self.retry.max_tries - 1
                );
                if status != 429 {
                    tokio::time::sleep(delay).await;
                }
                attempt += 1;
                continue;
            }
            let body = resp.bytes().await.unwrap_or_default();
            let hint = match status {
                401 | 403 => " (check the credentials in .env)",
                404 => " (check base_url and the stream path in apisip.toml)",
                _ => "",
            };
            bail!("GET {url}: HTTP {status}{hint}: {}", snippet(&body));
        }
    }
}

/// The pages of one stream, lazily: records of each page in order. Dropping
/// the stream stops paging.
pub fn pages(
    client: Arc<ApiClient>,
    first: Url,
    pagination: Option<crate::config::Pagination>,
    records: String,
) -> impl futures::Stream<Item = Result<Vec<Value>>> {
    async_stream::try_stream! {
        let mut paginator = Paginator::new(pagination);
        let mut next = Some(paginator.first(first));
        while let Some(url) = next.take() {
            let page = client.get(&url).await?;
            let recs = extract_records(&page.body, &records);
            next = paginator.next(&page, &recs);
            yield recs;
        }
    }
}

fn parse_base(base: &str) -> Result<Url> {
    let base = interpolate(base).context("source.base_url")?;
    Url::parse(&base).with_context(|| format!("source.base_url {base:?} is not an absolute URL"))
}

fn snippet(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if text.chars().count() > 300 {
        format!("{}…", text.chars().take(300).collect::<String>())
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests;
