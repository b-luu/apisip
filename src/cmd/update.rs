//! `apisip update`: re-detect from a new version of the spec and merge the
//! result into apisip.toml, three ways:
//!
//! - base: `.apisip/baseline.toml`, what detection produced last time;
//! - theirs: what detection produces from the new spec;
//! - mine: the current apisip.toml.
//!
//! A value the user never edited follows the spec. A value both changed is a
//! conflict and is asked about (`--yes` keeps the user's). Removed endpoints
//! are disabled, new ones are offered unselected, and auth is re-confirmed.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use toml_edit::{DocumentMut, Item, Table};

use crate::config::{AuthConfig, Config, StreamConfig, referenced_vars};
use crate::detect::{self, AuthOption, Detection, EnvVar};
use crate::envfile;
use crate::prompt::{self, Prompter};
use crate::schema::{RecordSchema, Shape};
use crate::sink::{META_DIR, Store};
use crate::spec::Spec;
use crate::state::Manifest;
use crate::workspace::Workspace;

use super::init::{BASELINE_HEADER, baseline, current_value, stream_note};
use super::schema_merge::{self, FieldMerge};

#[derive(Debug, Default, Clone)]
pub struct UpdateOptions {
    pub spec: Option<String>,
    pub check: bool,
    pub yes: bool,
}

