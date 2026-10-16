pub mod check;
pub mod init;
pub mod list;
pub mod run;

use anyhow::{Result, bail};
use url::Url;

use crate::config::{Config, Pagination, StreamConfig, interpolate};
use crate::http::ApiClient;

/// The streams to run: the named ones, or every enabled one.
pub fn select_streams(config: &Config, names: &[String]) -> Result<Vec<StreamConfig>> {
    if names.is_empty() {
        return Ok(config
            .streams
            .iter()
            .filter(|s| s.enabled)
            .cloned()
            .collect());
    }
    names
        .iter()
        .map(|n| match config.streams.iter().find(|s| &s.name == n) {
            Some(s) if s.enabled => Ok(s.clone()),
            Some(_) => bail!("stream `{n}` is disabled in apisip.toml (set enabled = true)"),
            None => bail!(
                "no stream `{n}` in apisip.toml; known: {}",
                config
                    .streams
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
        .collect()
}

/// The endpoint URL with the stream's fixed params, before paging and incremental params.
pub fn first_url(client: &ApiClient, s: &StreamConfig) -> Result<Url> {
    let mut url = client.endpoint(&s.path)?;
    for (k, v) in &s.params {
        crate::http::paginate::set_param(&mut url, k, &interpolate(v)?);
    }
    if let Some(sort) = s.incremental.as_ref().and_then(|i| i.sort.as_ref()) {
        crate::http::paginate::set_param(&mut url, &sort.param, &sort.value);
        for (k, v) in &sort.params {
            crate::http::paginate::set_param(&mut url, k, v);
        }
    }
    Ok(url)
}

pub fn pagination_label(p: &Option<Pagination>) -> &'static str {
    match p {
        None => "single page",
        Some(Pagination::PageNumber { .. }) => "page number",
        Some(Pagination::Offset { .. }) => "offset",
        Some(Pagination::Cursor { .. }) => "cursor",
        Some(Pagination::NextUrl { .. }) => "next url",
        Some(Pagination::LinkHeader) => "link header",
    }
}

/// The warning printed when no static rate limit is configured.
pub fn print_rate_limit_warning(long: bool) {
    if long {
        eprintln!(
            "warning: no rate limit was found in the spec, so none is configured.\n\
             apisip still backs off on 429 responses and follows rate-limit headers, but a\n\
             static limit avoids hitting the API too hard. Check the provider's docs or\n\
             pricing page, then add under [source] in apisip.toml:\n\
             \n    rate_limit = {{ max_calls = 100, period = \"1m\" }}\n"
        );
    } else {
        eprintln!(
            "warning: no rate_limit in apisip.toml; add `rate_limit = {{ max_calls = N, period = \"1m\" }}` under [source]"
        );
    }
}
