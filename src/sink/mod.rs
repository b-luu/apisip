//! Destinations: a local folder or any S3-compatible bucket, through
//! `object_store`. Writers stage files under `_apisip/staging/<run_id>/`;
//! the commit then moves them into the data layout.

pub mod jsonl;
pub mod parquet;

use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use crate::config::Destination;

pub use jsonl::JsonlWriter;
pub use parquet::ParquetWriter;

#[derive(Debug)]
pub struct Store {
    pub inner: Arc<dyn ObjectStore>,
    /// The local root for `file://` destinations, where commits can merge and rename.
    pub local_root: Option<PathBuf>,
    /// The destination as configured, for messages.
    pub url: String,
}

impl Store {
    pub fn open(dest: &Destination, base_dir: &FsPath) -> Result<Store> {
        let url = crate::config::interpolate(&dest.url).context("destination.url")?;
        if let Some(rest) = url.strip_prefix("s3://") {
            let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
            if bucket.is_empty() {
                bail!("destination.url {url:?} has no bucket");
            }
            let mut b = object_store::aws::AmazonS3Builder::from_env().with_bucket_name(bucket);
            if let Some(e) = &dest.endpoint {
                let e = crate::config::interpolate(e).context("destination.endpoint")?;
                b = b.with_allow_http(e.starts_with("http://")).with_endpoint(e);
            } else if let Ok(e) = std::env::var("AWS_ENDPOINT_URL")
                && e.starts_with("http://")
            {
                b = b.with_allow_http(true);
            }
            if let Some(r) = &dest.region {
                b = b.with_region(crate::config::interpolate(r)?);
            }
            let s3 = b
                .build()
                .context("configuring the S3 destination (see AWS_* in .env)")?;
            let inner: Arc<dyn ObjectStore> = match prefix.trim_matches('/') {
                "" => Arc::new(s3),
                p => Arc::new(object_store::prefix::PrefixStore::new(s3, p)),
            };
            return Ok(Store {
                inner,
                local_root: None,
                url,
            });
        }
        if url.contains("://") && !url.starts_with("file://") {
            bail!("destination.url {url:?}: use a folder, file://<folder> or s3://bucket/prefix");
        }
        let dir = url.strip_prefix("file://").unwrap_or(&url);
        let dir = if FsPath::new(dir).is_absolute() {
            PathBuf::from(dir)
        } else {
            base_dir.join(dir)
        };
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let dir = std::fs::canonicalize(&dir)?;
        let fs = object_store::local::LocalFileSystem::new_with_prefix(&dir)?;
        Ok(Store {
            inner: Arc::new(fs),
            local_root: Some(dir),
            url,
        })
    }

    pub async fn put(&self, path: &str, bytes: impl Into<Bytes>) -> Result<()> {
        self.inner
            .put(&Path::from(path), PutPayload::from(bytes.into()))
            .await
            .with_context(|| format!("writing {path} to {}", self.url))?;
        Ok(())
    }

    pub async fn get(&self, path: &str) -> Result<Option<Bytes>> {
        match self.inner.get(&Path::from(path)).await {
            Ok(r) => Ok(Some(r.bytes().await?)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {path} from {}", self.url)),
        }
    }

    pub async fn list(&self, prefix: &str) -> Result<Vec<object_store::ObjectMeta>> {
        let prefix = Path::from(prefix);
        Ok(self.inner.list(Some(&prefix)).try_collect().await?)
    }

    pub async fn delete_prefix(&self, prefix: &str) -> Result<()> {
        for meta in self.list(prefix).await? {
            self.inner.delete(&meta.location).await?;
        }
        if let Some(root) = &self.local_root {
            let dir = root.join(prefix);
            if dir.is_dir() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
        Ok(())
    }

    /// Moves a staged file into place. On a local destination, a JSONL part
    /// is appended to the latest part of the same folder while that stays
    /// under `max_bytes`: the merged file is written aside, then renamed
    /// over the old one, so readers never see a half-written file.
    /// Returns the final path.
    pub async fn promote(&self, f: &StagedFile, merge_into: Option<(&str, u64)>) -> Result<String> {
        if let (Some(root), Some((name_prefix, max_bytes))) = (&self.local_root, merge_into)
            && let Some(existing) = self.latest_part(&f.target, name_prefix).await?
            && existing.1 + f.bytes <= max_bytes
        {
            let staged = root.join(&f.staged);
            let target = root.join(&existing.0);
            let tmp = target.with_extension("jsonl.apisip-tmp");
            let mut merged = std::fs::read(&target)?;
            merged.extend(std::fs::read(&staged)?);
            std::fs::write(&tmp, merged)?;
            std::fs::rename(&tmp, &target)
                .with_context(|| format!("replacing {}", target.display()))?;
            std::fs::remove_file(&staged)?;
            return Ok(existing.0);
        }
        self.inner
            .rename(
                &Path::from(f.staged.as_str()),
                &Path::from(f.target.as_str()),
            )
            .await
            .with_context(|| format!("moving {} into {}", f.staged, f.target))?;
        Ok(f.target.clone())
    }

    /// The last part (by name) next to `target` whose name starts with `prefix`.
    async fn latest_part(&self, target: &str, prefix: &str) -> Result<Option<(String, u64)>> {
        let dir = target.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let listing = self
            .inner
            .list_with_delimiter(Some(&Path::from(dir)))
            .await?;
        Ok(listing
            .objects
            .into_iter()
            .filter(|m| {
                let name = m.location.filename().unwrap_or("");
                name.starts_with(prefix) && name.ends_with(".jsonl")
            })
            .max_by(|a, b| a.location.as_ref().cmp(b.location.as_ref()))
            .map(|m| (m.location.to_string(), m.size)))
    }
}

/// A file written under staging, waiting for the commit.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StagedFile {
    pub staged: String,
    pub target: String,
    pub rows: u64,
    pub bytes: u64,
}

/// File naming for one run.
#[derive(Debug, Clone)]
pub struct Layout {
    pub run_id: String,
    pub started: DateTime<Utc>,
}

pub const META_DIR: &str = "_apisip";

impl Layout {
    pub fn new(started: DateTime<Utc>) -> Layout {
        let suffix: u16 = rand::random();
        Layout {
            run_id: format!("{}-{suffix:04x}", started.format("%Y%m%dT%H%M%S%3fZ")),
            started,
        }
    }