/// Merged keys: the source settings detection owns, then per-stream fields.
const SOURCE_KEYS: [&str; 2] = ["base_url", "rate_limit"];
const STREAM_KEYS: [&str; 6] = [
    "path",
    "records",
    "primary_key",
    "params",
    "pagination",
    "incremental",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The user never edited it: take the spec's value.
    Update,
    /// Both the user and the spec changed it, differently.
    Conflict,
    /// The spec changed it to what the user already has.
    AlreadyApplied,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldChange {
    /// `None` for `[source]`.
    pub stream: Option<String>,
    pub key: String,
    pub base: Value,
    pub theirs: Value,
    pub mine: Value,
    pub kind: Kind,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Diff {
    pub fields: Vec<FieldChange>,
    /// New endpoints, not in apisip.toml yet.
    pub added: Vec<String>,
    /// Endpoints gone from the spec that are still enabled.
    pub removed: Vec<String>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty() && self.added.is_empty() && self.removed.is_empty()
    }
}

/// The three-way comparison. Pure, so the merge rules are tested directly.
pub fn diff(base: &Config, theirs: &Config, mine: &Config) -> Diff {
    let mut d = Diff::default();
    let (b, t, m) = (
        object(&base.source),
        object(&theirs.source),
        object(&mine.source),
    );
    for key in SOURCE_KEYS {
        // A static rate limit is never dropped because the spec stopped
        // documenting it: that would remove the throttle silently.
        if key == "rate_limit" && field(&t, key).is_null() {
            continue;
        }
        if let Some(c) = compare(None, key, &b, &t, &m) {
            d.fields.push(c);
        }
    }
    let by_name = |c: &Config| -> BTreeMap<String, StreamConfig> {
        c.streams
            .iter()
            .map(|s| (s.name.clone(), s.clone()))
            .collect()
    };
    let (bs, ts, ms) = (by_name(base), by_name(theirs), by_name(mine));
    for t_stream in &theirs.streams {
        let name = &t_stream.name;
        match (bs.get(name), ms.get(name)) {
            (_, None) => d.added.push(name.clone()),
            (Some(b_stream), Some(m_stream)) => {
                let (b, t, m) = (object(b_stream), object(t_stream), object(m_stream));
                for key in STREAM_KEYS {
                    if let Some(c) = compare(Some(name), key, &b, &t, &m) {
                        d.fields.push(c);
                    }
                }
            }
            // Added by hand before the spec declared it: the user's version stays.
            (None, Some(_)) => {}
        }
    }
    for name in bs.keys() {
        if !ts.contains_key(name) && ms.get(name).is_some_and(|s| s.enabled) {
            d.removed.push(name.clone());
        }
    }
    d
}

fn compare(
    stream: Option<&String>,
    key: &str,
    base: &Value,
    theirs: &Value,
    mine: &Value,
) -> Option<FieldChange> {
    let (b, t, m) = (field(base, key), field(theirs, key), field(mine, key));
    if b == t {
        return None;
    }
    let kind = if m == t {
        Kind::AlreadyApplied
    } else if m == b {
        Kind::Update
    } else {
        Kind::Conflict
    };
    Some(FieldChange {
        stream: stream.cloned(),
        key: key.to_string(),
        base: b,
        theirs: t,
        mine: m,
        kind,
    })
}

fn object<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn field(obj: &Value, key: &str) -> Value {
    obj.get(key).cloned().unwrap_or(Value::Null)
}

/// What the user decided, applied to the text of apisip.toml.
#[derive(Debug, Default)]
pub struct Resolution {
    /// Field changes to write, by index in `Diff::fields`.
    pub take: BTreeSet<usize>,
    /// New streams to add enabled; the others are added disabled.
    pub enable: BTreeSet<String>,
    pub auth: Option<Option<AuthConfig>>,
    pub spec: Option<String>,
}

/// Edits apisip.toml in place with toml_edit, so comments and layout survive.
pub fn apply(text: &str, d: &Diff, theirs: &Config, r: &Resolution) -> Result<String> {
    let mut doc: DocumentMut = text.parse().context("apisip.toml is not valid TOML")?;
    let rendered: DocumentMut = theirs
        .render("", &BTreeMap::new(), None)
        .parse()
        .expect("rendered config parses");

    for (i, c) in d.fields.iter().enumerate() {
        if !r.take.contains(&i) {
            continue;
        }
        let (target, source) = match &c.stream {
            None => (
                table_mut(&mut doc, "source")?,
                rendered.get("source").and_then(Item::as_table),
            ),
            Some(name) => (
                stream_table_mut(&mut doc, name)?,
                stream_table(&rendered, name),
            ),
        };
        match source.and_then(|t| t.get(&c.key)) {
            Some(item) => {
                target.insert(&c.key, item.clone());
            }
            None => {
                target.remove(&c.key);
            }
        }
    }
    for name in &d.removed {
        stream_table_mut(&mut doc, name)?.insert("enabled", toml_edit::value(false));
    }
    if !d.added.is_empty() {
        if doc.get("streams").is_none() {
            doc.insert(
                "streams",
                Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
            );
        }
        let streams = doc
            .get_mut("streams")
            .and_then(Item::as_array_of_tables_mut)
            .context("`streams` in apisip.toml is not an array of tables")?;
        for name in &d.added {
            let mut t = stream_table(&rendered, name)
                .cloned()
                .context("added stream missing from the detection")?;
            if !r.enable.contains(name) {
                // Right after `path`, where `init` puts it.
                t.insert("enabled", toml_edit::value(false));
                let order = ["name", "path", "enabled"];
                t.sort_values_by(|a, _, b, _| {
                    let rank = |k: &str| order.iter().position(|o| *o == k).unwrap_or(order.len());
                    rank(a.get()).cmp(&rank(b.get()))
                });
            }
            if let Some(s) = theirs.streams.iter().find(|s| &s.name == name) {
                t.decor_mut()
                    .set_prefix(format!("\n{} (added by apisip update)\n", stream_note(s)));
            }
            streams.push(t);
        }
    }
    if let Some(auth) = &r.auth {
        let source = table_mut(&mut doc, "source")?;
        match auth {
            None => {
                source.remove("auth");
            }
            Some(a) => {
                let mut c = theirs.clone();
                c.source.auth = Some(a.clone());
                let rendered: DocumentMut = c
                    .render("", &BTreeMap::new(), None)
                    .parse()
                    .expect("rendered config parses");
                let item = rendered["source"]["auth"].clone();
                source.insert("auth", item);
            }
        }
    }
    if let Some(spec) = &r.spec {
        table_mut(&mut doc, "source")?.insert("spec", toml_edit::value(spec.as_str()));
    }
    let out = doc.to_string();
    Config::parse(&out).context("the merged apisip.toml is invalid")?;
    Ok(out)
}

fn table_mut<'a>(doc: &'a mut DocumentMut, key: &str) -> Result<&'a mut Table> {
    doc.get_mut(key)
        .and_then(Item::as_table_mut)
        .with_context(|| format!("apisip.toml has no [{key}] table"))
}

fn stream_table_mut<'a>(doc: &'a mut DocumentMut, name: &str) -> Result<&'a mut Table> {
    doc.get_mut("streams")
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|a| {
            a.iter_mut()
                .find(|t| t.get("name").and_then(Item::as_str) == Some(name))
        })
        .with_context(|| format!("apisip.toml has no stream `{name}`"))
}

