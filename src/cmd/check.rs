//! `apisip check`: a dry run. Validates the config and the credentials with
//! one request per stream, and writes nothing.

use anyhow::{Context, Result};

use crate::config::{Cast, Format};
use crate::http::{ApiClient, Paginator, extract_records};
use crate::schema::{Drift, RecordSchema};
use crate::sink::Store;
use crate::state::{State, foreign_owner};
use crate::workspace::Workspace;

use super::{first_url, load_schema, print_rate_limit_warning, select_streams};

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
    let client = ApiClient::for_config(&config)?;
    let mut ok = true;

    let url = crate::config::interpolate(&config.destination.url).context("destination.url")?;
    if !url.starts_with("s3://") && !Store::local_dir(&url, &ws.dir)?.exists() {
        println!("ok    destination {url} (the first run creates it)");
    } else {
        let store = Store::open(&config.destination, &ws.dir)?;
        match State::load(&store).await {
            Ok(state) => match (state.owner.as_deref(), ws.read_folder_id()?) {
                (None, _) => println!(
                    "ok    destination {} (the first run claims it for this folder)",
                    store.url
                ),
                (Some(o), Some(id)) if o == id => {
                    println!("ok    destination {} belongs to this folder", store.url)
                }
                (Some(o), id) => {
                    ok = false;
                    println!("FAIL  {}", foreign_owner(&store.url, o, id.as_deref()));
                }
            },
            Err(e) => {
                ok = false;
                println!("FAIL  destination {}: {e:#}", store.url);
            }
        }
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
                // As in `run`: casts and quarantine apply to Parquet only.
                let parquet = config.destination.format == Format::Parquet;
                let schema = load_schema(ws, spec.as_ref(), &s)?.map(|sc| RecordSchema {
                    cast: if parquet { sc.cast } else { Cast::None },
                    ..sc
                });
                let mut line = format!(
                    "ok    {:<24} {} record(s) on the first page",
                    s.name,
                    records.len()
                );
                let mut details = Vec::new();
                if let Some(schema) = &schema {
                    let mut d = Drift::new(schema);
                    let mut violating = 0;
                    for r in &records {
                        let c = schema.conform(r);
                        violating += usize::from(!c.violations.is_empty());
                        d.observe(&c);
                    }
                    if violating > 0 && parquet {
                        line.push_str(&format!(", {violating} would be quarantined"));
                    }
                    if !d.is_empty() {
                        line.push_str(", drift:");
                        details = d.summary();
                    }
                } else {
                    line.push_str(", schema inferred");
                }
                println!("{line}");
                for l in details {
                    println!("        {l}");
                }
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
