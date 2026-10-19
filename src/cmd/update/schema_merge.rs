//! The per-field merge `apisip update` runs on `schemas/<stream>.json`, with
//! the same three sides as apisip.toml:
//!
//! - base: `.apisip/schemas/<stream>.json`, what apisip wrote last time;
//! - theirs: the schema of the new spec;
//! - mine: the current file.
//!
//! Each change is also classed against the current file: compatible changes
//! (a new field, integer to number, a required change, a removed field) are
//! applied; any other type change is incompatible and needs a decision,
//! because files already written keep the old type.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde_json::{Map, Value};

use super::config_merge::Diff;
use super::config_merge::Kind;
use crate::config::Config;
use crate::schema::{RecordSchema, Shape};
use crate::sink::{META_DIR, Store};
use crate::spec::Spec;
use crate::state::Manifest;
use crate::workspace::Workspace;

/// One property of a schema file, with whether it is required.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub schema: Value,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldMerge {
    pub field: String,
    pub base: Option<Field>,
    pub theirs: Option<Field>,
    pub mine: Option<Field>,
    pub kind: Kind,
    /// Whether taking the spec's side keeps the files already written readable.
    pub compatible: bool,
}

impl FieldMerge {
    /// What the spec changed. Without an edit here, that is also what
    /// taking the spec's side does to the current file.
    pub fn describe(&self) -> String {
        let f = &self.field;
        match (&self.base, &self.theirs) {
            (None, Some(t)) => format!("+ {f} ({})", label(t)),
            (Some(m), None) => format!("- {f} ({}), removed from the spec", label(m)),
            (Some(m), Some(t)) if label(m) != label(t) => {
                format!("~ {f}: {} -> {}", label(m), label(t))
            }
            (Some(m), Some(t)) if shape(m) != shape(t) => {
                format!("~ {f}: {} fields changed", label(t))
            }
            (Some(m), Some(t)) if m.required != t.required => format!(
                "~ {f}: {}",
                if t.required {
                    "now required"
                } else {
                    "now optional"
                }
            ),
            _ => format!("~ {f}: description changed"),
        }
    }
}

fn shape(f: &Field) -> Shape {
    Shape::from_schema(&f.schema)
}

fn label(f: &Field) -> &'static str {
    shape(f).label()
}

