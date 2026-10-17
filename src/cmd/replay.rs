//! `apisip replay`: conform the quarantined records of each stream again,
//! with the current schema file and cast setting.
//!
//! Records that now pass are committed as data with the run id and
//! extraction time they were fetched with, so the views order them as if
//! they had never been quarantined. The rest go to a new quarantine file.
//! The replayed quarantine files are deleted after the commit; a crash in
//! between leaves duplicates, which the views remove. Watermarks do not move.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};

use crate::config::{Cast, Format, Mode, StreamConfig};
use crate::schema::{EXTRACTED_AT, RUN_ID, RecordSchema, wrap};
use crate::sink::{JsonlWriter, Layout, ParquetWriter, StagedFile, Store};
use crate::spec::Spec;
use crate::state::{Manifest, RunKind, RunStatus, State, StreamReport, StreamStatus};
use crate::workspace::Workspace;

use super::run::{claim, fail_point, lock, print_views};
use super::{load_schema, select_streams};

#[derive(Debug, Default, Clone)]
pub struct ReplayOptions {
    pub streams: Vec<String>,
}

/// Returns false when a stream failed.
pub async fn run(ws: &Workspace, o: ReplayOptions) -> Result<bool> {
    let config = ws.load_config()?;
    let _lock = lock(ws)?;
    let streams = select_streams(&config, &o.streams)?;
    let spec = ws.load_spec(&config).await.ok();
    let store = Arc::new(Store::open(&config.destination, &ws.dir)?);
    claim(&store, &ws.folder_id()?).await?;
    let layout = Layout::new(Utc::now());
    store.delete_stale_staging(layout.started).await?;

    let mut todo = Vec::new();
    for s in streams {
        let files = quarantine_files(&store, &s.name).await?;
        if !files.is_empty() {
            todo.push((s, files));
        }
    }
    if todo.is_empty() {
        println!("Nothing to replay: no quarantined records.");
        return Ok(true);
    }

    let mut m = Manifest {
        run_id: layout.run_id.clone(),
        started_at: layout.started,
        finished_at: None,
        status: RunStatus::Running,
        full_refresh: false,
        format: config.destination.format.extension().into(),
        streams: BTreeMap::new(),
        kind: RunKind::Replay,
    };
    m.save(&store).await?;
    let format = config.destination.format;
    let max_bytes = config.destination.rotate.max_bytes;
    let ctx = Replay {
        ws,
        spec: spec.as_ref(),
        store: &store,
        layout: &layout,
        format,
        max_bytes,
    };
    for (s, files) in &todo {
        let mut report = StreamReport {
            status: StreamStatus::Committed,
            mode: "replay".into(),
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
        if let Err(e) = ctx.stream(s, files, &mut report).await {
            report.status = StreamStatus::Failed;
            report.error = Some(format!("{e:#}"));
            let staged = format!("{}/{}/", layout.staging_root(), s.name);
            if let Err(e) = store.delete_prefix(&staged).await {
                tracing::warn!("could not clean up {staged}: {e:#}");
            }
        }
        m.streams.insert(s.name.clone(), report);
        m.save(&store).await?;
    }

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

    println!();
    for (name, r) in &m.streams {
        match r.status {
            StreamStatus::Committed => {
                println!(
                    "  {name:<24} {:>8} record(s) moved into the data, {} still quarantined",
                    r.records, r.quarantined
                );
                for f in r.files.iter().filter(|f| f.contains("/_quarantine/")) {
                    println!("      still quarantined: {f}");
                }
            }
            StreamStatus::Failed => println!(
                "  {name:<24} FAILED: {}",
                r.error.as_deref().unwrap_or("unknown error")
            ),
        }
    }
    println!(
        "\nReplay {} done. Manifest: {}",
        m.run_id,
        Manifest::path(&m.run_id)
    );
    let state = State::load(&store).await?;
    print_views(
        crate::views::write_all(&store, &config, &state).await,
        &config,
        &store,
    );
    Ok(failed == 0)
}

/// The quarantine files of a stream, oldest first.
async fn quarantine_files(store: &Store, stream: &str) -> Result<Vec<String>> {
    let mut files: Vec<String> = store
        .list(&format!("{stream}/_quarantine"))
        .await?
        .into_iter()
        .map(|m| m.location.to_string())
        .filter(|p| p.ends_with(".jsonl"))
        .collect();
    files.sort();
    Ok(files)
}

struct Replay<'a> {
    ws: &'a Workspace,
    spec: Option<&'a Spec>,
    store: &'a Arc<Store>,
    layout: &'a Layout,
    format: Format,
    max_bytes: u64,
}

enum Writer {
    Jsonl(Box<JsonlWriter>),
    Parquet(Box<ParquetWriter>),
}

