//! `apisip inspect [STREAM]`: what the destination holds for each stream,
//! read from `state.json`, the run manifests and the quarantine folders.
//! It never calls the API.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

use crate::config::{Config, StreamConfig};
use crate::sink::{META_DIR, Store};
use crate::state::{Manifest, RunKind, State, StreamReport, StreamStatus};
use crate::views::VIEWS_DIR;
use crate::workspace::Workspace;

use super::{open_existing_store, schema_label};

/// Manifests read, newest first: enough for the last runs of most folders.
const MANIFESTS: usize = 200;
const LAST_RUNS: usize = 5;

/// One stream's entry in one run manifest.
struct Run {
    run_id: String,
    kind: RunKind,
    report: StreamReport,
}

struct Quarantine {
    path: String,
    records: usize,
}

pub async fn run(ws: &Workspace, stream: Option<String>) -> Result<()> {
    let config = ws.load_config()?;
    let streams: Vec<&StreamConfig> = match &stream {
        Some(n) => match config.streams.iter().find(|s| &s.name == n) {
            Some(s) => vec![s],
            None => bail!(
                "no stream `{n}` in apisip.toml; known: {}",
                config
                    .streams
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
        None => config.streams.iter().collect(),
    };
    let Some(store) = open_existing_store(ws, &config) else {
        println!(
            "Nothing written yet to {}: run `apisip run` first.",
            config.destination.url
        );
        return Ok(());
    };
    let state = State::load(&store).await?;
    let runs = runs_by_stream(&store).await?;
    let mut quarantine = BTreeMap::new();
    for s in &streams {
        quarantine.insert(s.name.clone(), quarantine_files(&store, &s.name).await?);
    }
    let none = Vec::new();
    match stream {
        None => table(&streams, &state, &runs, &quarantine),
        Some(_) => {
            let s = streams[0];
            let spec = ws.load_spec(&config).await.ok();
            detail(
                s,
                &state,
                runs.get(&s.name).unwrap_or(&none),
                &quarantine[&s.name],
                schema_label(ws, spec.as_ref(), s),
                &store,
                &config,
            )
            .await?;
        }
    }
    Ok(())
}

/// The last runs of each stream, newest first.
async fn runs_by_stream(store: &Store) -> Result<BTreeMap<String, Vec<Run>>> {
    let mut metas = store.list(&format!("{META_DIR}/runs")).await?;
    // Run ids start with their timestamp.
    metas.sort_by(|a, b| b.location.cmp(&a.location));
    let mut out: BTreeMap<String, Vec<Run>> = BTreeMap::new();
    for meta in metas.iter().take(MANIFESTS) {
        let Some(bytes) = store.get(meta.location.as_ref()).await? else {
            continue;
        };
        let Ok(m) = serde_json::from_slice::<Manifest>(&bytes) else {
            tracing::warn!("{} is not a run manifest: skipped", meta.location);
            continue;
        };
        for (name, report) in m.streams {
            let runs = out.entry(name).or_default();
            if runs.len() < LAST_RUNS {
                runs.push(Run {
                    run_id: m.run_id.clone(),
                    kind: m.kind,
                    report,
                });
            }
        }
    }
    Ok(out)
}

async fn quarantine_files(store: &Store, stream: &str) -> Result<Vec<Quarantine>> {
    let mut out = Vec::new();
    for meta in store.list(&format!("{stream}/_quarantine")).await? {
        let path = meta.location.to_string();
        if !path.ends_with(".jsonl") {
            continue;
        }
        let bytes = store.get(&path).await?.unwrap_or_default();
        let records = bytes
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .count();
        out.push(Quarantine { path, records });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn mode(s: &StreamConfig, state: &State) -> String {
    let mut mode = s.mode().label().to_string();
    let unreliable = state.streams.get(&s.name).is_some_and(|st| {
        st.sort_is_unreliable(s.incremental.as_ref().and_then(|i| i.sort.as_ref()))
    });
    if unreliable {
        mode.push_str(" (unreliable)");
    }
    if !s.enabled {
        mode.push_str(" (disabled)");
    }
    mode
}

/// The destination as the user wrote it, to prefix paths inside it.
fn dest(config: &Config) -> &str {
    config
        .destination
        .url
        .trim_start_matches("file://")
        .trim_end_matches('/')
}

fn watermark<'a>(s: &StreamConfig, state: &'a State) -> Option<&'a str> {
    let cursor = &s.incremental.as_ref()?.cursor;
    state.streams.get(&s.name)?.watermark_for(cursor)
}

fn run_label(r: &Run) -> String {
    let at = crate::sink::Layout::started_at(&r.run_id)
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| r.run_id.clone());
    let status = match r.report.status {
        StreamStatus::Committed => "ok",
        StreamStatus::Failed => "failed",
    };
    match r.kind {
        RunKind::Fetch => format!("{at} {status}"),
        RunKind::Replay => format!("{at} {status} (replay)"),
    }
}

fn drift_fields(r: &StreamReport) -> usize {
    r.drift
        .as_ref()
        .map(|d| d.undeclared.len() + d.mistyped.len() + d.missing.len())
        .unwrap_or(0)
}

fn table(
    streams: &[&StreamConfig],
    state: &State,
    runs: &BTreeMap<String, Vec<Run>>,
    quarantine: &BTreeMap<String, Vec<Quarantine>>,
) {
    println!(
        "{:<24} {:<26} {:<26} {:<26} {:>8} {:>11} {:>6}",
        "STREAM", "MODE", "WATERMARK", "LAST RUN", "RECORDS", "QUARANTINED", "DRIFT"
    );
    for s in streams {
        let last = runs.get(&s.name).and_then(|r| r.first());
        let quarantined: usize = quarantine[&s.name].iter().map(|q| q.records).sum();
        println!(
            "{:<24} {:<26} {:<26} {:<26} {:>8} {:>11} {:>6}",
            s.name,
            mode(s, state),
            watermark(s, state).unwrap_or("-"),
            last.map(run_label).unwrap_or_else(|| "never".into()),
            last.map(|r| r.report.records.to_string())
                .unwrap_or_else(|| "-".into()),
            quarantined,
            last.map(|r| drift_fields(&r.report).to_string())
                .unwrap_or_else(|| "-".into()),
        );
    }
    println!(
        "\nRECORDS and DRIFT are from the last run; QUARANTINED counts every quarantine file."
    );
}

async fn detail(
    s: &StreamConfig,
    state: &State,
    runs: &[Run],
    quarantine: &[Quarantine],
    schema: &str,
    store: &Store,
    config: &Config,
) -> Result<()> {
    let st = state.streams.get(&s.name);
    println!("Stream:     {}", s.name);
    println!("Mode:       {}", mode(s, state));
    if let Some(i) = &s.incremental {
        println!("Cursor:     {}", i.cursor);
        println!("Watermark:  {}", watermark(s, state).unwrap_or("none yet"));
        if let Some(r) = st.and_then(|st| st.last_range.as_ref()) {
            println!("Last range: {} .. {}", r.min, r.max);
        }
        if let Some(sig) = st.and_then(|st| st.unreliable_sort.as_deref()) {
            println!("Unreliable: the API ignored the sort {sig}; every run reads all pages");
        }
    }
    println!("Schema:     schemas/{}.json ({schema})", s.name);

    println!("\nLast runs:");
    if runs.is_empty() {
        println!("  none");
    }
    for r in runs {
        let rep = &r.report;
        let mut line = format!(
            "  {:<28} {:<26} {} records, {} quarantined",
            r.run_id,
            run_label(r),
            rep.records,
            rep.quarantined
        );
        if rep.unchanged > 0 {
            line.push_str(&format!(", {} unchanged", rep.unchanged));
        }
        println!("{line}");
        if let Some(e) = &rep.error {
            println!("      error: {e}");
        }
    }
    if let Some(drift) = runs.first().and_then(|r| r.report.drift.as_ref()) {
        let lines = drift.summary();
        if !lines.is_empty() {
            println!("\nDrift in the last run:");
            for l in lines {
                println!("  {l}");
            }
        }
    }

    println!("\nQuarantine:");
    if quarantine.is_empty() {
        println!("  empty");
    }
    for q in quarantine {
        println!("  {}/{} ({} records)", dest(config), q.path, q.records);
    }
    if !quarantine.is_empty() {
        println!(
            "  `apisip replay --stream {}` re-checks them against the current schema.",
            s.name
        );
    }

    println!("\nViews:");
    let mut any = false;
    for kind in ["current", "history"] {
        let path = format!("{VIEWS_DIR}/{}_{kind}.sql", s.name);
        if store.get(&path).await?.is_some() {
            println!("  {}/{path}", dest(config));
            any = true;
        }
    }
    if !any {
        println!("  none yet");
    }
    Ok(())
}