fn stream_table<'a>(doc: &'a DocumentMut, name: &str) -> Option<&'a Table> {
    doc.get("streams")
        .and_then(Item::as_array_of_tables)?
        .iter()
        .find(|t| t.get("name").and_then(Item::as_str) == Some(name))
}

/// A change of the primary key or cursor field changes what the views
/// dedupe on, so it is always asked about.
fn changes_views(c: &FieldChange) -> bool {
    c.stream.is_some()
        && (c.key == "primary_key"
            || (c.key == "incremental" && c.mine.get("cursor") != c.theirs.get("cursor")))
}

/// A stream's schema file and what the new spec changes in it.
#[derive(Debug)]
struct StreamSchema {
    name: String,
    /// The spec's schema; `None` when the spec has none (an inferred file).
    theirs: Option<Value>,
    base: Option<Value>,
    mine: Option<Value>,
    changes: Vec<FieldMerge>,
    /// Fields seen undeclared in each recent run, with their observed type.
    undeclared: Vec<(String, Shape)>,
}

impl StreamSchema {
    fn changed(&self) -> bool {
        self.theirs.is_some() && (self.mine.is_none() || !self.changes.is_empty())
    }
}

/// How many recent runs of a stream must all have seen an undeclared field
/// before update offers to declare it.
const RECENT_RUNS: usize = 3;

fn plan_schemas(
    ws: &Workspace,
    mine: &Config,
    theirs: &Config,
    d: &Diff,
    spec: &Spec,
    undeclared: &BTreeMap<String, Vec<(String, Value)>>,
) -> Result<Vec<StreamSchema>> {
    let streams = mine.streams.iter().map(|s| {
        theirs
            .streams
            .iter()
            .find(|t| t.name == s.name)
            .unwrap_or(s)
    });
    let added = theirs.streams.iter().filter(|t| d.added.contains(&t.name));
    let mut out = Vec::new();
    for s in streams.chain(added) {
        let theirs = super::spec_schema_file(spec, s);
        let mine = ws.read_schema(&s.name)?;
        let base = ws.read_schema_baseline(&s.name)?;
        let changes = match (&theirs, &mine) {
            (Some(t), Some(m)) => schema_merge::diff(base.as_ref(), t, m),
            _ => Vec::new(),
        };
        let declared: BTreeSet<String> = [&theirs, &mine]
            .into_iter()
            .flatten()
            .flat_map(schema_merge::fields)
            .map(|(n, _)| n)
            .collect();
        let undeclared = match &mine {
            Some(_) => undeclared
                .get(&s.name)
                .into_iter()
                .flatten()
                .filter(|(f, _)| !declared.contains(f))
                .map(|(f, ex)| (f.clone(), schema_merge::observed_shape(f, ex)))
                .collect(),
            None => Vec::new(),
        };
        out.push(StreamSchema {
            name: s.name.clone(),
            theirs,
            base,
            mine,
            changes,
            undeclared,
        });
    }
    Ok(out)
}

/// Top-level fields undeclared in each of the last runs of a stream (up to
/// `RECENT_RUNS`), with the latest example.
async fn recent_undeclared(store: &Store) -> BTreeMap<String, Vec<(String, Value)>> {
    let Ok(mut runs) = store.list(&format!("{META_DIR}/runs")).await else {
        return BTreeMap::new();
    };
    // Run ids start with their timestamp: newest first.
    runs.sort_by(|a, b| b.location.cmp(&a.location));
    let mut seen: BTreeMap<String, Vec<BTreeMap<String, Value>>> = BTreeMap::new();
    for meta in runs.iter().take(50) {
        let Ok(Some(bytes)) = store.get(meta.location.as_ref()).await else {
            continue;
        };
        let Ok(m) = serde_json::from_slice::<Manifest>(&bytes) else {
            continue;
        };
        for (name, r) in m.streams {
            let Some(drift) = r.drift else { continue };
            let runs = seen.entry(name).or_default();
            if runs.len() < RECENT_RUNS {
                runs.push(
                    drift
                        .undeclared
                        .into_iter()
                        .filter(|(p, _)| !p.contains(['.', '[']))
                        .map(|(p, s)| (p, s.example))
                        .collect(),
                );
            }
        }
    }
    seen.into_iter()
        .map(|(name, runs)| (name, in_every_run(&runs)))
        .collect()
}

