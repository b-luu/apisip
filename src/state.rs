//! Run state in the destination: watermarks in `_apisip/state.json`, one
//! manifest per run in `_apisip/runs/<run_id>.json`.

use std::collections::{BTreeMap, BTreeSet};

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use object_store::path::Path;
use object_store::{GetOptions, ObjectStore, PutMode, PutPayload, UpdateVersion};
use serde::{Deserialize, Serialize};

use crate::config::{Config, Sort};
use crate::schema::Drift;
use crate::sink::{META_DIR, Store};
use crate::workspace::Workspace;

pub const STATE_FILE: &str = "_apisip/state.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    /// The `.apisip/folder-id` of the folder that writes this destination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default)]
    pub streams: BTreeMap<String, StreamState>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamState {
    /// Highest cursor value committed so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark: Option<String>,
    /// The cursor field the watermark belongs to; a different field resets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_committed_at: Option<DateTime<Utc>>,
    /// Cursor range of the last committed run, to flag overlaps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_range: Option<CursorRange>,
    /// Fingerprints of the records written at the watermark: the next run
    /// asks from the watermark inclusive and skips them when unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boundary: Vec<String>,
    /// Set when a run saw the cursor go up under a descending sort. Holds the
    /// sort it was seen with: changing the sort in apisip.toml clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unreliable_sort: Option<String>,
}

impl StreamState {
    /// The watermark, if it belongs to this cursor field.
    pub fn watermark_for(&self, cursor: &str) -> Option<&str> {
        match &self.cursor {
            Some(c) if c == cursor => self.watermark.as_deref(),
            _ => None,
        }
    }

    pub fn sort_is_unreliable(&self, sort: Option<&Sort>) -> bool {
        match (sort, &self.unreliable_sort) {
            (Some(s), Some(seen)) => sort_signature(s) == *seen,
            _ => false,
        }
    }
}

/// Why a folder may not write a destination another folder owns.
pub fn foreign_owner(url: &str, owner: &str, this: Option<&str>) -> String {
    format!(
        "the destination {url} belongs to another apisip folder (folder id {owner}; this \
         folder's is {}). Two folders writing one destination would mix their data. Use \
         another destination, or, if this folder is a copy of that one, copy its \
         .apisip/folder-id here.",
        this.unwrap_or("not created yet")
    )
}

pub fn sort_signature(s: &Sort) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CursorRange {
    pub min: String,
    pub max: String,
}

impl State {
    pub async fn load(store: &Store) -> Result<State> {
        match store.get(STATE_FILE).await? {
            None => Ok(State::default()),
            Some(b) => serde_json::from_slice(&b)
                .with_context(|| format!("{STATE_FILE} in {} is not valid", store.url)),
        }
    }

    /// Reads the state, applies `change` and writes it back without losing a
    /// concurrent run's update. On an object store the write is conditional
    /// on the version read, and a conflict re-reads and re-applies `change`.
    /// A local destination has no conditional writes and relies on
    /// `Store::commit_lock`, which every caller holds. Returns the state as written.
    pub async fn update(store: &Store, mut change: impl FnMut(&mut State)) -> Result<State> {
        let path = Path::from(STATE_FILE);
        for attempt in 0..MAX_STATE_TRIES {
            let (mut state, version) =
                match store.inner.get_opts(&path, GetOptions::default()).await {
                    Ok(r) => {
                        let version = UpdateVersion {
                            e_tag: r.meta.e_tag.clone(),
                            version: r.meta.version.clone(),
                        };
                        let state: State =
                            serde_json::from_slice(&r.bytes().await?).with_context(|| {
                                format!("{STATE_FILE} in {} is not valid", store.url)
                            })?;
                        (state, Some(version))
                    }
                    Err(object_store::Error::NotFound { .. }) => (State::default(), None),
                    Err(e) => {
                        return Err(e)
                            .with_context(|| format!("reading {STATE_FILE} from {}", store.url));
                    }
                };
            change(&mut state);
            let bytes = serde_json::to_vec_pretty(&state)?;
            let mode = match (&store.local_root, version) {
                (Some(_), _) => PutMode::Overwrite,
                (None, Some(v)) => PutMode::Update(v),
                (None, None) => PutMode::Create,
            };
            let conditional = mode != PutMode::Overwrite;
            match store
                .inner
                .put_opts(&path, PutPayload::from(bytes.clone()), mode.into())
                .await
            {
                Ok(_) => return Ok(state),
                Err(
                    object_store::Error::Precondition { .. }
                    | object_store::Error::AlreadyExists { .. },
                ) => {
                    // Another run committed in between: back off, then retry.
                    let ms = rand::random_range(10..50u64) << attempt.min(5);
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                }
                Err(e) if conditional && rejects_conditional_writes(&e) => {
                    // Some S3-compatible stores reject conditional writes.
                    store.put(STATE_FILE, bytes).await.map_err(|plain| {
                        plain.context(format!("conditional write failed first: {e}"))
                    })?;
                    if !NO_CONDITIONAL_WARNED.swap(true, Ordering::Relaxed) {
                        eprintln!(
                            "warning: {} does not support conditional writes. A run of a copy of \
                             this folder at the same time can move a watermark back: the next run \
                             then fetches some records again (duplicates, no loss)",
                            store.url
                        );
                    }
                    return Ok(state);
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("writing {STATE_FILE} to {}", store.url));
                }
            }
        }
        bail!(
            "{STATE_FILE} in {} kept changing under concurrent runs; try again",
            store.url
        )
    }
}