    /// `<stream>/dt=<date>/`, plus `run_id=<id>/` for full-refresh snapshots.
    pub fn data_dir(&self, stream: &str, snapshot: bool) -> String {
        let mut d = format!("{stream}/dt={}", self.started.format("%Y-%m-%d"));
        if snapshot {
            d.push_str(&format!("/run_id={}", self.run_id));
        }
        d
    }

    pub fn part_prefix(stream: &str) -> String {
        format!("{stream}-")
    }

    pub fn part_name(&self, stream: &str, seq: u32, ext: &str) -> String {
        format!("{stream}-{}-{seq:04}.{ext}", self.run_id)
    }

    pub fn quarantine(&self, stream: &str) -> String {
        format!("{stream}/_quarantine/{}.jsonl", self.run_id)
    }

    pub fn staging_root(&self) -> String {
        format!("{META_DIR}/staging/{}", self.run_id)
    }

    pub fn staged(&self, target: &str) -> String {
        format!("{}/{target}", self.staging_root())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(dir: &FsPath) -> Store {
        Store::open(
            &Destination {
                url: "file://out".into(),
                format: Default::default(),
                rotate: Default::default(),
                endpoint: None,
                region: None,
            },
            dir,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn promote_merges_into_the_latest_local_part_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let store = local(tmp.path());
        store.put("s/dt=1/s-a-0000.jsonl", "1\n").await.unwrap();
        store.put("s/dt=1/s-b-0000.jsonl", "2\n").await.unwrap();
        store
            .put("_apisip/staging/r/s/dt=1/s-r-0000.jsonl", "3\n")
            .await
            .unwrap();
        let f = StagedFile {
            staged: "_apisip/staging/r/s/dt=1/s-r-0000.jsonl".into(),
            target: "s/dt=1/s-r-0000.jsonl".into(),
            rows: 1,
            bytes: 2,
        };
        let out = store.promote(&f, Some(("s-", 100))).await.unwrap();
        assert_eq!(out, "s/dt=1/s-b-0000.jsonl");
        assert_eq!(store.get(&out).await.unwrap().unwrap(), "2\n3\n");
        assert!(store.get(&f.staged).await.unwrap().is_none());
        let names: Vec<_> = std::fs::read_dir(tmp.path().join("out/s/dt=1"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 2, "no temp file left behind: {names:?}");
    }

    #[tokio::test]
    async fn promote_starts_a_new_part_when_the_latest_is_full() {
        let tmp = tempfile::tempdir().unwrap();
        let store = local(tmp.path());
        store.put("s/dt=1/s-a-0000.jsonl", "1234\n").await.unwrap();
        store
            .put("_apisip/staging/r/s/dt=1/s-r-0000.jsonl", "3\n")
            .await
            .unwrap();
        let f = StagedFile {
            staged: "_apisip/staging/r/s/dt=1/s-r-0000.jsonl".into(),
            target: "s/dt=1/s-r-0000.jsonl".into(),
            rows: 1,
            bytes: 2,
        };
        let out = store.promote(&f, Some(("s-", 6))).await.unwrap();
        assert_eq!(out, f.target);
        assert_eq!(
            store.get("s/dt=1/s-a-0000.jsonl").await.unwrap().unwrap(),
            "1234\n"
        );
    }

    #[test]
    fn layout_paths() {
        let l = Layout {
            run_id: "20261016T120000Z-00ab".into(),
            started: "2026-10-16T12:00:00Z".parse().unwrap(),
        };
        assert_eq!(l.data_dir("users", false), "users/dt=2026-10-16");
        assert_eq!(
            l.data_dir("users", true),
            "users/dt=2026-10-16/run_id=20261016T120000Z-00ab"
        );
        assert_eq!(
            l.part_name("users", 3, "jsonl"),
            "users-20261016T120000Z-00ab-0003.jsonl"
        );
        assert_eq!(
            l.quarantine("users"),
            "users/_quarantine/20261016T120000Z-00ab.jsonl"
        );
    }
}
