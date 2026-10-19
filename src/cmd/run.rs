//! `apisip run`: fetch every enabled stream, stage its files, then commit
//! them in order: move into place, advance the watermark, write the manifest,
//! clean up staging.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::pin::pin;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{SecondsFormat, Utc};
use futures::{StreamExt, TryStreamExt};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::config::{Cast, Config, Format, Mode, StreamConfig};
use crate::http::{self, ApiClient};
use crate::incremental::{
    Boundary, BoundaryTracker, CursorValue, EarlyStop, MAX_BOUNDARY, Range, field,
};
use crate::schema::{Drift, EXTRACTED_AT, RUN_ID, RecordSchema, wrap};
use crate::sink::{JsonlWriter, Layout, ParquetWriter, StagedFile, Store};
use crate::spec::Spec;
use crate::state::{
    CursorRange, Manifest, Overlap, RunKind, RunStatus, State, StreamReport, StreamStatus,
    foreign_owner, sort_signature,
};
use crate::workspace::Workspace;

use super::{first_url, load_schema, print_rate_limit_warning, select_streams};

#[derive(Debug, Default, Clone)]
pub struct RunOptions {
    pub streams: Vec<String>,
    pub full_refresh: bool,
    pub strict: bool,
}

struct Ctx {
    ws: Workspace,
    config: Config,
    spec: Option<Spec>,
    client: Arc<ApiClient>,
    store: Arc<Store>,
    layout: Layout,
    state: Mutex<State>,
    options: RunOptions,
}

/// Stops before anything is written when another folder owns the
/// destination; claims it for this folder when nobody does.
pub(super) async fn claim(store: &Store, folder_id: &str) -> Result<State> {
    let state = State::load(store).await?;
    let state = match &state.owner {
        Some(_) => state,
        None => {
            let _commit = store.commit_lock().await?;
            State::update(store, |s| {
                s.owner.get_or_insert_with(|| folder_id.to_string());
            })
            .await
            .context("claiming the destination for this folder")?
        }
    };
    match state.owner.as_deref() {
        Some(o) if o != folder_id => bail!(foreign_owner(&store.url, o, Some(folder_id))),
        _ => Ok(state),
    }
}

