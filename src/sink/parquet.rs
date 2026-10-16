//! Parquet parts typed from the record schema: row groups of about 10k
//! rows, zstd, rotated at `max_bytes`.

use std::sync::Arc;

use anyhow::{Context, Result};
use arrow::datatypes::Schema;
use object_store::buffered::BufWriter;
use object_store::path::Path;
use parquet::arrow::AsyncArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use serde_json::{Map, Value};

use super::{Layout, StagedFile, Store};

pub const ROW_GROUP: usize = 10_000;

pub struct ParquetWriter {
    store: Arc<Store>,
    layout: Layout,
    stream: String,
    dir: String,
    schema: Arc<Schema>,
    max_bytes: u64,
    buffer: Vec<Map<String, Value>>,
    seq: u32,
    current: Option<(AsyncArrowWriter<BufWriter>, StagedFile)>,
    files: Vec<StagedFile>,
}

impl ParquetWriter {
    pub fn new(
        store: Arc<Store>,
        layout: &Layout,
        stream: &str,
        dir: String,
        schema: Arc<Schema>,
        max_bytes: u64,
    ) -> Self {
        ParquetWriter {
            store,
            layout: layout.clone(),
            stream: stream.to_string(),
            dir,
            schema,
            max_bytes: max_bytes.max(1),
            buffer: Vec::new(),
            seq: 0,
            current: None,
            files: Vec::new(),
        }
    }

    /// Adds one row produced by `RecordSchema::conform` (plus apisip's columns).
    pub async fn write(&mut self, row: Map<String, Value>) -> Result<()> {
        self.buffer.push(row);
        if self.buffer.len() >= ROW_GROUP {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.buffer);
        let mut decoder = arrow::json::ReaderBuilder::new(self.schema.clone())
            .with_batch_size(rows.len())
            .build_decoder()?;
        decoder
            .serialize(&rows)
            .with_context(|| format!("converting {} records to Arrow", self.stream))?;
        let Some(batch) = decoder.flush()? else {
            return Ok(());
        };
        if self.current.is_none() {
            let target = format!(
                "{}/{}",
                self.dir,
                self.layout.part_name(&self.stream, self.seq, "parquet")
            );
            self.seq += 1;
            let staged = self.layout.staged(&target);
            let props = WriterProperties::builder()
                .set_compression(Compression::ZSTD(ZstdLevel::default()))
                .set_max_row_group_row_count(Some(ROW_GROUP))
                .build();
            let out = BufWriter::new(self.store.inner.clone(), Path::from(staged.as_str()));
            let w = AsyncArrowWriter::try_new(out, self.schema.clone(), Some(props))?;
            self.current = Some((
                w,
                StagedFile {
                    staged,
                    target,
                    rows: 0,
                    bytes: 0,
                },
            ));
        }
        let (w, file) = self.current.as_mut().expect("opened above");
        w.write(&batch).await?;
        file.rows += batch.num_rows() as u64;
        if (w.bytes_written() + w.in_progress_size()) as u64 >= self.max_bytes {
            self.close_part().await?;
        }
        Ok(())
    }

    async fn close_part(&mut self) -> Result<()> {
        if let Some((w, mut file)) = self.current.take() {
            let written = w.bytes_written();
            w.close().await?;
            let size = self
                .store
                .list(&file.staged)
                .await?
                .first()
                .map(|m| m.size)
                .unwrap_or(written as u64);
            file.bytes = size;
            self.files.push(file);
        }
        Ok(())
    }

    pub async fn finish(mut self) -> Result<Vec<StagedFile>> {
        self.flush().await?;
        self.close_part().await?;
        Ok(self.files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Destination;
    use crate::schema::RecordSchema;
    use arrow::array::{Array, AsArray};
    use arrow::datatypes::{Int64Type, TimestampMicrosecondType};
    use bytes::Bytes;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use serde_json::json;

    #[tokio::test]
    async fn writes_typed_columns() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            Store::open(
                &Destination {
                    url: tmp.path().display().to_string(),
                    format: crate::config::Format::Parquet,
                    rotate: Default::default(),
                    endpoint: None,
                    region: None,
                },
                tmp.path(),
            )
            .unwrap(),
        );
        let schema = RecordSchema::from_spec(&json!({"properties": {
            "id": {"type": "integer"},
            "updated_at": {"type": "string", "format": "date-time"},
            "tags": {"type": "array", "items": {"type": "string"}},
            "address": {"properties": {"city": {"type": "string"}}}
        }}))
        .unwrap();
        let layout = Layout::new(chrono::Utc::now());
        let mut w = ParquetWriter::new(
            store.clone(),
            &layout,
            "users",
            "users/dt=x".into(),
            schema.arrow(),
            1 << 30,
        );
        for i in 0..25_001i64 {
            let mut c = schema.conform(&json!({
                "id": i, "updated_at": "2026-01-01T00:00:00Z", "tags": ["a"],
                "address": {"city": "Lyon"}, "x": 1
            }));
            c.row.insert("_apisip_run_id".into(), json!(layout.run_id));
            w.write(c.row).await.unwrap();
        }
        let files = w.finish().await.unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].rows, 25_001);
        let bytes: Bytes = store.get(&files[0].staged).await.unwrap().unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(bytes).unwrap();
        assert_eq!(reader.metadata().num_row_groups(), 3);
        let batch = reader.build().unwrap().next().unwrap().unwrap();
        assert_eq!(
            batch
                .column_by_name("id")
                .unwrap()
                .as_primitive::<Int64Type>()
                .value(7),
            7
        );
        let ts = batch
            .column_by_name("updated_at")
            .unwrap()
            .as_primitive::<TimestampMicrosecondType>();
        assert_eq!(ts.value(0), 1_767_225_600_000_000);
        let extra = batch
            .column_by_name("_apisip_extra")
            .unwrap()
            .as_string::<i32>();
        assert_eq!(extra.value(0), "{\"x\":1}");
        assert!(
            batch
                .column_by_name("_apisip_extracted_at")
                .unwrap()
                .is_null(0)
        );
    }
}