/// The fields of the newest run (first) that every other run saw too.
fn in_every_run(runs: &[BTreeMap<String, Value>]) -> Vec<(String, Value)> {
    let Some(newest) = runs.first() else {
        return Vec::new();
    };
    newest
        .iter()
        .filter(|(f, _)| runs.iter().all(|r| r.contains_key(*f)))
        .map(|(f, ex)| (f.clone(), ex.clone()))
        .collect()
}

fn field_short(f: &Option<schema_merge::Field>) -> String {
    match f {
        None => "(absent)".into(),
        Some(f) => short(&f.schema),
    }
}

/// Returns false when `--check` found changes (exit code 1).
pub async fn run(ws: &Workspace, o: UpdateOptions) -> Result<bool> {
    let text = ws.read_config_text()?;
    let mine = ws.load_config()?;
    let base_path = ws.baseline_path();
    let base_text = std::fs::read_to_string(&base_path).with_context(|| {
        format!(
            "reading {}: update needs the baseline `apisip init` writes. Re-create it with `apisip init --force`",
            base_path.display()
        )
    })?;
    let base = Config::parse(&base_text).context("invalid .apisip/baseline.toml")?;
    let old_spec = ws.load_spec(&mine).await.ok();

    let new_spec = match &o.spec {
        Some(s) => Spec::load(s, &std::env::current_dir()?).await?,
        None => Spec::load(&mine.source.spec, &ws.dir).await?,
    };
    let det = detect::detect(&new_spec);
    let theirs = baseline(&det, &mine.source);
    let d = diff(&base, &theirs, &mine);

    let store = super::open_existing_store(ws, &mine);
    let undeclared = match &store {
        Some(s) => recent_undeclared(s).await,
        None => BTreeMap::new(),
    };
    let schemas = plan_schemas(ws, &mine, &theirs, &d, &new_spec, &undeclared)?;
    let spec_change = o.spec.as_ref().filter(|s| **s != mine.source.spec).cloned();
    let old_auth = old_spec.as_ref().map(|s| detect::detect(s).auth);
    let auth_changed = old_auth
        .as_ref()
        .is_some_and(|old| auth_names(old) != auth_names(&det.auth));

    print_diff(&d, &schemas, auth_changed.then_some((&old_auth, &det.auth)));
    if let Some(store) = &store {
        print_drift(store, &mine, &new_spec).await;
    }
    let changed = !d.is_empty()
        || schemas.iter().any(StreamSchema::changed)
        || auth_changed
        || spec_change.is_some();
    if o.check {
        if !changed {
            println!("No change: apisip.toml matches the spec.");
        }
        return Ok(!changed);
    }

    let p = Prompter::new(o.yes)?;
    if !p.interactive {
        let mut blocking: Vec<String> = d
            .fields
            .iter()
            .filter(|c| c.kind == Kind::Update && changes_views(c))
            .map(|c| {
                format!(
                    "streams.{}.{}: {} -> {}",
                    c.stream.as_deref().unwrap_or_default(),
                    c.key,
                    short(&c.mine),
                    short(&c.theirs)
                )
            })
            .collect();
        for s in &schemas {
            for c in &s.changes {
                if c.kind == Kind::Update && !c.compatible {
                    blocking.push(format!("schemas/{}.json: {}", s.name, c.describe()));
                }
            }
        }
        if !blocking.is_empty() {
            bail!(
                "nothing was written: these spec changes do not match the data already written and need a decision:\n  {}\nRun `apisip update` in a terminal to choose.",
                blocking.join("\n  ")
            );
        }
    }
    let mut r = Resolution {
        spec: spec_change,
        ..Default::default()
    };
    for (i, c) in d.fields.iter().enumerate() {
        match c.kind {
            Kind::Update if changes_views(c) => {
                let options = vec![
                    format!("keep the current one: {}", short(&c.mine)),
                    format!("take the spec's: {}", short(&c.theirs)),
                ];
                let pick = p.select(
                    &format!(
                        "streams.{}.{} changed in the spec; the views deduplicate on it",
                        c.stream.as_deref().unwrap_or_default(),
                        c.key
                    ),
                    &options,
                    Some(0),
                )?;
                if pick == 1 {
                    r.take.insert(i);
                }
            }
            Kind::Update => {
                r.take.insert(i);
            }
            Kind::AlreadyApplied => {}
            Kind::Conflict => {
                let options = vec![
                    format!("keep mine: {}", short(&c.mine)),
                    format!("take the spec's: {}", short(&c.theirs)),
                ];
                let what = match &c.stream {
                    Some(s) => format!("streams.{s}.{}", c.key),
                    None => format!("source.{}", c.key),
                };
                let pick = p.select(
                    &format!("{what} was edited here and changed in the spec"),
                    &options,
                    Some(0),
                )?;
                if pick == 1 {
                    r.take.insert(i);
                }
            }
        }
    }
    if !d.added.is_empty() {
        let labels: Vec<String> = d
            .added
            .iter()
            .map(|n| {
                let s = theirs.streams.iter().find(|s| &s.name == n).unwrap();
                format!("{n:<24} GET {:<32} {}", s.path, s.mode().label())
            })
            .collect();
        let picked = p.multi_select("New endpoints to download", &labels, &[])?;
        r.enable = picked.into_iter().map(|i| d.added[i].clone()).collect();
    }

    let mut schema_picks: Vec<(Vec<bool>, Vec<usize>)> = Vec::new();
    for s in &schemas {
        let mut take = Vec::new();
        for c in &s.changes {
            take.push(match c.kind {
                Kind::AlreadyApplied => false,
                Kind::Update if c.compatible => true,
                Kind::Update => {
                    let options = vec![
                        format!("keep the current one: {}", field_short(&c.mine)),
                        format!("take the spec's: {}", field_short(&c.theirs)),
                    ];
                    let message = format!(
                        "schemas/{}.json: {} (the files already written keep the old type)",
                        s.name,
                        c.describe().trim_start_matches("~ ")
                    );
                    p.select(&message, &options, Some(0))? == 1
                }
                Kind::Conflict => {
                    let options = vec![
                        format!("keep mine: {}", field_short(&c.mine)),
                        format!("take the spec's: {}", field_short(&c.theirs)),
                    ];
                    let message = format!(
                        "schemas/{}.json: {} was edited here and changed in the spec",
                        s.name, c.field
                    );
                    p.select(&message, &options, Some(0))? == 1
                }
            });
        }
        let adopt = if s.undeclared.is_empty() {
            Vec::new()
        } else {
            let labels: Vec<String> = s
                .undeclared
                .iter()
                .map(|(f, shape)| format!("{f} ({})", shape.label()))
                .collect();
            p.multi_select(
                &format!(
                    "Declare in schemas/{}.json the fields each recent run saw",
                    s.name
                ),
                &labels,
                &[],
            )?
        };
        schema_picks.push((take, adopt));
    }

    let (auth, vars) = confirm_auth(&p, &det, &base, &mine, old_auth.as_deref())?;
    if auth != mine.source.auth {
        r.auth = Some(auth.clone());
    }
    let creds = prompt::ask_credentials(&p, &vars, |k| current_value(ws, k))?;

    let merged = apply(&text, &d, &theirs, &r)?;
    let wrote = merged != text;
    if wrote {
        std::fs::write(ws.config_path(), &merged)?;
    }
    let new_base = Config {
        source: crate::config::Source {
            auth,
            ..theirs.source.clone()
        },
        ..theirs.clone()
    };
    ws.save_meta(
        &new_spec,
        &new_base.render(BASELINE_HEADER, &BTreeMap::new(), None),
    )?;
    let mut kept = Vec::new();
    for (s, (take, adopt)) in schemas.iter().zip(&schema_picks) {
        let Some(m) = &s.mine else {
            if let Some(t) = &s.theirs {
                ws.write_schema(&s.name, t)?;
            }
            continue;
        };
        let taken: Vec<&FieldMerge> = s
            .changes
            .iter()
            .zip(take)
            .filter(|(_, t)| **t)
            .map(|(c, _)| c)
            .collect();
        let all_taken = taken.len()
            == s.changes
                .iter()
                .filter(|c| c.kind != Kind::AlreadyApplied)
                .count();
        let mut out = match &s.theirs {
            Some(t) if all_taken && s.base.as_ref() == Some(m) => t.clone(),
            _ => schema_merge::apply(m, &taken),
        };
        for i in adopt {
            let (f, shape) = &s.undeclared[*i];
            schema_merge::adopt(&mut out, f, shape);
        }
        let not_taken: Vec<&str> = s
            .changes
            .iter()
            .zip(take)
            .filter(|(c, t)| c.kind != Kind::AlreadyApplied && !**t)
            .map(|(c, _)| c.field.as_str())
            .collect();
        if !not_taken.is_empty() {
            kept.push(format!(
                "schemas/{}.json keeps your version of: {}",
                s.name,
                not_taken.join(", ")
            ));
        }
        match &s.theirs {
            Some(t) => ws.write_schema_files(&s.name, &out, t)?,
            None if out != *m => {
                let base = s.base.as_ref().unwrap_or(m);
                ws.write_schema_files(&s.name, &out, base)?
            }
            None => {}
        }
    }
    let updates: Vec<(String, Option<String>)> =
        creds.into_iter().filter(|(_, v)| v.is_some()).collect();
    if !updates.is_empty() {
        envfile::upsert(&ws.env_path(), &updates)?;
    }
    let mut example_keys: Vec<String> = envfile::read(&ws.dir.join(".env.example"))
        .unwrap_or_default()
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    for v in &vars {
        if !example_keys.contains(&v.name) {
            example_keys.push(v.name.clone());
        }
    }
    envfile::write_example(&ws.dir.join(".env.example"), &example_keys)?;

    println!(
        "\n{} Spec snapshot and baseline refreshed in .apisip/.",
        if wrote {
            "Updated apisip.toml."
        } else {
            "apisip.toml unchanged."
        }
    );
    if !d.removed.is_empty() {
        println!(
            "Disabled (gone from the spec, data and watermarks kept): {}",
            d.removed.join(", ")
        );
    }
    for line in kept {
        println!("{line}");
    }
    Ok(true)
}