const MAX_STATE_TRIES: u32 = 10;

fn is_zero(n: &u64) -> bool {
    *n == 0
}
static NO_CONDITIONAL_WARNED: AtomicBool = AtomicBool::new(false);

/// What one run did, written to `_apisip/runs/<run_id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub run_id: String,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    pub status: RunStatus,
    pub full_refresh: bool,
    pub format: String,
    pub streams: BTreeMap<String, StreamReport>,
    #[serde(default, skip_serializing_if = "RunKind::is_fetch")]
    pub kind: RunKind,
}

/// A run fetches from the API; a replay re-reads quarantined records.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    #[default]
    Fetch,
    Replay,
}

impl RunKind {
    fn is_fetch(&self) -> bool {
        *self == RunKind::Fetch
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Success,
    Partial,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamReport {
    pub status: StreamStatus,
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub records: u64,
    pub quarantined: u64,
    pub pages: u64,
    #[serde(default)]
    pub files: Vec<String>,
    /// `from` is the watermark the run started from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<CursorRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlap: Option<Overlap>,
    /// Records at the watermark skipped because an earlier run wrote them unchanged.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unchanged: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unreliable_sort: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<crate::schema::SchemaSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drift: Option<Drift>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamStatus {
    Committed,
    Failed,
}

/// This run fetched records older than the previous run's newest one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Overlap {
    pub previous_run_id: Option<String>,
    pub previous_max: String,
}

impl Manifest {
    pub fn path(run_id: &str) -> String {
        format!("{META_DIR}/runs/{run_id}.json")
    }

