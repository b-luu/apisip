//! Watermarks and run state. Filled in with the staged commit.

use std::collections::BTreeSet;

use anyhow::Result;

use crate::config::Config;
use crate::workspace::Workspace;

/// Streams whose descending sort was found unreliable by an earlier run.
pub async fn unreliable_streams(_ws: &Workspace, _config: &Config) -> Result<BTreeSet<String>> {
    Ok(BTreeSet::new())
}