impl Replay<'_> {
    async fn stream(
        &self,
        s: &StreamConfig,
        files: &[String],
        report: &mut StreamReport,
    ) -> Result<()> {
        let name = &s.name;
        let parquet = self.format == Format::Parquet;
        let cast = parquet && s.cast == Cast::Lossless;
        let schema = load_schema(self.ws, self.spec, s)?.map(|sc| RecordSchema { cast, ..sc });
        if parquet && schema.is_none() {
            bail!("schemas/{name}.json is missing: replay needs the schema to type the records");
        }
        report.schema = schema.as_ref().map(|sc| sc.source);
        let snapshot = s.mode() == Mode::FullRefresh;
        // One writer per data folder: a snapshot's records go back to their run's folder.
        let mut writers: BTreeMap<String, Writer> = BTreeMap::new();
        let mut quarantine: Option<JsonlWriter> = None;

        for file in files {
            let bytes = self.store.get(file).await?.unwrap_or_default();
            for (i, line) in bytes
                .split(|b| *b == b'\n')
                .filter(|l| !l.is_empty())
                .enumerate()
            {
                let entry: Value = serde_json::from_slice(line)
                    .with_context(|| format!("{file}, line {}: not JSON", i + 1))?;
                let record = entry
                    .get("record")
                    .cloned()
                    .with_context(|| format!("{file}, line {}: no `record`", i + 1))?;
                let run_id = entry
                    .get(RUN_ID)
                    .and_then(Value::as_str)
                    .unwrap_or(&self.layout.run_id)
                    .to_string();
                let extracted = entry
                    .get(EXTRACTED_AT)
                    .and_then(Value::as_str)
                    .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                    .map(|t| t.with_timezone(&Utc))
                    .unwrap_or(self.layout.started);
                let extracted_text = extracted.to_rfc3339_opts(SecondsFormat::Micros, true);

                let conformed = schema.as_ref().map(|sc| sc.conform(&record));
                let violations = conformed
                    .as_ref()
                    .map(|c| c.violations.as_slice())
                    .unwrap_or_default();
                if parquet && !violations.is_empty() {
                    let q = quarantine.get_or_insert_with(|| {
                        JsonlWriter::single(
                            self.store.clone(),
                            self.layout,
                            self.layout.quarantine(name),
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
                    continue;
                }

                let dir = if snapshot {
                    let day = Layout::started_at(&run_id).unwrap_or(extracted);
                    format!("{name}/dt={}/run_id={run_id}", day.format("%Y-%m-%d"))
                } else {
                    self.layout.data_dir(name, false)
                };
                let w = writers.entry(dir.clone()).or_insert_with(|| match &schema {
                    Some(sc) if parquet => Writer::Parquet(Box::new(ParquetWriter::new(
                        self.store.clone(),
                        self.layout,
                        name,
                        dir.clone(),
                        sc.arrow(),
                        self.max_bytes,
                    ))),
                    _ => Writer::Jsonl(Box::new(JsonlWriter::new(
                        self.store.clone(),
                        self.layout,
                        name,
                        dir.clone(),
                        self.max_bytes,
                    ))),
                });
                match w {
                    Writer::Parquet(w) => {
                        let mut row = conformed.expect("parquet has a schema").row;
                        row.insert(RUN_ID.into(), Value::String(run_id));
                        row.insert(EXTRACTED_AT.into(), json!(extracted.timestamp_micros()));
                        w.write(row).await?;
                    }
                    Writer::Jsonl(w) => {
                        let mut v = wrap(record);
                        let obj = v.as_object_mut().expect("wrapped");
                        obj.insert(RUN_ID.into(), Value::String(run_id));
                        obj.insert(EXTRACTED_AT.into(), Value::String(extracted_text));
                        w.write(&v).await?;
                    }
                }
                report.records += 1;
            }
        }

        let mut data_files: Vec<StagedFile> = Vec::new();
        for w in writers.into_values() {
            data_files.extend(match w {
                Writer::Jsonl(w) => w.finish().await?,
                Writer::Parquet(w) => w.finish().await?,
            });
        }
        let quarantine_files = match quarantine {
            Some(q) => q.finish().await?,
            None => Vec::new(),
        };

        let _commit = self.store.commit_lock().await?;
        let prefix = Layout::part_prefix(name);
        let merge = (!parquet && !snapshot).then_some((prefix.as_str(), self.max_bytes));
        for f in &data_files {
            let target = self.store.promote(f, merge).await?;
            if !report.files.contains(&target) {
                report.files.push(target);
            }
        }
        for f in &quarantine_files {
            report.files.push(self.store.promote(f, None).await?);
        }
        fail_point("replay_after_move")?;
        for f in files {
            self.store.delete(f).await?;
        }
        Ok(())
    }
}