    pub async fn save(&self, store: &Store) -> Result<()> {
        store
            .put(
                &Manifest::path(&self.run_id),
                serde_json::to_vec_pretty(self)?,
            )
            .await
    }
}

/// An S3-compatible store without conditional writes answers 501 Not
/// Implemented (or a similar refusal). Any other error, such as a timeout or
/// a denied access, is a failure, not a reason to write without the condition.
fn rejects_conditional_writes(e: &object_store::Error) -> bool {
    match e {
        object_store::Error::NotImplemented { .. } | object_store::Error::NotSupported { .. } => {
            true
        }
        e => {
            let text = format!("{e:?}").to_lowercase();
            ["notimplemented", "not implemented", "not supported"]
                .iter()
                .any(|t| text.contains(t))
        }
    }
}

/// Streams whose descending sort was found unreliable by an earlier run.
pub async fn unreliable_streams(ws: &Workspace, config: &Config) -> Result<BTreeSet<String>> {
    if let Some(dir) = config
        .destination
        .url
        .strip_prefix("file://")
        .or(Some(config.destination.url.as_str()))
        .filter(|u| !u.contains("://"))
    {
        let dir = if std::path::Path::new(dir).is_absolute() {
            std::path::PathBuf::from(dir)
        } else {
            ws.dir.join(dir)
        };
        if !dir.join(STATE_FILE).exists() {
            return Ok(BTreeSet::new());
        }
    }
    let store = Store::open(&config.destination, &ws.dir)?;
    let state = State::load(&store).await?;
    Ok(config
        .streams
        .iter()
        .filter(|s| {
            state.streams.get(&s.name).is_some_and(|st| {
                st.sort_is_unreliable(s.incremental.as_ref().and_then(|i| i.sort.as_ref()))
            })
        })
        .map(|s| s.name.clone())
        .collect())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    async fn commit_streams(store: &Store, lock: bool) -> State {
        let commits = (0..5).map(|i| async move {
            let _guard = if lock {
                Some(store.commit_lock().await.unwrap())
            } else {
                None
            };
            State::update(store, |s| {
                s.streams.entry(format!("s{i}")).or_default().watermark = Some(i.to_string());
            })
            .await
            .unwrap();
        });
        futures::future::join_all(commits).await;
        State::load(store).await.unwrap()
    }

    #[tokio::test]
    async fn a_commit_racing_another_on_an_object_store_keeps_both() {
        let store = Store {
            inner: Arc::new(object_store::memory::InMemory::new()),
            local_root: None,
            url: "memory://".into(),
            commits: Default::default(),
        };
        State::update(&store, |_| {}).await.unwrap();
        let mut raced = false;
        let state = State::update(&store, |s| {
            if !raced {
                // Another run commits between this read and this write.
                raced = true;
                futures::executor::block_on(State::update(&store, |o| {
                    o.streams.entry("other".into()).or_default().watermark = Some("1".into());
                }))
                .unwrap();
            }
            s.streams.entry("mine".into()).or_default().watermark = Some("2".into());
        })
        .await
        .unwrap();
        assert!(raced);
        let stored = State::load(&store).await.unwrap();
        for s in [&state, &stored] {
            assert_eq!(
                s.streams.keys().collect::<Vec<_>>(),
                ["mine", "other"],
                "{s:?}"
            );
        }
    }

    #[tokio::test]
    async fn concurrent_commits_on_a_local_folder_take_turns() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            &crate::config::Destination {
                url: "file://out".into(),
                format: Default::default(),
                rotate: Default::default(),
                endpoint: None,
                region: None,
            },
            dir.path(),
        )
        .unwrap();
        let state = commit_streams(&store, true).await;
        assert_eq!(state.streams.len(), 5, "{state:?}");
    }

    /// An S3-compatible store that refuses conditional writes, and yields on
    /// each read and write so that concurrent commits interleave.
    #[derive(Debug)]
    struct NoConditionalWrites(object_store::memory::InMemory);

    impl std::fmt::Display for NoConditionalWrites {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("NoConditionalWrites")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for NoConditionalWrites {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            opts: object_store::PutOptions,
        ) -> object_store::Result<object_store::PutResult> {
            tokio::task::yield_now().await;
            if opts.mode != PutMode::Overwrite {
                return Err(object_store::Error::NotImplemented {
                    operation: "conditional put".into(),
                    implementer: self.to_string(),
                });
            }
            self.0.put_opts(location, payload, opts).await
        }
        async fn put_multipart_opts(
            &self,
            location: &Path,
            opts: object_store::PutMultipartOptions,
        ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
            self.0.put_multipart_opts(location, opts).await
        }
        async fn get_opts(
            &self,
            location: &Path,
            options: GetOptions,
        ) -> object_store::Result<object_store::GetResult> {
            tokio::task::yield_now().await;
            self.0.get_opts(location, options).await
        }
        fn delete_stream(
            &self,
            locations: futures::stream::BoxStream<'static, object_store::Result<Path>>,
        ) -> futures::stream::BoxStream<'static, object_store::Result<Path>> {
            self.0.delete_stream(locations)
        }
        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
        {
            self.0.list(prefix)
        }
        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> object_store::Result<object_store::ListResult> {
            self.0.list_with_delimiter(prefix).await
        }
        async fn copy_opts(
            &self,
            from: &Path,
            to: &Path,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.0.copy_opts(from, to, options).await
        }
    }

    #[tokio::test]
    async fn streams_of_one_run_keep_their_state_without_conditional_writes() {
        let store = Store {
            inner: Arc::new(NoConditionalWrites(object_store::memory::InMemory::new())),
            local_root: None,
            url: "memory://".into(),
            commits: Default::default(),
        };
        let state = commit_streams(&store, true).await;
        assert_eq!(state.streams.len(), 5, "{state:?}");
    }

    #[test]
    fn a_failed_conditional_write_is_not_retried_without_the_condition() {
        let e = object_store::Error::Generic {
            store: "S3",
            source: "request timed out".into(),
        };
        assert!(!rejects_conditional_writes(&e));
        let e = object_store::Error::Generic {
            store: "S3",
            source: "Received 501 Not Implemented: NotImplemented".into(),
        };
        assert!(rejects_conditional_writes(&e));
    }
}
