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
use crate::spec::Spec;
use crate::workspace::Workspace;

use super::init::{BASELINE_HEADER, baseline, current_value, stream_note};

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

/// Field-level changes of a stream's record schema between two spec versions.
pub fn schema_changes(old: &Spec, new: &Spec, stream: &StreamConfig) -> Vec<String> {
    let fields = |spec: &Spec| -> Option<BTreeMap<String, Shape>> {
        let item = detect::stream_record_schema(spec, stream)?;
        Some(RecordSchema::from_spec(&item)?.fields.into_iter().collect())
    };
    let (Some(o), Some(n)) = (fields(old), fields(new)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (k, s) in &n {
        match o.get(k) {
            None => out.push(format!("+ {k} ({})", s.label())),
            Some(prev) if prev != s => {
                if prev.label() == s.label() {
                    out.push(format!("~ {k}: {} fields changed", s.label()));
                } else {
                    out.push(format!("~ {k}: {} -> {}", prev.label(), s.label()));
                }
            }
            Some(_) => {}
        }
    }
    for (k, s) in &o {
        if !n.contains_key(k) {
            out.push(format!("- {k} ({})", s.label()));
        }
    }
    out
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

    let schema: Vec<(String, Vec<String>)> = match &old_spec {
        Some(old) => mine
            .streams
            .iter()
            .filter(|s| theirs.streams.iter().any(|t| t.name == s.name))
            .map(|s| (s.name.clone(), schema_changes(old, &new_spec, s)))
            .filter(|(_, c)| !c.is_empty())
            .collect(),
        None => Vec::new(),
    };
    let spec_change = o.spec.as_ref().filter(|s| **s != mine.source.spec).cloned();
    let old_auth = old_spec.as_ref().map(|s| detect::detect(s).auth);
    let auth_changed = old_auth
        .as_ref()
        .is_some_and(|old| auth_names(old) != auth_names(&det.auth));

    print_diff(&d, &schema, auth_changed.then_some((&old_auth, &det.auth)));
    print_drift(ws, &mine, &new_spec).await;
    let changed = !d.is_empty() || !schema.is_empty() || auth_changed || spec_change.is_some();
    if o.check {
        if !changed {
            println!("No change: apisip.toml matches the spec.");
        }
        return Ok(!changed);
    }

    let p = Prompter::new(o.yes)?;
    let mut r = Resolution {
        spec: spec_change,
        ..Default::default()
    };
    for (i, c) in d.fields.iter().enumerate() {
        match c.kind {
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
    let edited = refresh_schemas(ws, &Config::parse(&merged)?, &new_spec)?;
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
    for name in edited {
        println!(
            "schemas/{name}.json was edited here: left as is, apply the spec's changes by hand"
        );
    }
    Ok(true)
}

/// Writes the spec's schema for each stream whose file is missing or was
/// never edited. Returns the streams whose edited file differs from it.
fn refresh_schemas(ws: &Workspace, config: &Config, spec: &Spec) -> Result<Vec<String>> {
    let mut edited = Vec::new();
    for s in &config.streams {
        let Some(new) = super::spec_schema_file(spec, s) else {
            continue;
        };
        match ws.read_schema(&s.name)? {
            Some(current) if current == new => ws.write_schema(&s.name, &new)?,
            Some(_) if ws.schema_edited(&s.name)? => edited.push(s.name.clone()),
            _ => ws.write_schema(&s.name, &new)?,
        }
    }
    Ok(edited)
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
    schema: &[(String, Vec<String>)],
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
    for (stream, changes) in schema {
        println!("Schema of {stream}:");
        for c in changes {
            println!("  {c}");
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
async fn print_drift(ws: &Workspace, config: &Config, new_spec: &Spec) {
    let Ok(store) = crate::sink::Store::open(&config.destination, &ws.dir) else {
        return;
    };
    let Ok(state) = crate::state::State::load(&store).await else {
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
        let Ok(Some(bytes)) = store.get(&crate::state::Manifest::path(&run_id)).await else {
            continue;
        };
        let Ok(m) = serde_json::from_slice::<crate::state::Manifest>(&bytes) else {
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
    fn a_rate_limit_is_never_dropped_silently() {
        let with = BASE.replace(
            "base_url = \"https://api.example.test/v1\"",
            "base_url = \"https://api.example.test/v1\"\nrate_limit = { max_calls = 10, period = \"1s\" }",
        );
        let d = diff(&cfg(&with), &cfg(BASE), &cfg(&with));
        assert!(d.fields.iter().all(|f| f.key != "rate_limit"), "{d:?}");
    }
}