/// The properties of a schema file, in file order.
pub fn fields(file: &Value) -> Vec<(String, Field)> {
    let required: Vec<&str> = file
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    file.get("properties")
        .and_then(Value::as_object)
        .map(|p| {
            p.iter()
                .map(|(k, v)| {
                    let f = Field {
                        schema: v.clone(),
                        required: required.contains(&k.as_str()),
                    };
                    (k.clone(), f)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The three-way comparison. Without a baseline, the current file counts as
/// unedited.
pub fn diff(base: Option<&Value>, theirs: &Value, mine: &Value) -> Vec<FieldMerge> {
    let (b, t, m) = (fields(base.unwrap_or(mine)), fields(theirs), fields(mine));
    let get = |side: &[(String, Field)], k: &str| {
        side.iter().find(|(n, _)| n == k).map(|(_, f)| f.clone())
    };
    let mut names: Vec<&String> = Vec::new();
    for (n, _) in t.iter().chain(&m).chain(&b) {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    let mut out = Vec::new();
    for name in names {
        let (bf, tf, mf) = (get(&b, name), get(&t, name), get(&m, name));
        if bf == tf {
            continue;
        }
        let kind = if mf == tf {
            Kind::AlreadyApplied
        } else if mf == bf {
            Kind::Update
        } else {
            Kind::Conflict
        };
        let compatible = match (&mf, &tf) {
            (Some(m), Some(t)) => widens(&shape(m), &shape(t)),
            _ => true,
        };
        out.push(FieldMerge {
            field: name.clone(),
            base: bf,
            theirs: tf,
            mine: mf,
            kind,
            compatible,
        });
    }
    out
}

/// Whether data written as `from` still reads as `to`: the same type,
/// integer to number, and objects that only gain fields.
pub fn widens(from: &Shape, to: &Shape) -> bool {
    match (from, to) {
        _ if from == to => true,
        (Shape::Int, Shape::Float) => true,
        (Shape::List(a), Shape::List(b)) => widens(a, b),
        (Shape::Struct(a), Shape::Struct(b)) => a.iter().all(|(n, s)| {
            b.iter()
                .find(|(bn, _)| bn == n)
                .is_some_and(|(_, bs)| widens(s, bs))
        }),
        _ => false,
    }
}

/// The current file with the spec's side of the chosen changes.
pub fn apply(mine: &Value, changes: &[&FieldMerge]) -> Value {
    let mut out = mine.clone();
    for c in changes {
        set_field(&mut out, &c.field, c.theirs.as_ref());
    }
    out
}

/// Declares a field the runs saw but the schema does not.
pub fn adopt(file: &mut Value, name: &str, shape: &Shape) {
    let f = Field {
        schema: shape.json_schema(),
        required: false,
    };
    set_field(file, name, Some(&f));
}

fn set_field(file: &mut Value, name: &str, f: Option<&Field>) {
    let Some(obj) = file.as_object_mut() else {
        return;
    };
    let props = obj
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(p) = props.as_object_mut() {
        match f {
            Some(f) => {
                p.insert(name.to_string(), f.schema.clone());
            }
            None => {
                p.shift_remove(name);
            }
        }
    }
    let mut required: Vec<Value> = obj
        .get("required")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    required.retain(|v| v.as_str() != Some(name));
    if f.is_some_and(|f| f.required) {
        required.push(name.into());
    }
    if required.is_empty() {
        obj.remove("required");
    } else {
        obj.insert("required".into(), Value::Array(required));
    }
}

/// The type the runs observed for an undeclared field, from an example value.
pub fn observed_shape(name: &str, example: &Value) -> Shape {
    let record = Value::Object(Map::from_iter([(name.to_string(), example.clone())]));
    RecordSchema::infer(&[record])
        .fields
        .into_iter()
        .next()
        .map(|(_, s)| s)
        .unwrap_or(Shape::Json)
}

/// A stream's schema file and what the new spec changes in it.
#[derive(Debug)]
pub(super) struct StreamSchema {
    pub(super) name: String,
    /// The spec's schema; `None` when the spec has none (an inferred file).
    pub(super) theirs: Option<Value>,
    pub(super) base: Option<Value>,
    pub(super) mine: Option<Value>,
    pub(super) changes: Vec<FieldMerge>,
    /// Fields seen undeclared in each recent run, with their observed type.
    pub(super) undeclared: Vec<(String, Shape)>,
}

impl StreamSchema {
    pub(super) fn changed(&self) -> bool {
        self.theirs.is_some() && (self.mine.is_none() || !self.changes.is_empty())
    }
}

/// How many recent runs of a stream must all have seen an undeclared field
/// before update offers to declare it.
const RECENT_RUNS: usize = 3;

pub(super) fn plan_schemas(
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
        let theirs = crate::cmd::spec_schema_file(spec, s);
        let mine = ws.read_schema(&s.name)?;
        let base = ws.read_schema_baseline(&s.name)?;
        let changes = match (&theirs, &mine) {
            (Some(t), Some(m)) => diff(base.as_ref(), t, m),
            _ => Vec::new(),
        };
        let declared: BTreeSet<String> = [&theirs, &mine]
            .into_iter()
            .flatten()
            .flat_map(fields)
            .map(|(n, _)| n)
            .collect();
        let undeclared = match &mine {
            Some(_) => undeclared
                .get(&s.name)
                .into_iter()
                .flatten()
                .filter(|(f, _)| !declared.contains(f))
                .map(|(f, ex)| (f.clone(), observed_shape(f, ex)))
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
pub(super) async fn recent_undeclared(store: &Store) -> BTreeMap<String, Vec<(String, Value)>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file(props: Value, required: &[&str]) -> Value {
        json!({"type": "object", "properties": props, "required": required, "x-apisip-source": "spec"})
    }

    fn base() -> Value {
        file(
            json!({
                "id": {"type": "integer"},
                "age": {"type": "integer"},
                "name": {"type": "string"},
                "code": {"type": "integer"},
                "legacy": {"type": "string"},
            }),
            &["id", "name"],
        )
    }

    /// v2: age widens, name becomes optional, code narrows to a string,
    /// legacy is gone, phone is new.
    fn theirs() -> Value {
        file(
            json!({
                "id": {"type": "integer"},
                "age": {"type": "number"},
                "name": {"type": "string"},
                "code": {"type": "string"},
                "phone": {"type": "string"},
            }),
            &["id"],
        )
    }

    fn by_field(d: &[FieldMerge]) -> Vec<(&str, Kind, bool)> {
        d.iter()
            .map(|c| (c.field.as_str(), c.kind, c.compatible))
            .collect()
    }

    #[test]
    fn unedited_fields_follow_the_spec_by_compatibility() {
        let d = diff(Some(&base()), &theirs(), &base());
        assert_eq!(
            by_field(&d),
            [
                ("age", Kind::Update, true),
                ("name", Kind::Update, true),
                ("code", Kind::Update, false),
                ("phone", Kind::Update, true),
                ("legacy", Kind::Update, true),
            ]
        );
        let compatible: Vec<&FieldMerge> = d.iter().filter(|c| c.compatible).collect();
        let out = apply(&base(), &compatible);
        let f = fields(&out);
        let names: Vec<&str> = f.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["id", "age", "name", "code", "phone"]);
        assert_eq!(out["properties"]["age"], json!({"type": "number"}));
        assert_eq!(out["properties"]["code"], json!({"type": "integer"}));
        assert_eq!(out["required"], json!(["id"]));
        // Taking everything gives the spec's file.
        let all: Vec<&FieldMerge> = d.iter().collect();
        assert_eq!(fields(&apply(&base(), &all)), fields(&theirs()));
    }

    #[test]
    fn edits_on_both_sides_conflict_and_matching_edits_are_applied() {
        let mut mine = base();
        mine["properties"]["code"] = json!({"type": "string"});
        mine["properties"]["age"] = json!({"type": "string"});
        let d = diff(Some(&base()), &theirs(), &mine);
        let kinds: Vec<(&str, Kind)> = d.iter().map(|c| (c.field.as_str(), c.kind)).collect();
        assert!(kinds.contains(&("code", Kind::AlreadyApplied)), "{kinds:?}");
        assert!(kinds.contains(&("age", Kind::Conflict)), "{kinds:?}");
        // string -> number would rewrite the user's own type.
        let age = d.iter().find(|c| c.field == "age").unwrap();
        assert!(!age.compatible);
        // An edit the spec did not touch is never listed.
        mine["properties"]["id"] = json!({"type": "string"});
        let d = diff(Some(&base()), &theirs(), &mine);
        assert!(d.iter().all(|c| c.field != "id"), "{d:?}");
    }

    #[test]
    fn widening_rules() {
        let s = |v: Value| Shape::from_schema(&v);
        let obj = |p: Value| s(json!({"type": "object", "properties": p}));
        assert!(widens(&Shape::Int, &Shape::Float));
        assert!(!widens(&Shape::Float, &Shape::Int));
        assert!(!widens(&Shape::Int, &Shape::Str));
        assert!(widens(
            &obj(json!({"a": {"type": "integer"}})),
            &obj(json!({"a": {"type": "number"}, "b": {"type": "string"}}))
        ));
        assert!(!widens(
            &obj(json!({"a": {"type": "integer"}, "b": {"type": "string"}})),
            &obj(json!({"a": {"type": "integer"}}))
        ));
        assert!(widens(
            &Shape::List(Box::new(Shape::Int)),
            &Shape::List(Box::new(Shape::Float))
        ));
    }

    #[test]
    fn adopting_a_field_declares_its_observed_type() {
        let mut f = base();
        adopt(
            &mut f,
            "nickname",
            &observed_shape("nickname", &json!("ada")),
        );
        adopt(&mut f, "score", &observed_shape("score", &json!(3)));
        assert_eq!(f["properties"]["nickname"], json!({"type": "string"}));
        assert_eq!(f["properties"]["score"], json!({"type": "integer"}));
        assert_eq!(f["required"], json!(["id", "name"]));
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
}
