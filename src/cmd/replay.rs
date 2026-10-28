//! `apisip replay`: conform the quarantined records of each stream again,
//! with the current schema file and cast setting.
//!
//! Records that now pass are committed as data with the run id and
//! extraction time they were fetched with, in the folder of that run, so the
//! views order them as if they had never been quarantined. The records that
//! still fail replace the quarantine file. A replay that stops halfway can
//! run again: it writes the same data files over the first ones, so no
//! record is committed twice. Watermarks do not move.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use crate::config::{Cast, Format, Mode, StreamConfig};
use crate::schema::{EXTRACTED_AT, RUN_ID, RecordSchema};
use crate::sink::writer::reason;
use crate::sink::{DataWriter, JsonlWriter, Layout, StagedFile, Stamp, Store};
use crate::spec::Spec;
use crate::state::{Manifest, RunKind, RunStatus, State, StreamReport, StreamStatus};
use crate::workspace::Workspace;

use super::run::{check_owner, claim, fail_point, lock, print_views};
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
    let ctx = Replay {
        ws,
        spec: spec.as_ref(),
        store: &store,
        layout: &layout,
        format,
    };
    for (s, files) in &todo {
        let mut report = StreamReport::new("replay");
        if let Err(e) = ctx.stream(s, files, &mut report).await {
            report.status = StreamStatus::Failed;
            report.error = Some(format!("{e:#}"));
            store.drop_staged(&layout, &s.name).await;
        }
        m.streams.insert(s.name.clone(), report);
        m.save(&store).await?;
    }

    let failed = m.finish();
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
        let cast = if parquet { s.cast } else { Cast::None };
        let schema = load_schema(self.ws, self.spec, s)?.map(|sc| RecordSchema { cast, ..sc });
        if parquet && schema.is_none() {
            bail!("schemas/{name}.json is missing: replay needs the schema to type the records");
        }
        report.schema = schema.as_ref().map(|sc| sc.source);
        for file in files {
            self.file(s, schema.as_ref(), file, report).await?;
        }
        Ok(())
    }

    /// Replays one quarantine file. The data files are named after the file
    /// and its content, and the records that still fail replace the file: a
    /// replay that crashed before its end, run again, writes the same files
    /// over the first ones instead of adding duplicates.
    async fn file(
        &self,
        s: &StreamConfig,
        schema: Option<&RecordSchema>,
        file: &str,
        report: &mut StreamReport,
    ) -> Result<()> {
        let name = &s.name;
        let parquet = self.format == Format::Parquet;
        let snapshot = s.mode() == Mode::FullRefresh;
        let bytes = self.store.get(file).await?.unwrap_or_default();
        let stem = file
            .rsplit_once('/')
            .map_or(file, |(_, f)| f)
            .trim_end_matches(".jsonl");
        let tag = format!("replay-{stem}-{}", fnv(&bytes));
        // One writer per data folder: records go back to their run's folder.
        let mut writers: BTreeMap<String, DataWriter> = BTreeMap::new();
        let mut remaining: Option<JsonlWriter> = None;
        let mut kept = 0;

        for (i, line) in bytes
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .enumerate()
        {
            let mut entry: Value = serde_json::from_slice(line)
                .with_context(|| format!("{file}, line {}: not JSON", i + 1))?;
            let record = entry
                .get("record")
                .cloned()
                .with_context(|| format!("{file}, line {}: no `record`", i + 1))?;
            let run_id = entry.get(RUN_ID).and_then(Value::as_str).map(String::from);
            let extracted = entry
                .get(EXTRACTED_AT)
                .and_then(Value::as_str)
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&Utc));
            let conformed = schema.map(|sc| sc.conform(&record));
            let reason = match (&run_id, extracted, &conformed) {
                // Without them the record cannot go back to the run it was fetched in.
                (None, _, _) | (_, None, _) => {
                    Some(format!("no {RUN_ID} or {EXTRACTED_AT} to replay it with"))
                }
                (_, _, Some(c)) if parquet && !c.violations.is_empty() => {
                    Some(reason(&c.violations))
                }
                _ => None,
            };
            let (Some(run_id), Some(extracted), None) = (run_id, extracted, &reason) else {
                entry["reason"] = json!(reason);
                let q = remaining.get_or_insert_with(|| {
                    JsonlWriter::single(self.store.clone(), self.layout, file.to_string())
                });
                q.write(&entry).await?;
                kept += 1;
                continue;
            };

            let day = Layout::started_at(&run_id).unwrap_or(extracted);
            let mut dir = format!("{name}/dt={}", day.format("%Y-%m-%d"));
            if snapshot {
                dir.push_str(&format!("/run_id={run_id}"));
            }
            // No rotation: the same records always give the same files.
            let w = writers.entry(dir.clone()).or_insert_with(|| {
                DataWriter::new(
                    self.store.clone(),
                    self.layout,
                    name,
                    dir.clone(),
                    schema.filter(|_| parquet).map(RecordSchema::arrow),
                    u64::MAX,
                )
            });
            let stamp = Stamp {
                run_id: &run_id,
                extracted,
            };
            w.write(record, conformed, &stamp).await?;
            report.records += 1;
        }

        let mut data_files: Vec<StagedFile> = Vec::new();
        for w in writers.into_values() {
            data_files.extend(w.finish().await?);
        }
        for f in &mut data_files {
            // `<stream>-<run id>-0000.parquet` becomes `<stream>-<tag>-0000.parquet`.
            if let Some((dir, base)) = f.target.rsplit_once('/') {
                f.target = format!("{dir}/{}", base.replacen(&self.layout.run_id, &tag, 1));
            }
        }
        let remaining = match remaining {
            Some(q) => q.finish().await?,
            None => Vec::new(),
        };

        let _commit = self.store.commit_lock().await?;
        check_owner(self.store, &self.ws.folder_id()?).await?;
        for f in &data_files {
            let target = self.store.promote(f, None).await?;
            if !report.files.contains(&target) {
                report.files.push(target);
            }
        }
        fail_point("replay_after_move")?;
        if remaining.is_empty() {
            match self.store.delete(file).await {
                Ok(()) => {}
                Err(e) if is_not_found(&e) => {}
                Err(e) => return Err(e),
            }
        } else {
            for f in &remaining {
                report.files.push(self.store.promote(f, None).await?);
            }
            report.quarantined += kept;
        }
        Ok(())
    }
}

/// FNV-1a of a file's bytes, to name what a replay of it writes.
fn fnv(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn is_not_found(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<object_store::Error>(),
        Some(object_store::Error::NotFound { .. })
    )
}
