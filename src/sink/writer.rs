//! What `run` and `replay` share to write a stream: one data writer over
//! both formats, the fields apisip stamps on each row, and the line a
//! quarantined record becomes.

use std::sync::Arc;

use anyhow::Result;
use arrow::datatypes::Schema;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};

use super::{JsonlWriter, Layout, ParquetWriter, StagedFile, Store};
use crate::schema::{Conformed, EXTRACTED_AT, RUN_ID, Violation, wrap};

/// The run a record was fetched in, and when.
pub struct Stamp<'a> {
    pub run_id: &'a str,
    pub extracted: DateTime<Utc>,
}

impl Stamp<'_> {
    /// The extraction time as JSONL and quarantine lines write it.
    pub fn extracted_text(&self) -> String {
        self.extracted.to_rfc3339_opts(SecondsFormat::Micros, true)
    }
}

pub enum DataWriter {
    Jsonl(Box<JsonlWriter>),
    Parquet(Box<ParquetWriter>),
}

impl DataWriter {
    /// Parquet files typed by `parquet`, or JSONL files when it is `None`.
    pub fn new(
        store: Arc<Store>,
        layout: &Layout,
        stream: &str,
        dir: String,
        parquet: Option<Arc<Schema>>,
        max_bytes: u64,
    ) -> Self {
        match parquet {
            Some(schema) => DataWriter::Parquet(Box::new(ParquetWriter::new(
                store, layout, stream, dir, schema, max_bytes,
            ))),
            None => DataWriter::Jsonl(Box::new(JsonlWriter::new(
                store, layout, stream, dir, max_bytes,
            ))),
        }
    }

    /// Writes one record with its stamp: JSONL keeps the record as received,
    /// Parquet writes the conformed row, with the time in microseconds.
    pub async fn write(
        &mut self,
        record: Value,
        conformed: Option<Conformed>,
        stamp: &Stamp<'_>,
    ) -> Result<()> {
        match self {
            DataWriter::Jsonl(w) => {
                let mut v = wrap(record);
                let obj = v.as_object_mut().expect("wrapped");
                obj.insert(RUN_ID.into(), Value::String(stamp.run_id.to_string()));
                obj.insert(EXTRACTED_AT.into(), Value::String(stamp.extracted_text()));
                w.write(&v).await
            }
            DataWriter::Parquet(w) => {
                let mut row = conformed.expect("parquet has a schema").row;
                row.insert(RUN_ID.into(), Value::String(stamp.run_id.to_string()));
                row.insert(
                    EXTRACTED_AT.into(),
                    json!(stamp.extracted.timestamp_micros()),
                );
                w.write(row).await
            }
        }
    }

    pub async fn finish(self) -> Result<Vec<StagedFile>> {
        match self {
            DataWriter::Jsonl(w) => w.finish().await,
            DataWriter::Parquet(w) => w.finish().await,
        }
    }
}

/// Why a record is quarantined, one violation after the other.
pub fn reason(violations: &[Violation]) -> String {
    violations
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

/// The quarantine line of a record: the record whole, with the reason and
/// the stamp `replay` needs to put it back in its run.
pub fn quarantine_line(record: Value, violations: &[Violation], stamp: &Stamp<'_>) -> Value {
    json!({
        "reason": reason(violations),
        RUN_ID: stamp.run_id,
        EXTRACTED_AT: stamp.extracted_text(),
        "record": record,
    })
}
