pub mod check;
pub mod init;
pub mod inspect;
pub mod list;
pub mod replay;
pub mod run;
pub mod schema_merge;
pub mod update;

use anyhow::{Result, bail};
use url::Url;

use crate::config::{Cast, Config, Pagination, StreamConfig, interpolate};
use crate::detect::stream_record_schema;
use crate::http::ApiClient;
use crate::schema::{RecordSchema, SOURCE_KEY};
use crate::sink::Store;
use crate::spec::Spec;
use crate::workspace::Workspace;

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

/// Where a stream's schema comes from: `spec`, `inferred`, `edited` (the
/// file differs from what apisip wrote), or `?` when the spec is unreadable.
pub fn schema_label(ws: &Workspace, spec: Option<&Spec>, s: &StreamConfig) -> &'static str {
    match ws.read_schema(&s.name).ok().flatten() {
        Some(_) if ws.schema_edited(&s.name).unwrap_or(false) => "edited",
        Some(v) if v.get(SOURCE_KEY).and_then(|v| v.as_str()) == Some("inferred") => "inferred",
        Some(_) => "spec",
        None => match spec {
            Some(spec) if stream_record_schema(spec, s).is_some() => "spec",
            Some(_) => "inferred",
            None => "?",
        },
    }
}

/// The destination, or `None` when it is a local folder nothing was written to yet.
pub fn open_existing_store(ws: &Workspace, config: &Config) -> Option<Store> {
    let url = interpolate(&config.destination.url).ok()?;
    if !url.starts_with("s3://") && !Store::local_dir(&url, &ws.dir).ok()?.exists() {
        return None;
    }
    Store::open(&config.destination, &ws.dir).ok()
}

/// A stream's record schema: `schemas/<stream>.json`, or the spec's when
/// the file is missing. `None` when there is neither: the run infers one.
pub fn load_schema(
    ws: &Workspace,
    spec: Option<&Spec>,
    s: &StreamConfig,
) -> Result<Option<RecordSchema>> {
    let cast = s.cast == Cast::Lossless;
    if let Some(v) = ws.read_schema(&s.name)? {
        return match RecordSchema::from_file(&v) {
            Some(schema) => Ok(Some(RecordSchema { cast, ..schema })),
            None => bail!(
                "schemas/{}.json has no `properties`: it must describe one record as an object",
                s.name
            ),
        };
    }
    let schema = spec
        .and_then(|sp| stream_record_schema(sp, s))
        .and_then(|i| RecordSchema::from_spec(&i))
        .map(|schema| RecordSchema { cast, ..schema });
    if schema.is_some() {
        eprintln!(
            "warning: schemas/{}.json is missing, so the spec's schema is used; `apisip update` writes the file",
            s.name
        );
    }
    Ok(schema)
}

/// The content of a schema file for a stream the spec describes.
pub fn spec_schema_file(spec: &Spec, s: &StreamConfig) -> Option<serde_json::Value> {
    let mut item = stream_record_schema(spec, s)?;
    RecordSchema::from_spec(&item)?;
    item[SOURCE_KEY] = "spec".into();
    Some(item)
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