/// Returns false when a stream failed.
pub async fn run(ws: &Workspace, options: RunOptions) -> Result<bool> {
    let config = ws.load_config()?;
    // Held until the run ends; the OS releases it if the process dies.
    let _lock = lock(ws)?;
    let streams = select_streams(&config, &options.streams)?;
    if config.source.rate_limit.is_none() {
        print_rate_limit_warning(false);
    }
    let spec = match ws.load_spec(&config).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("warning: could not load the spec ({e:#}); record types are inferred");
            None
        }
    };
    let client = ApiClient::for_config(&config)?;
    let store = Arc::new(Store::open(&config.destination, &ws.dir)?);
    let layout = Layout::new(Utc::now());
    let state = claim(&store, &ws.folder_id()?).await?;
    // Leftovers of an interrupted run were never committed: drop them.
    store.delete_stale_staging(layout.started).await?;

    let manifest = Mutex::new(Manifest {
        run_id: layout.run_id.clone(),
        started_at: layout.started,
        finished_at: None,
        status: RunStatus::Running,
        full_refresh: options.full_refresh,
        format: config.destination.format.extension().into(),
        streams: BTreeMap::new(),
        kind: RunKind::Fetch,
    });
    manifest.lock().await.save(&store).await?;
    eprintln!(
        "Run {}: {} stream(s) into {}",
        layout.run_id,
        streams.len(),
        store.url
    );

    let concurrency = config.source.concurrency;
    let ctx = Arc::new(Ctx {
        ws: ws.clone(),
        config,
        spec,
        client,
        store: store.clone(),
        layout: layout.clone(),
        state: Mutex::new(state),
        options,
    });
    let manifest = &manifest;
    futures::stream::iter(streams)
        .map(|s| {
            let ctx = ctx.clone();
            async move {
                let report = run_stream(&ctx, &s).await;
                let mut m = manifest.lock().await;
                m.streams.insert(s.name.clone(), report);
                if let Err(e) = m.save(&ctx.store).await {
                    tracing::warn!("could not write the manifest: {e:#}");
                }
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<()>>()
        .await;

    let mut m = manifest.lock().await;
    let failed = m
        .streams
        .values()
        .filter(|r| r.status == StreamStatus::Failed)
        .count();
    m.status = match failed {
        0 => RunStatus::Success,
        n if n == m.streams.len() => RunStatus::Failed,
        _ => RunStatus::Partial,
    };
    m.finished_at = Some(Utc::now());
    m.save(&store).await?;
    store.delete_prefix(&layout.staging_root()).await?;
    let state = ctx.state.lock().await;
    let views = crate::views::write_all(&store, &ctx.config, &state).await;
    print_summary(&m, &store);
    print_views(views, &ctx.config, &store);
    Ok(failed == 0)
}

pub(super) fn print_views(views: Result<Vec<String>>, config: &Config, store: &Store) {
    match views {
        Ok(v) if !v.is_empty() => println!(
            "Views: {}/{}/all.sql (duckdb -c \".read {}/all.sql\")",
            config
                .destination
                .url
                .trim_start_matches("file://")
                .trim_end_matches('/'),
            crate::views::VIEWS_DIR,
            view_dir_hint(store)
        ),
        Ok(_) => {}
        Err(e) => eprintln!("warning: could not write the views: {e:#}"),
    }
}

async fn run_stream(ctx: &Ctx, s: &StreamConfig) -> StreamReport {
    let mut report = StreamReport {
        status: StreamStatus::Committed,
        mode: s.mode().label().into(),
        error: None,
        records: 0,
        quarantined: 0,
        unchanged: 0,
        pages: 0,
        files: Vec::new(),
        requested_from: None,
        observed: None,
        overlap: None,
        unreliable_sort: false,
        warnings: Vec::new(),
        schema: None,
        drift: None,
    };
    if let Err(e) = fetch_and_commit(ctx, s, &mut report).await {
        report.status = StreamStatus::Failed;
        report.error = Some(format!("{e:#}"));
        let staged = format!("{}/{}/", ctx.layout.staging_root(), s.name);
        if let Err(e) = ctx.store.delete_prefix(&staged).await {
            tracing::warn!("could not clean up {staged}: {e:#}");
        }
    }
    report
}

enum DataWriter {
    Jsonl(Box<JsonlWriter>),
    Parquet(Box<ParquetWriter>),
}

async fn fetch_and_commit(ctx: &Ctx, s: &StreamConfig, report: &mut StreamReport) -> Result<()> {
    let name = &s.name;
    let format = ctx.config.destination.format;
    let max_bytes = ctx.config.destination.rotate.max_bytes;
    let run_id = ctx.layout.run_id.clone();
    let previous = ctx
        .state
        .lock()
        .await
        .streams
        .get(name)
        .cloned()
        .unwrap_or_default();
    let mode = s.mode();
    let cursor = s.cursor();
    let sort = s.incremental.as_ref().and_then(|i| i.sort.as_ref());
    let watermark = match (ctx.options.full_refresh, cursor) {
        (false, Some(c)) => previous.watermark_for(c).map(str::to_string),
        _ => None,
    };

    let mut url = first_url(&ctx.client, s)?;
    let mut early = None;
    match mode {
        Mode::Filter => {
            if let (Some(w), Some(param)) = (
                &watermark,
                s.incremental.as_ref().and_then(|i| i.param.as_ref()),
            ) {
                http::paginate::set_param(&mut url, param, w);
                report.requested_from = Some(w.clone());
            }
        }
        Mode::EarlyStop => {
            if previous.sort_is_unreliable(sort) {
                report.unreliable_sort = true;
                report.warnings.push(
                    "an earlier run found the descending sort unreliable: fetching everything"
                        .into(),
                );
            } else {
                report.requested_from = watermark.clone();
                early = Some(EarlyStop::new(
                    cursor.unwrap_or_default(),
                    watermark.as_deref(),
                ));
            }
        }
        Mode::FullRefresh => {}
    }
    let snapshot = mode == Mode::FullRefresh;
    let dir = ctx.layout.data_dir(name, snapshot);

    // JSONL keeps records as received, so nothing is cast there.
    let cast = if format == Format::Parquet {
        s.cast
    } else {
        Cast::None
    };
    let mut schema =
        load_schema(&ctx.ws, ctx.spec.as_ref(), s)?.map(|sc| RecordSchema { cast, ..sc });
    let mut drift = schema.as_ref().map(Drift::new);
    let mut writer = match format {
        Format::Jsonl => Some(DataWriter::Jsonl(Box::new(JsonlWriter::new(
            ctx.store.clone(),
            &ctx.layout,
            name,
            dir.clone(),
            max_bytes,
        )))),
        Format::Parquet => None,
    };
    let mut quarantine: Option<JsonlWriter> = None;
    let mut range = Range::new(watermark.as_deref());
    // Snapshots keep every row; incremental runs skip what the last run wrote at the watermark.
    let boundary = watermark
        .as_deref()
        .filter(|_| !snapshot)
        .and_then(|w| Boundary::new(w, &previous.boundary));
    let mut tracker = BoundaryTracker::default();
    let mut mistyped_cursors = 0usize;

    let mut pages = pin!(http::pages(
        ctx.client.clone(),
        url,
        s.pagination.clone(),
        s.records.clone()
    ));
    while let Some(records) = pages.try_next().await? {
        report.pages += 1;
        let extracted = Utc::now();
        let extracted_text = extracted.to_rfc3339_opts(SecondsFormat::Micros, true);
        let (mut records, stop) = match &mut early {
            Some(g) => g.page(records),
            None => (records, false),
        };
        if let (Some(b), Some(c)) = (&boundary, cursor) {
            let before = records.len();
            records.retain(|r| !b.unchanged(r, c));
            report.unchanged += (before - records.len()) as u64;
        }
        if schema.is_none() && format == Format::Parquet && !records.is_empty() {
            // Written once: later runs type their columns the same way.
            let inferred = RecordSchema {
                cast,
                ..RecordSchema::infer(&records)
            };
            ctx.ws.write_schema(name, &inferred.to_file())?;
            drift = Some(Drift::new(&inferred));
            schema = Some(inferred);
        }
        if writer.is_none()
            && let Some(sc) = &schema
        {
            writer = Some(DataWriter::Parquet(Box::new(ParquetWriter::new(
                ctx.store.clone(),
                &ctx.layout,
                name,
                dir.clone(),
                sc.arrow(),
                max_bytes,
            ))));
        }
        for record in records {
            let conformed = schema.as_ref().map(|sc| sc.conform(&record));
            if let (Some(c), Some(d)) = (&conformed, &mut drift) {
                d.observe(c);
            }
            let violations = conformed
                .as_ref()
                .map(|c| c.violations.as_slice())
                .unwrap_or_default();
            // Only a written record with a well-typed cursor moves the watermark.
            let quarantined = format == Format::Parquet && !violations.is_empty();
            if let Some(c) = cursor
                && !quarantined
                && let Some(v) = field(&record, c)
            {
                if violations.iter().any(|v| v.path == c) {
                    mistyped_cursors += 1;
                } else {
                    range.observe(v);
                    tracker.observe(&record, v);
                }
            }
            if !violations.is_empty() && ctx.options.strict {
                bail!(
                    "a record does not match the spec ({}); run without --strict to {}",
                    violations[0],
                    if format == Format::Parquet {
                        "quarantine it"
                    } else {
                        "keep it and report drift"
                    }
                );
            }
            match writer.as_mut() {
                Some(DataWriter::Jsonl(w)) => {
                    let mut v = wrap(record);
                    let obj = v.as_object_mut().expect("wrapped");
                    obj.insert(RUN_ID.into(), Value::String(run_id.clone()));
                    obj.insert(EXTRACTED_AT.into(), Value::String(extracted_text.clone()));
                    w.write(&v).await?;
                    report.records += 1;
                }
                Some(DataWriter::Parquet(w)) if violations.is_empty() => {
                    let mut row = conformed.expect("parquet has a schema").row;
                    row.insert(RUN_ID.into(), Value::String(run_id.clone()));
                    row.insert(EXTRACTED_AT.into(), json!(extracted.timestamp_micros()));
                    w.write(row).await?;
                    report.records += 1;
                }
                Some(DataWriter::Parquet(_)) => {
                    let q = quarantine.get_or_insert_with(|| {
                        JsonlWriter::single(
                            ctx.store.clone(),
                            &ctx.layout,
                            ctx.layout.quarantine(name),
                        )
                    });
                    let reason: Vec<String> = violations.iter().map(|v| v.to_string()).collect();
                    q.write(&json!({
                        "reason": reason.join("; "),
                        RUN_ID: run_id,
                        EXTRACTED_AT: extracted_text,
                        "record": record,
                    }))
                    .await?;
                    report.quarantined += 1;
                }
                None => unreachable!("a Parquet writer exists once records arrive"),
            }
        }
        if stop {
            tracing::info!("{name}: reached the watermark, stopping early");
            break;
        }
    }
    let unreliable = early.as_ref().is_some_and(|g| g.unreliable);
    if unreliable {
        report.unreliable_sort = true;
        report.warnings.push(
            "the cursor went up under a descending sort: the API does not sort as documented. \
             This run fetched everything; later runs will too."
                .into(),
        );
    }
    let stray = mistyped_cursors + range.ignored();
    if stray > 0 {
        report.warnings.push(format!(
            "{stray} record(s) have a `{}` value that does not match the spec or the other \
             values: they were written but do not move the watermark",
            cursor.unwrap_or_default()
        ));
    }
    if let Some(d) = &mut drift {
        d.finish(report.records + report.quarantined);
        if !d.is_empty() {
            report.drift = Some(d.clone());
        }
    }
    report.schema = schema.as_ref().map(|s| s.source);

    let data_files: Vec<StagedFile> = match writer {
        Some(DataWriter::Jsonl(w)) => w.finish().await?,
        Some(DataWriter::Parquet(w)) => w.finish().await?,
        None => Vec::new(),
    };
    let quarantine_files = match quarantine {
        Some(q) => q.finish().await?,
        None => Vec::new(),
    };
    fail_point("after_staging")?;

    // Copies of this folder may commit to the same destination at the same
    // time. A local destination has no conditional writes: commits take turns.
    let _commit = ctx.store.commit_lock().await?;

    // 1. Move the files into the data path.
    let prefix = Layout::part_prefix(name);
    let merge = (format == Format::Jsonl && !snapshot).then_some((prefix.as_str(), max_bytes));
    for f in &data_files {
        let target = ctx.store.promote(f, merge).await?;
        if !report.files.contains(&target) {
            report.files.push(target);
        }
    }
    for f in &quarantine_files {
        report.files.push(ctx.store.promote(f, None).await?);
    }
    fail_point("after_move")?;

    // 2. Advance the watermark, on the state as it is now: another run may
    // have committed since this one started.
    let observed = match (range.min_raw(), range.max_raw()) {
        (Some(min), Some(max)) => Some(CursorRange { min, max }),
        _ => None,
    };
    if tracker.overflow && !snapshot {
        report.warnings.push(format!(
            "more than {MAX_BOUNDARY} records share the newest `{}` value: the next run writes \
             them again (the views deduplicate them)",
            cursor.unwrap_or_default()
        ));
    }
    let written_at_max = observed
        .as_ref()
        .filter(|_| !snapshot)
        .map(|o| tracker.finish(&o.max))
        .unwrap_or_default();
    let committed_at = Utc::now();
    let mut overlap = None;
    let fresh = State::update(&ctx.store, |state| {
        let entry = state.streams.entry(name.clone()).or_default();
        overlap = None;
        if let (Some(prev), Some(obs)) = (&entry.last_range, &observed)
            && CursorValue::parse(&obs.min) < CursorValue::parse(&prev.max)
        {
            overlap = Some(Overlap {
                previous_run_id: entry.last_run_id.clone(),
                previous_max: prev.max.clone(),
            });
        }
        if let (Some(c), Some(obs)) = (cursor, &observed) {
            let new = CursorValue::parse(&obs.max);
            match entry
                .watermark_for(c)
                .map(|w| new.partial_cmp(&CursorValue::parse(w)))
            {
                None | Some(Some(Ordering::Greater)) => {
                    entry.watermark = Some(obs.max.clone());
                    entry.boundary = written_at_max.clone();
                }
                // Another run committed the same watermark: keep both sets.
                Some(Some(Ordering::Equal)) => {
                    let mut all: BTreeSet<String> = entry.boundary.drain(..).collect();
                    all.extend(written_at_max.iter().cloned());
                    if all.len() <= MAX_BOUNDARY {
                        entry.boundary = all.into_iter().collect();
                    }
                }
                Some(_) => {}
            }
            entry.cursor = Some(c.to_string());
        }
        if observed.is_some() {
            entry.last_range = observed.clone();
        }
        if unreliable && let Some(sort) = sort {
            entry.unreliable_sort = Some(sort_signature(sort));
        }
        entry.last_run_id = Some(run_id.clone());
        entry.last_committed_at = Some(committed_at);
    })
    .await
    .context("saving the watermark")?;
    report.overlap = overlap;
    *ctx.state.lock().await = fresh;
    report.observed = observed;
    Ok(())
}

/// One run at a time per folder: a second run would delete the first one's
/// staging files and overwrite its watermarks.
pub(super) fn lock(ws: &Workspace) -> Result<std::fs::File> {
    let dir = ws.meta_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("run.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("another `apisip run` is in progress in this folder; wait for it to finish")
        }
        // Some network filesystems cannot lock: run anyway, with a warning.
        Err(std::fs::TryLockError::Error(e)) => {
            eprintln!(
                "warning: cannot lock {} ({e}); make sure no other run uses this folder",
                path.display()
            );
            Ok(file)
        }
    }
}

/// Debug builds fail on purpose at a named point, for the crash tests.
pub(super) fn fail_point(name: &str) -> Result<()> {
    if cfg!(debug_assertions) && std::env::var("APISIP_FAIL_POINT").as_deref() == Ok(name) {
        bail!("injected failure at {name}");
    }
    Ok(())
}

fn view_dir_hint(store: &Store) -> String {
    match &store.local_root {
        Some(p) => p.join(crate::views::VIEWS_DIR).display().to_string(),
        None => format!(
            "{}/{}",
            store.url.trim_end_matches('/'),
            crate::views::VIEWS_DIR
        ),
    }
}

fn print_summary(m: &Manifest, store: &Store) {
    println!();
    for (name, r) in &m.streams {
        match r.status {
            StreamStatus::Committed => {
                let mut line = format!(
                    "  {name:<24} {:<13} {:>8} records  {:>3} file(s)",
                    r.mode,
                    r.records,
                    r.files.len()
                );
                if let Some(from) = &r.requested_from {
                    line.push_str(&format!("  since {from}"));
                }
                if r.unchanged > 0 {
                    line.push_str(&format!("  {} unchanged", r.unchanged));
                }
                if r.quarantined > 0 {
                    line.push_str(&format!("  {} quarantined", r.quarantined));
                }
                println!("{line}");
            }
            StreamStatus::Failed => {
                println!(
                    "  {name:<24} FAILED: {}",
                    r.error.as_deref().unwrap_or("unknown error")
                );
            }
        }
        for w in &r.warnings {
            println!("      warning: {w}");
        }
        if let Some(o) = &r.overlap {
            println!(
                "      note: overlaps the previous run (records older than {}); the views deduplicate them",
                o.previous_max
            );
        }
        if let Some(d) = &r.drift {
            for line in d.summary() {
                println!("      drift: {line}");
            }
        }
    }
    let status = match m.status {
        RunStatus::Success => "done",
        RunStatus::Partial => "partly failed",
        RunStatus::Failed => "failed",
        RunStatus::Running => "running",
    };
    println!(
        "\nRun {} {status}. Data: {}  Manifest: {}",
        m.run_id,
        store.url,
        Manifest::path(&m.run_id)
    );
}
