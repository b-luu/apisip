//! The three-way merge of apisip.toml: base (`.apisip/baseline.toml`),
//! theirs (detection on the new spec) and mine (the current file). Pure, so
//! the merge rules are tested directly.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde_json::Value;
use toml_edit::{DocumentMut, Item, Table};

use crate::cmd::init::stream_note;
use crate::config::{AuthConfig, Config, StreamConfig};

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
pub(super) fn changes_views(c: &FieldChange) -> bool {
    c.stream.is_some()
        && (c.key == "primary_key"
            || (c.key == "incremental" && c.mine.get("cursor") != c.theirs.get("cursor")))
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
    fn a_rate_limit_is_never_dropped_silently() {
        let with = BASE.replace(
            "base_url = \"https://api.example.test/v1\"",
            "base_url = \"https://api.example.test/v1\"\nrate_limit = { max_calls = 10, period = \"1s\" }",
        );
        let d = diff(&cfg(&with), &cfg(BASE), &cfg(&with));
        assert!(d.fields.iter().all(|f| f.key != "rate_limit"), "{d:?}");
    }
}
