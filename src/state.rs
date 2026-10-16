//! Run state in the destination: watermarks in `_apisip/state.json`, one
//! manifest per run in `_apisip/runs/<run_id>.json`.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{Config, Sort};
use crate::schema::Drift;
use crate::sink::{META_DIR, Store};
use crate::workspace::Workspace;

pub const STATE_FILE: &str = "_apisip/state.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
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

    pub async fn save(&self, store: &Store) -> Result<()> {
        store
            .put(STATE_FILE, serde_json::to_vec_pretty(self)?)
            .await
    }
}

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