fn auth_names(options: &[AuthOption]) -> BTreeSet<String> {
    options
        .iter()
        .filter(|o| o.config.is_some())
        .map(|o| o.scheme.clone())
        .collect()
}

/// Auth is always re-confirmed: the current scheme is preselected and each
/// credential shows its masked value, so Enter keeps everything.
fn confirm_auth(
    p: &Prompter,
    det: &Detection,
    base: &Config,
    mine: &Config,
    old: Option<&[AuthOption]>,
) -> Result<(Option<AuthConfig>, Vec<EnvVar>)> {
    let options: Vec<AuthOption> = if det.auth.iter().any(|o| o.config.is_some()) {
        det.auth.clone()
    } else {
        prompt::generic_auth_options(&det.env_prefix)
    };
    let current = mine.source.auth.as_ref();
    let matches = |o: &AuthOption| match (current, &o.config) {
        (Some(c), Some(oc)) => match c.scheme() {
            Some(name) => name == o.scheme,
            None => c.kind() == oc.kind() && oc.scheme().is_none(),
        },
        _ => false,
    };
    let old_names = old.map(auth_names);
    let items = prompt::auth_items(&options, |o| {
        if matches(o) {
            Some("current")
        } else if old_names.as_ref().is_some_and(|n| !n.contains(&o.scheme)) {
            Some("new")
        } else {
            None
        }
    });
    let labels: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
    let default = match current {
        None => Some(items.len() - 1),
        Some(_) => items
            .iter()
            .position(|i| i.option.as_ref().is_some_and(&matches)),
    };
    if let (Some(c), None) = (current, default) {
        let name = c.scheme().unwrap_or(c.kind());
        eprintln!("The auth scheme in use, `{name}`, is no longer in the spec [removed].");
        if !p.interactive {
            bail!(
                "the auth scheme `{name}` was removed from the spec: run `apisip update` in a terminal to pick another"
            );
        }
    }
    let i = p.select(
        "Authentication (Enter keeps the current one)",
        &labels,
        default,
    )?;
    let Some(opt) = &items[i].option else {
        return Ok((None, Vec::new()));
    };
    let detected = opt.config.clone().unwrap();
    if !matches(opt) {
        return Ok((Some(detected), opt.vars.clone()));
    }
    let cur = current.cloned().unwrap();
    let (auth, vars) = same_scheme(base.source.auth.as_ref(), cur, detected, &opt.vars);
    Ok((Some(auth), vars))
}

