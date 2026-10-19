//! What update prints before it asks anything: the config and schema
//! changes, the auth schemes, and the drift the last runs saw.

use std::collections::BTreeSet;

use serde_json::Value;

use super::auth::auth_names;
use super::config_merge::{Diff, Kind, changes_views};
use super::schema_merge::{self, StreamSchema};
use crate::config::Config;
use crate::detect::{self, AuthOption};
use crate::schema::RecordSchema;
use crate::sink::Store;
use crate::spec::Spec;
use crate::state::Manifest;

pub(super) fn print_diff(
    d: &Diff,
    schemas: &[StreamSchema],
    auth: Option<(&Option<Vec<AuthOption>>, &Vec<AuthOption>)>,
) {
    if !d.added.is_empty() {
        println!("New endpoints: {}", d.added.join(", "));
    }
    if !d.removed.is_empty() {
        println!(
            "Removed from the spec (will be disabled): {}",
            d.removed.join(", ")
        );
    }
    for c in &d.fields {
        let what = match &c.stream {
            Some(s) => format!("streams.{s}.{}", c.key),
            None => format!("source.{}", c.key),
        };
        let note = match c.kind {
            Kind::Update if changes_views(c) => "spec changed; the views deduplicate on it: asks",
            Kind::Update => "spec changed, not edited here: updating",
            Kind::Conflict => "edited here and changed in the spec: conflict",
            Kind::AlreadyApplied => "spec now matches your edit",
        };
        println!(
            "{what}: {} -> {} ({note})",
            short(&c.base),
            short(&c.theirs)
        );
        if c.kind == Kind::Conflict {
            println!("  yours: {}", short(&c.mine));
        }
    }
    for s in schemas {
        if s.theirs.is_some() && s.mine.is_none() {
            println!(
                "schemas/{}.json is missing: it will be written from the spec",
                s.name
            );
        }
        if s.changes.is_empty() && s.undeclared.is_empty() {
            continue;
        }
        println!("Schema of {} (schemas/{}.json):", s.name, s.name);
        for c in &s.changes {
            let note = match c.kind {
                Kind::AlreadyApplied => " (spec now matches your edit)",
                Kind::Conflict => " (edited here and changed in the spec: conflict)",
                Kind::Update if c.compatible => "",
                Kind::Update => " (does not match the files already written: asks)",
            };
            println!("  {}{note}", c.describe());
        }
        for (f, shape) in &s.undeclared {
            println!(
                "  ? {f} ({}): undeclared, seen in each recent run",
                shape.label()
            );
        }
    }
    if let Some((Some(old), new)) = auth {
        let (o, n) = (auth_names(old), auth_names(new));
        for s in n.difference(&o) {
            println!("Auth scheme added: {s}");
        }
        for s in o.difference(&n) {
            println!("Auth scheme removed: {s}");
        }
    }
}

/// The drift the last run saw, and which of it the new spec now declares.
pub(super) async fn print_drift(store: &Store, config: &Config, new_spec: &Spec) {
    let Ok(state) = crate::state::State::load(store).await else {
        return;
    };
    for s in &config.streams {
        let Some(run_id) = state
            .streams
            .get(&s.name)
            .and_then(|st| st.last_run_id.clone())
        else {
            continue;
        };
        let Ok(Some(bytes)) = store.get(&Manifest::path(&run_id)).await else {
            continue;
        };
        let Ok(m) = serde_json::from_slice::<Manifest>(&bytes) else {
            continue;
        };
        let Some(drift) = m.streams.get(&s.name).and_then(|r| r.drift.clone()) else {
            continue;
        };
        let declared: BTreeSet<String> = detect::stream_record_schema(new_spec, s)
            .and_then(|i| RecordSchema::from_spec(&i))
            .map(|r| r.fields.into_iter().map(|(k, _)| k).collect())
            .unwrap_or_default();
        println!("Drift seen by run {run_id} on {}:", s.name);
        for line in drift.summary() {
            let top = line
                .strip_prefix("undeclared field ")
                .and_then(|r| r.split([' ', '.', '[']).next());
            let now = top.is_some_and(|f| declared.contains(f));
            println!(
                "  {line}{}",
                if now {
                    " (declared in the new spec)"
                } else {
                    ""
                }
            );
        }
    }
}

pub(super) fn short(v: &Value) -> String {
    let s = match v {
        Value::Null => "(unset)".to_string(),
        Value::String(s) => format!("{s:?}"),
        other => other.to_string(),
    };
    if s.chars().count() > 80 {
        format!("{}…", s.chars().take(80).collect::<String>())
    } else {
        s
    }
}

pub(super) fn field_short(f: &Option<schema_merge::Field>) -> String {
    match f {
        None => "(absent)".into(),
        Some(f) => short(&f.schema),
    }
}
