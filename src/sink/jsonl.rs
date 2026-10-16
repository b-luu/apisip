//! JSONL parts: one record per line, rotated at `max_bytes`.

use std::sync::Arc;

use anyhow::Result;
use object_store::buffered::BufWriter;
use object_store::path::Path;
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use super::{Layout, StagedFile, Store};

pub struct JsonlWriter {
    store: Arc<Store>,
    layout: Layout,
    stream: String,
    dir: String,
    max_bytes: u64,
    /// Fixed file name (no rotation), for quarantine files.
    fixed: Option<String>,
    seq: u32,
    current: Option<Part>,
    files: Vec<StagedFile>,
}

struct Part {
    writer: BufWriter,
    file: StagedFile,
}

impl JsonlWriter {
    pub fn new(
        store: Arc<Store>,
        layout: &Layout,
        stream: &str,
        dir: String,
        max_bytes: u64,
    ) -> Self {
        JsonlWriter {
            store,
            layout: layout.clone(),
            stream: stream.to_string(),
            dir,
            max_bytes: max_bytes.max(1),
            fixed: None,
            seq: 0,
            current: None,
            files: Vec::new(),
        }
    }

    /// One file at `target`, never rotated.
    pub fn single(store: Arc<Store>, layout: &Layout, target: String) -> Self {
        let mut w = JsonlWriter::new(store, layout, "", String::new(), u64::MAX);
        w.fixed = Some(target);
        w
    }

    pub async fn write(&mut self, record: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        if self
            .current
            .as_ref()
            .is_some_and(|p| p.file.bytes >= self.max_bytes)
        {
            self.close_part().await?;
        }
        if self.current.is_none() {
            let target = match &self.fixed {
                Some(t) => t.clone(),
                None => format!(
                    "{}/{}",
                    self.dir,
                    self.layout.part_name(&self.stream, self.seq, "jsonl")
                ),
            };
            self.seq += 1;
            let staged = self.layout.staged(&target);
            self.current = Some(Part {
                writer: BufWriter::new(self.store.inner.clone(), Path::from(staged.as_str())),
                file: StagedFile {
                    staged,
                    target,
                    rows: 0,
                    bytes: 0,
                },
            });
        }
        let part = self.current.as_mut().expect("opened above");
        part.writer.write_all(&line).await?;
        part.file.rows += 1;
        part.file.bytes += line.len() as u64;
        Ok(())
    }

    async fn close_part(&mut self) -> Result<()> {
        if let Some(mut p) = self.current.take() {
            p.writer.shutdown().await?;
            self.files.push(p.file);
        }
        Ok(())
    }

    /// Closes the open part. Returns the staged files.
    pub async fn finish(mut self) -> Result<Vec<StagedFile>> {
        self.close_part().await?;
        Ok(self.files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Destination;
    use serde_json::json;

    #[tokio::test]
    async fn rotates_at_max_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            Store::open(
                &Destination {
                    url: tmp.path().display().to_string(),
                    format: Default::default(),
                    rotate: Default::default(),
                    endpoint: None,
                    region: None,
                },
                tmp.path(),
            )
            .unwrap(),
        );
        let layout = Layout::new(chrono::Utc::now());
        let mut w = JsonlWriter::new(store.clone(), &layout, "s", "s/dt=x".into(), 20);
        for i in 0..5 {
            w.write(&json!({"id": i, "pad": "xxxx"})).await.unwrap();
        }
        let files = w.finish().await.unwrap();
        assert_eq!(
            files.iter().map(|f| f.rows).collect::<Vec<_>>(),
            vec![1, 1, 1, 1, 1]
        );
        let mut w = JsonlWriter::new(store.clone(), &layout, "t", "t/dt=x".into(), 1 << 20);
        for i in 0..5 {
            w.write(&json!({"id": i})).await.unwrap();
        }
        let files = w.finish().await.unwrap();
        assert_eq!(files.len(), 1);
        let text = store.get(&files[0].staged).await.unwrap().unwrap();
        assert_eq!(text.iter().filter(|b| **b == b'\n').count(), 5);
        assert!(files[0].staged.starts_with("_apisip/staging/"));
    }
}