/// Same scheme: follow the spec unless the user edited the auth table. The
/// current variable names are kept when only they differ (the spec title
/// they derive from changed), so `.env` keeps working.
fn same_scheme(
    base: Option<&AuthConfig>,
    cur: AuthConfig,
    detected: AuthConfig,
    known: &[EnvVar],
) -> (AuthConfig, Vec<EnvVar>) {
    let edited = base != Some(&cur);
    if cur == detected || (!edited && !same_but_vars(&cur, &detected)) {
        return (detected, known.to_vec());
    }
    let vars = auth_vars(&cur, known);
    (cur, vars)
}

fn same_but_vars(a: &AuthConfig, b: &AuthConfig) -> bool {
    let re = regex::Regex::new(r"\$\{[A-Za-z_][A-Za-z0-9_]*\}").unwrap();
    let shape = |c: &AuthConfig| {
        re.replace_all(&serde_json::to_string(c).unwrap_or_default(), "${}")
            .into_owned()
    };
    shape(a) == shape(b)
}

/// The variables an edited auth table references.
fn auth_vars(auth: &AuthConfig, known: &[EnvVar]) -> Vec<EnvVar> {
    let text = serde_json::to_string(auth).unwrap_or_default();
    let mut out = Vec::new();
    for name in referenced_vars(&text) {
        if out.iter().any(|v: &EnvVar| v.name == name) {
            continue;
        }
        out.push(
            known
                .iter()
                .find(|v| v.name == name)
                .cloned()
                .unwrap_or_else(|| EnvVar {
                    secret: !(name.ends_with("USERNAME") || name.ends_with("CLIENT_ID")),
                    label: name.clone(),
                    name,
                }),
        );
    }
    out
}

