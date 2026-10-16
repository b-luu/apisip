//! `apisip check`: a dry run. Validates the config and the credentials with
//! one request per stream, and writes nothing.

use anyhow::Result;

use crate::detect::stream_record_schema;
use crate::http::{ApiClient, Paginator, extract_records};
use crate::schema::{Drift, RecordSchema};
use crate::workspace::Workspace;

use super::{first_url, print_rate_limit_warning, select_streams};

/// Returns false when a check failed.
pub async fn run(ws: &Workspace) -> Result<bool> {
    let config = ws.load_config()?;
    println!("Config: {} is valid", ws.config_path().display());
    if config.source.rate_limit.is_none() {
        print_rate_limit_warning(false);
    }
    let spec = match ws.load_spec(&config).await {
        Ok(s) => Some(s),
        Err(e) => {
            println!("warning: could not load the spec ({e:#}); types will be inferred");
            None
        }
    };
    let client = ApiClient::new(&config.source)?;
    let mut ok = true;

    if config.destination.url.starts_with("s3://") {
        let store = crate::sink::Store::open(&config.destination, &ws.dir)?;
        match store.list(crate::sink::META_DIR).await {
            Ok(_) => println!("ok    destination {} is reachable", store.url),
            Err(e) => {
                ok = false;
                println!("FAIL  destination {}: {e:#}", store.url);
            }
        }
    } else {
        println!("ok    destination {}", config.destination.url);
    }

    for s in select_streams(&config, &[])? {
        let result = async {
            let url = Paginator::new(s.pagination.clone()).first(first_url(&client, &s)?);
            let page = client.get(&url).await?;
            anyhow::Ok(extract_records(&page.body, &s.records))
        }
        .await;
        match result {
            Ok(records) => {
                let schema = spec
                    .as_ref()
                    .and_then(|sp| stream_record_schema(sp, &s))
                    .and_then(|i| RecordSchema::from_spec(&i));
                let mut line = format!(
                    "ok    {:<24} {} record(s) on the first page",
                    s.name,
                    records.len()
                );
                if let Some(schema) = &schema {
                    let mut d = Drift::new(schema);
                    for r in &records {
                        d.observe(&schema.conform(r));
                    }
                    if !d.undeclared.is_empty() || !d.mistyped.is_empty() {
                        line.push_str(&format!(
                            ", drift: {} undeclared, {} mistyped field(s)",
                            d.undeclared.len(),
                            d.mistyped.len()
                        ));
                    }
                } else {
                    line.push_str(", schema inferred");
                }
                println!("{line}");
            }
            Err(e) => {
                ok = false;
                println!("FAIL  {:<24} {e:#}", s.name);
            }
        }
    }
    if ok {
        println!("\nAll checks passed. Next: apisip run");
    }
    Ok(ok)
}