fn print_diff(
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
async fn print_drift(store: &Store, config: &Config, new_spec: &Spec) {
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

fn short(v: &Value) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
[source]
spec = "spec.json"
base_url = "https://api.example.test/v1"

[destination]

[[streams]]
name = "users"
path = "/users"
primary_key = "id"
pagination = { type = "page_number", param = "page" }

[[streams]]
name = "teams"
path = "/teams"
primary_key = "id"

[[streams]]
name = "legacy"
path = "/legacy"
"#;

    fn cfg(s: &str) -> Config {
        Config::parse(s).unwrap()
    }

    /// v2 of the spec: base URL, users paging and teams key change; legacy
    /// is gone; projects is new.
    fn theirs() -> Config {
        cfg(&BASE
            .replace("/v1", "/v2")
            .replace(
                r#"{ type = "page_number", param = "page" }"#,
                r#"{ type = "cursor", param = "cursor", cursor_path = "/next" }"#,
            )
            .replace(
                "name = \"teams\"\npath = \"/teams\"\nprimary_key = \"id\"",
                "name = \"teams\"\npath = \"/teams\"\nprimary_key = \"team_id\"",
            )
            .replace(
                "[[streams]]\nname = \"legacy\"\npath = \"/legacy\"\n",
                "[[streams]]\nname = \"projects\"\npath = \"/projects\"\n",
            ))
    }

    /// The user edited the teams key and commented the file.
    fn mine_text() -> String {
        BASE.replace(
            "name = \"teams\"\npath = \"/teams\"\nprimary_key = \"id\"",
            "name = \"teams\"\npath = \"/teams\"\nprimary_key = \"slug\" # my key",
        )
        .replace("[destination]", "# where data goes\n[destination]")
    }

    #[test]
    fn three_way_rules() {
        let d = diff(&cfg(BASE), &theirs(), &cfg(&mine_text()));
        let kinds: Vec<(Option<&str>, &str, Kind)> = d
            .fields
            .iter()
            .map(|c| (c.stream.as_deref(), c.key.as_str(), c.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                (None, "base_url", Kind::Update),
                (Some("users"), "pagination", Kind::Update),
                (Some("teams"), "primary_key", Kind::Conflict),
            ]
        );
        assert_eq!(d.added, ["projects"]);
        assert_eq!(d.removed, ["legacy"]);
        // The same spec again: nothing to do.
        assert!(diff(&theirs(), &theirs(), &theirs()).is_empty());
    }

    #[test]
    fn apply_keeps_comments_and_edits() {
        let mine = mine_text();
        let d = diff(&cfg(BASE), &theirs(), &cfg(&mine));
        // Take the updates, keep the conflicting edit.
        let r = Resolution {
            take: [0, 1].into(),
            ..Default::default()
        };
        let out = apply(&mine, &d, &theirs(), &r).unwrap();
        assert!(out.contains("# where data goes"), "{out}");
        assert!(out.contains("primary_key = \"slug\" # my key"), "{out}");
        assert!(out.contains("/v2"), "{out}");
        let c = cfg(&out);
        assert!(matches!(
            c.streams[0].pagination,
            Some(crate::config::Pagination::Cursor { .. })
        ));
        let legacy = c.streams.iter().find(|s| s.name == "legacy").unwrap();
        assert!(
            !legacy.enabled,
            "a removed endpoint is disabled, not deleted"
        );
        let projects = c.streams.iter().find(|s| s.name == "projects").unwrap();
        assert!(!projects.enabled, "new endpoints are added unselected");
        // A second merge of the same spec changes nothing.
        let d2 = diff(&theirs(), &theirs(), &c);
        assert!(d2.fields.iter().all(|f| f.kind != Kind::Update), "{d2:?}");
        assert!(d2.added.is_empty() && d2.removed.is_empty());
    }

    #[test]
    fn taking_the_spec_side_of_a_conflict() {
        let mine = mine_text();
        let d = diff(&cfg(BASE), &theirs(), &cfg(&mine));
        let r = Resolution {
            take: [2].into(),
            enable: ["projects".to_string()].into(),
            ..Default::default()
        };
        let c = cfg(&apply(&mine, &d, &theirs(), &r).unwrap());
        let teams = c.streams.iter().find(|s| s.name == "teams").unwrap();
        assert_eq!(teams.primary_key, ["team_id"]);
        assert!(
            c.streams
                .iter()
                .find(|s| s.name == "projects")
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn a_renamed_spec_keeps_the_auth_variables_in_use() {
        let auth = |var: &str, header: &str| -> AuthConfig {
            toml::from_str(&format!(
                "type = \"api_key\"\nin = \"header\"\nname = \"{header}\"\nvalue = \"${{{var}}}\""
            ))
            .unwrap()
        };
        let old = auth("OLD_API_KEY", "X-Key");
        let known = [EnvVar {
            name: "NEW_API_KEY".into(),
            label: "API key".into(),
            secret: true,
        }];
        // Only the variable name changed: keep the one .env defines.
        let (kept, vars) = same_scheme(
            Some(&old),
            old.clone(),
            auth("NEW_API_KEY", "X-Key"),
            &known,
        );
        assert_eq!(kept, old);
        assert_eq!(
            vars.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
            ["OLD_API_KEY"]
        );
        // A real change in the spec is followed.
        let moved = auth("NEW_API_KEY", "X-Api-Key");
        let (taken, _) = same_scheme(Some(&old), old.clone(), moved.clone(), &known);
        assert_eq!(taken, moved);
    }

    #[test]
    fn a_key_or_cursor_change_is_asked_about_even_unedited() {
        let d = diff(&cfg(BASE), &theirs(), &cfg(BASE));
        let asks: Vec<(&str, bool)> = d
            .fields
            .iter()
            .map(|c| (c.key.as_str(), changes_views(c)))
            .collect();
        assert_eq!(
            asks,
            [
                ("base_url", false),
                ("pagination", false),
                ("primary_key", true)
            ]
        );
        let inc = |cursor: &str, param: &str| {
            BASE.replace(
                "name = \"teams\"\npath = \"/teams\"\nprimary_key = \"id\"",
                &format!(
                    "name = \"teams\"\npath = \"/teams\"\nprimary_key = \"id\"\nincremental = {{ cursor = \"{cursor}\", param = \"{param}\" }}"
                ),
            )
        };
        let base = cfg(&inc("updated_at", "since"));
        let param_only = diff(&base, &cfg(&inc("updated_at", "from")), &base);
        assert!(!changes_views(&param_only.fields[0]), "{param_only:?}");
        let cursor = diff(&base, &cfg(&inc("modified_at", "since")), &base);
        assert!(changes_views(&cursor.fields[0]), "{cursor:?}");
    }

    #[test]
    fn only_fields_every_recent_run_saw_are_offered() {
        let run = |fields: &[&str]| -> BTreeMap<String, Value> {
            fields
                .iter()
                .map(|f| (f.to_string(), Value::from(*f)))
                .collect()
        };
        let runs = [run(&["a", "b"]), run(&["a", "c"]), run(&["a", "b"])];
        assert_eq!(in_every_run(&runs), [("a".to_string(), Value::from("a"))]);
        assert!(in_every_run(&[]).is_empty());
    }

    #[test]
    fn a_rate_limit_is_never_dropped_silently() {
        let with = BASE.replace(
            "base_url = \"https://api.example.test/v1\"",
            "base_url = \"https://api.example.test/v1\"\nrate_limit = { max_calls = 10, period = \"1s\" }",
        );
        let d = diff(&cfg(&with), &cfg(BASE), &cfg(&with));
        assert!(d.fields.iter().all(|f| f.key != "rate_limit"), "{d:?}");
    }
}
