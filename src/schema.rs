//! Record schemas. The spec is the source of truth: Parquet columns come
//! from the response schema, every record is checked against it, and what
//! does not fit is reported as drift (and quarantined for Parquet).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use arrow::datatypes::{DataType, Field, Fields, Schema, TimeUnit};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::spec::schema_type;

pub const EXTRA: &str = "_apisip_extra";
pub const RUN_ID: &str = "_apisip_run_id";
pub const EXTRACTED_AT: &str = "_apisip_extracted_at";
/// Top-level key of a schema file that says where the schema came from.
pub const SOURCE_KEY: &str = "x-apisip-source";

#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Int,
    Float,
    Bool,
    Str,
    Timestamp,
    Date,
    /// Free-form, `oneOf`/`anyOf` or mixed types: stored as JSON text.
    Json,
    Struct(Vec<(String, Shape)>),
    List(Box<Shape>),
}

impl Shape {
    /// Maps a (fully inlined) JSON schema.
    pub fn from_schema(s: &Value) -> Shape {
        if s.get("oneOf").is_some() || s.get("anyOf").is_some() {
            return Shape::Json;
        }
        if let Some(Value::Array(ts)) = s.get("type")
            && ts.iter().filter(|t| t.as_str() != Some("null")).count() > 1
        {
            return Shape::Json;
        }
        match schema_type(s) {
            Some("integer") => Shape::Int,
            Some("number") => Shape::Float,
            Some("boolean") => Shape::Bool,
            Some("string") => match s.get("format").and_then(Value::as_str) {
                Some("date-time") => Shape::Timestamp,
                Some("date") => Shape::Date,
                _ => Shape::Str,
            },
            Some("array") => Shape::List(Box::new(
                s.get("items")
                    .map(Shape::from_schema)
                    .unwrap_or(Shape::Json),
            )),
            Some("object") | None => match s.get("properties").and_then(Value::as_object) {
                Some(props) if !props.is_empty() => Shape::Struct(
                    props
                        .iter()
                        .map(|(k, v)| (k.clone(), Shape::from_schema(v)))
                        .collect(),
                ),
                _ => Shape::Json,
            },
            Some(_) => Shape::Json,
        }
    }

    pub fn arrow(&self) -> DataType {
        match self {
            Shape::Int => DataType::Int64,
            Shape::Float => DataType::Float64,
            Shape::Bool => DataType::Boolean,
            Shape::Str | Shape::Json => DataType::Utf8,
            Shape::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, Some("+00:00".into())),
            Shape::Date => DataType::Date32,
            Shape::Struct(fields) => DataType::Struct(arrow_fields(fields)),
            Shape::List(item) => DataType::List(Arc::new(Field::new("item", item.arrow(), true))),
        }
    }

    /// The JSON schema `from_schema` maps back to this shape.
    pub fn json_schema(&self) -> Value {
        let t = |t: &str| Value::Object(Map::from_iter([("type".into(), Value::from(t))]));
        match self {
            Shape::Int => t("integer"),
            Shape::Float => t("number"),
            Shape::Bool => t("boolean"),
            Shape::Str => t("string"),
            Shape::Timestamp => serde_json::json!({"type": "string", "format": "date-time"}),
            Shape::Date => serde_json::json!({"type": "string", "format": "date"}),
            Shape::Json => Value::Object(Map::new()),
            Shape::Struct(fields) => serde_json::json!({
                "type": "object",
                "properties": Map::from_iter(fields.iter().map(|(n, s)| (n.clone(), s.json_schema()))),
            }),
            Shape::List(item) => serde_json::json!({"type": "array", "items": item.json_schema()}),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Shape::Int => "integer",
            Shape::Float => "number",
            Shape::Bool => "boolean",
            Shape::Str => "string",
            Shape::Timestamp => "date-time string",
            Shape::Date => "date string",
            Shape::Json => "any",
            Shape::Struct(_) => "object",
            Shape::List(_) => "array",
        }
    }
}

fn arrow_fields(fields: &[(String, Shape)]) -> Fields {
    fields
        .iter()
        .map(|(n, s)| Field::new(n, s.arrow(), true))
        .collect()
}

/// Where a record schema came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaSource {
    Spec,
    Inferred,
}

#[derive(Debug, Clone)]
pub struct RecordSchema {
    pub fields: Vec<(String, Shape)>,
    pub source: SchemaSource,
    /// Whether `conform` applies lossless casts (see `config::Cast`).
    pub cast: bool,
}

impl RecordSchema {
    /// From the item schema of a stream (see `detect::stream_record_schema`).
    pub fn from_spec(item: &Value) -> Option<RecordSchema> {
        match Shape::from_schema(item) {
            Shape::Struct(fields) => Some(RecordSchema {
                fields,
                source: SchemaSource::Spec,
                cast: true,
            }),
            _ => None,
        }
    }

    /// From a `schemas/<stream>.json` file.
    pub fn from_file(v: &Value) -> Option<RecordSchema> {
        let mut s = RecordSchema::from_spec(v)?;
        if v.get(SOURCE_KEY).and_then(Value::as_str) == Some("inferred") {
            s.source = SchemaSource::Inferred;
        }
        Some(s)
    }

    /// The content of a `schemas/<stream>.json` file for this schema.
    pub fn to_file(&self) -> Value {
        let mut v = Shape::Struct(self.fields.clone()).json_schema();
        v[SOURCE_KEY] = serde_json::to_value(self.source).unwrap_or_default();
        v
    }

    /// Infers a schema from sample records: integers and floats widen to
    /// float, any other conflict widens to string.
    pub fn infer(records: &[Value]) -> RecordSchema {
        let mut shape: Option<Shape> = None;
        for r in records {
            let s = infer_value(&wrap(r.clone()));
            shape = Some(match shape {
                None => s,
                Some(prev) => merge(prev, s),
            });
        }
        let fields = match shape {
            Some(Shape::Struct(f)) => f,
            _ => vec![("value".into(), Shape::Json)],
        };
        RecordSchema {
            fields: fields.into_iter().map(|(n, s)| (n, finalize(s))).collect(),
            source: SchemaSource::Inferred,
            cast: true,
        }
    }

    /// Arrow schema of the output: the record fields plus apisip's columns.
    pub fn arrow(&self) -> Arc<Schema> {
        let mut fields: Vec<Field> = self
            .fields
            .iter()
            .filter(|(n, _)| !n.starts_with("_apisip_"))
            .map(|(n, s)| Field::new(n, s.arrow(), true))
            .collect();
        fields.push(Field::new(EXTRA, DataType::Utf8, true));
        fields.push(Field::new(RUN_ID, DataType::Utf8, true));
        fields.push(Field::new(
            EXTRACTED_AT,
            DataType::Timestamp(TimeUnit::Microsecond, Some("+00:00".into())),
            true,
        ));
        Arc::new(Schema::new(fields))
    }

    /// Checks one record. Returns the row to write (declared fields in Arrow
    /// friendly form, undeclared ones in `_apisip_extra`) and the violations.
    pub fn conform(&self, record: &Value) -> Conformed {
        let mut ctx = Ctx {
            lenient: self.source == SchemaSource::Inferred,
            cast: self.cast,
            ..Ctx::default()
        };
        let record = wrap(record.clone());
        let mut row = Map::new();
        let obj = record.as_object().expect("wrapped");
        for (name, shape) in &self.fields {
            if name.starts_with("_apisip_") {
                continue;
            }
            match obj.get(name) {
                None => ctx.absent.push(name.clone()),
                Some(v) => {
                    let out = conform_value(v, shape, name, &mut ctx);
                    if !out.is_null() {
                        row.insert(name.clone(), out);
                    }
                }
            }
        }
        for (k, v) in obj {
            if !self.fields.iter().any(|(n, _)| n == k) && !k.starts_with("_apisip_") {
                ctx.extra.insert(k.clone(), v.clone());
            }
        }
        if !ctx.extra.is_empty() {
            row.insert(
                EXTRA.into(),
                Value::String(Value::Object(ctx.extra.clone()).to_string()),
            );
        }
        Conformed {
            row,
            extra: ctx.extra,
            violations: ctx.violations,
            absent: ctx.absent,
            casts: ctx.casts,
        }
    }
}

/// Records that are not objects are stored under a `value` field.
pub fn wrap(record: Value) -> Value {
    match record {
        Value::Object(_) => record,
        other => Value::Object(Map::from_iter([("value".to_string(), other)])),
    }
}

#[derive(Debug)]
pub struct Conformed {
    pub row: Map<String, Value>,
    /// Undeclared fields by path.
    pub extra: Map<String, Value>,
    pub violations: Vec<Violation>,
    /// Declared top-level fields absent from the record.
    pub absent: Vec<String>,
    /// Values converted to the declared type without loss.
    pub casts: Vec<Cast>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cast {
    pub path: String,
    pub from: &'static str,
    pub to: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Violation {
    pub path: String,
    pub expected: String,
    pub value: Value,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: expected {}, got {}",
            self.path,
            self.expected,
            short(&self.value)
        )
    }
}

#[derive(Default)]
struct Ctx {
    lenient: bool,
    cast: bool,
    casts: Vec<Cast>,
    extra: Map<String, Value>,
    violations: Vec<Violation>,
    absent: Vec<String>,
}

fn conform_value(v: &Value, shape: &Shape, path: &str, ctx: &mut Ctx) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    // Largest integer an f64 holds exactly.
    const EXACT: u64 = 1 << 53;
    let mut cast_from = None;
    let ok = match (shape, v) {
        (Shape::Json, v) => Some(Value::String(v.to_string())),
        // Lossless casts: only when the value reads back the same.
        (Shape::Int, Value::String(s)) if ctx.cast => {
            cast_from = Some("string");
            s.parse::<i64>()
                .ok()
                .filter(|n| n.to_string() == *s)
                .map(Value::from)
        }
        (Shape::Float, Value::String(s)) if ctx.cast => {
            cast_from = Some("string");
            // Trailing zeros aside, the double must print back the same
            // digits: "12345678901234567890" would come out rounded.
            let digits = match s.contains('.') && !s.contains(['e', 'E']) {
                true => s.trim_end_matches('0').trim_end_matches('.'),
                false => s.as_str(),
            };
            s.parse::<f64>()
                .ok()
                .filter(|f| f.is_finite() && f.to_string() == digits)
                .map(Value::from)
        }
        (Shape::Bool, Value::String(s)) if ctx.cast => {
            cast_from = Some("string");
            match s.as_str() {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            }
        }
        (Shape::Str, Value::Number(_) | Value::Bool(_)) if ctx.cast => {
            cast_from = Some(if v.is_number() { "number" } else { "boolean" });
            Some(Value::String(v.to_string()))
        }
        (Shape::Int, Value::Number(n)) => n
            .as_i64()
            .or_else(|| {
                n.as_f64()
                    .filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15)
                    .map(|f| f as i64)
            })
            .map(Value::from),
        // An integer too large for a double would come out rounded.
        (Shape::Float, Value::Number(n)) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) if i.unsigned_abs() > EXACT => None,
            (None, Some(u)) if u > EXACT => None,
            _ => n.as_f64().map(Value::from),
        },
        (Shape::Bool, Value::Bool(_)) => Some(v.clone()),
        (Shape::Str, Value::String(_)) => Some(v.clone()),
        (Shape::Str, other) if ctx.lenient => Some(Value::String(match other {
            Value::String(s) => s.clone(),
            o => o.to_string(),
        })),
        (Shape::Timestamp, Value::String(s)) => {
            parse_timestamp(s).map(|t| Value::from(t.timestamp_micros()))
        }
        (Shape::Date, Value::String(s)) => NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .ok()
            .map(|_| v.clone()),
        (Shape::Struct(fields), Value::Object(obj)) => {
            let mut out = Map::new();
            for (name, s) in fields {
                if let Some(child) = obj.get(name) {
                    let c = conform_value(child, s, &format!("{path}.{name}"), ctx);
                    if !c.is_null() {
                        out.insert(name.clone(), c);
                    }
                }
            }
            for (k, child) in obj {
                if !fields.iter().any(|(n, _)| n == k) {
                    ctx.extra.insert(format!("{path}.{k}"), child.clone());
                }
            }
            Some(Value::Object(out))
        }
        (Shape::List(item), Value::Array(items)) => Some(Value::Array(
            items
                .iter()
                .enumerate()
                .map(|(i, x)| conform_value(x, item, &format!("{path}[{i}]"), ctx))
                .collect(),
        )),
        _ => None,
    };
    if ok.is_some()
        && let Some(from) = cast_from
    {
        ctx.casts.push(Cast {
            path: path.to_string(),
            from,
            to: shape.label(),
        });
    }
    ok.unwrap_or_else(|| {
        ctx.violations.push(Violation {
            path: path.to_string(),
            expected: shape.label().to_string(),
            value: v.clone(),
        });
        Value::Null
    })
}

pub fn parse_timestamp(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(s, f).ok())
        .map(|n| n.and_utc())
}

fn infer_value(v: &Value) -> Shape {
    match v {
        Value::Null => Shape::Json,
        Value::Bool(_) => Shape::Bool,
        Value::Number(n) if n.is_i64() || n.is_u64() => Shape::Int,
        Value::Number(_) => Shape::Float,
        Value::String(_) => Shape::Str,
        Value::Array(items) => Shape::List(Box::new(
            items
                .iter()
                .map(infer_value)
                .reduce(merge)
                .unwrap_or(Shape::Json),
        )),
        Value::Object(o) if o.is_empty() => Shape::Json,
        Value::Object(o) => {
            Shape::Struct(o.iter().map(|(k, v)| (k.clone(), infer_value(v))).collect())
        }
    }
}

/// `Json` doubles as "unknown so far" during inference (from nulls and empty containers).
fn merge(a: Shape, b: Shape) -> Shape {
    match (a, b) {
        (a, b) if a == b => a,
        (Shape::Json, b) => b,
        (a, Shape::Json) => a,
        (Shape::Int, Shape::Float) | (Shape::Float, Shape::Int) => Shape::Float,
        (Shape::Struct(mut fa), Shape::Struct(fb)) => {
            for (name, sb) in fb {
                match fa.iter().position(|(n, _)| *n == name) {
                    Some(i) => {
                        let sa = std::mem::replace(&mut fa[i].1, Shape::Json);
                        fa[i].1 = merge(sa, sb);
                    }
                    None => fa.push((name, sb)),
                }
            }
            Shape::Struct(fa)
        }
        (Shape::List(a), Shape::List(b)) => Shape::List(Box::new(merge(*a, *b))),
        _ => Shape::Str,
    }
}

/// Leftover "unknown" shapes become strings once inference is done.
fn finalize(s: Shape) -> Shape {
    match s {
        Shape::Json => Shape::Str,
        Shape::Struct(f) => Shape::Struct(f.into_iter().map(|(n, s)| (n, finalize(s))).collect()),
        Shape::List(i) => Shape::List(Box::new(match *i {
            Shape::Json => Shape::Json,
            other => finalize(other),
        })),
        other => other,
    }
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 80 {
        format!("{}…", s.chars().take(80).collect::<String>())
    } else {
        s
    }
}

/// Per-stream drift against the spec, for the manifest and the run summary.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Drift {
    /// Fields the spec does not declare: count and one example.
    pub undeclared: BTreeMap<String, FieldStat>,
    /// Values whose type does not match the spec.
    pub mistyped: BTreeMap<String, Mistyped>,
    /// Declared top-level fields that no record of the run carried.
    #[serde(default)]
    pub missing: Vec<String>,
    /// Values converted without loss, by `field: from -> to`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub casts: BTreeMap<String, u64>,
    #[serde(skip)]
    seen: BTreeSet<String>,
    #[serde(skip)]
    declared: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldStat {
    pub count: u64,
    pub example: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mistyped {
    pub count: u64,
    pub expected: String,
    pub example: Value,
}

impl Drift {
    pub fn new(schema: &RecordSchema) -> Drift {
        Drift {
            declared: schema.fields.iter().map(|(n, _)| n.clone()).collect(),
            ..Drift::default()
        }
    }

    pub fn observe(&mut self, c: &Conformed) {
        for (path, v) in &c.extra {
            let e = self
                .undeclared
                .entry(generic_path(path))
                .or_insert_with(|| FieldStat {
                    count: 0,
                    example: v.clone(),
                });
            e.count += 1;
        }
        for viol in &c.violations {
            let e = self
                .mistyped
                .entry(generic_path(&viol.path))
                .or_insert_with(|| Mistyped {
                    count: 0,
                    expected: viol.expected.clone(),
                    example: viol.value.clone(),
                });
            e.count += 1;
        }
        for cast in &c.casts {
            let key = format!("{}: {} -> {}", generic_path(&cast.path), cast.from, cast.to);
            *self.casts.entry(key).or_default() += 1;
        }
        for name in &self.declared {
            if !c.absent.contains(name) {
                self.seen.insert(name.clone());
            }
        }
    }

    /// Call once at the end of the stream.
    pub fn finish(&mut self, records: u64) {
        if records > 0 {
            self.missing = self
                .declared
                .iter()
                .filter(|n| !self.seen.contains(*n))
                .cloned()
                .collect();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.undeclared.is_empty()
            && self.mistyped.is_empty()
            && self.missing.is_empty()
            && self.casts.is_empty()
    }

    pub fn summary(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (p, s) in &self.undeclared {
            out.push(format!(
                "undeclared field {p} ({} records), e.g. {}",
                s.count,
                short(&s.example)
            ));
        }
        for (p, m) in &self.mistyped {
            out.push(format!(
                "{p}: expected {}, got {} ({} records)",
                m.expected,
                short(&m.example),
                m.count
            ));
        }
        for p in &self.missing {
            out.push(format!("declared field {p} never present"));
        }
        for (c, n) in &self.casts {
            out.push(format!("{c}, cast without loss ({n} records)"));
        }
        out
    }
}

/// `items[3].id` → `items[].id`, so drift aggregates across list elements.
fn generic_path(p: &str) -> String {
    static IDX: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\[\d+\]").unwrap());
    IDX.replace_all(p, "[]").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user_schema() -> RecordSchema {
        RecordSchema::from_spec(&json!({
            "type": "object",
            "properties": {
                "id": {"type": "integer"},
                "name": {"type": "string"},
                "score": {"type": "number"},
                "active": {"type": "boolean"},
                "updated_at": {"type": "string", "format": "date-time"},
                "birthday": {"type": "string", "format": "date"},
                "role": {"type": "string", "enum": ["admin", "user"]},
                "address": {"type": "object", "properties": {"city": {"type": "string"}}},
                "tags": {"type": "array", "items": {"type": "string"}},
                "meta": {"type": "object"},
                "ref": {"oneOf": [{"type": "string"}, {"type": "integer"}]}
            }
        }))
        .unwrap()
    }

    #[test]
    fn spec_types_map_to_arrow() {
        let s = user_schema().arrow();
        let ty = |n: &str| s.field_with_name(n).unwrap().data_type().clone();
        assert_eq!(ty("id"), DataType::Int64);
        assert_eq!(ty("score"), DataType::Float64);
        assert_eq!(ty("active"), DataType::Boolean);
        assert!(matches!(
            ty("updated_at"),
            DataType::Timestamp(TimeUnit::Microsecond, Some(_))
        ));
        assert_eq!(ty("birthday"), DataType::Date32);
        assert_eq!(ty("role"), DataType::Utf8);
        assert!(matches!(ty("address"), DataType::Struct(_)));
        assert!(matches!(ty("tags"), DataType::List(_)));
        assert_eq!(ty("meta"), DataType::Utf8);
        assert_eq!(ty("ref"), DataType::Utf8);
        assert_eq!(ty(EXTRA), DataType::Utf8);
        assert!(s.fields().iter().all(|f| f.is_nullable()));
    }

    #[test]
    fn conform_keeps_undeclared_and_flags_mistyped() {
        let c = user_schema().conform(&json!({
            "id": 1, "name": "Ada", "updated_at": "2026-01-01T00:00:00Z",
            "address": {"city": "Paris", "zip": "75001"},
            "nickname": "ada", "meta": {"k": 1}
        }));
        assert!(c.violations.is_empty(), "{:?}", c.violations);
        assert_eq!(c.row["updated_at"], json!(1767225600000000i64));
        assert_eq!(c.row["meta"], json!("{\"k\":1}"));
        let extra: Value = serde_json::from_str(c.row[EXTRA].as_str().unwrap()).unwrap();
        assert_eq!(extra, json!({"nickname": "ada", "address.zip": "75001"}));

        let c = user_schema().conform(&json!({"id": "12x", "tags": ["a", {"k": 1}]}));
        let paths: Vec<_> = c.violations.iter().map(|v| v.path.as_str()).collect();
        assert_eq!(paths, vec!["id", "tags[1]"]);
    }

    #[test]
    fn inference_widens() {
        let s = RecordSchema::infer(&[
            json!({"a": 1, "b": "x", "c": null, "d": {"e": 1}}),
            json!({"a": 1.5, "b": 2, "d": {"e": 2, "f": true}}),
        ]);
        assert_eq!(s.source, SchemaSource::Inferred);
        let f: BTreeMap<_, _> = s.fields.iter().cloned().collect();
        assert_eq!(f["a"], Shape::Float);
        assert_eq!(f["b"], Shape::Str);
        assert_eq!(f["c"], Shape::Str);
        assert_eq!(
            f["d"],
            Shape::Struct(vec![("e".into(), Shape::Int), ("f".into(), Shape::Bool)])
        );
        // Widened to string: the number is stored as text, not quarantined.
        let c = s.conform(&json!({"b": 3}));
        assert!(c.violations.is_empty());
        assert_eq!(c.row["b"], json!("3"));
    }

    #[test]
    fn drift_aggregates() {
        let schema = user_schema();
        let mut d = Drift::new(&schema);
        for r in [json!({"id": 1, "x": 1}), json!({"id": "two", "x": 2})] {
            d.observe(&schema.conform(&r));
        }
        d.finish(2);
        assert_eq!(d.undeclared["x"].count, 2);
        assert_eq!(d.mistyped["id"].count, 1);
        d.observe(&schema.conform(&json!({"id": "3", "score": "1.5"})));
        assert_eq!(d.casts["id: string -> integer"], 1);
        assert_eq!(d.casts["score: string -> number"], 1);
        assert!(d.missing.contains(&"name".to_string()) && !d.missing.contains(&"id".to_string()));
    }

    #[test]
    fn an_inferred_schema_file_reads_back_the_same() {
        let records = vec![json!({
            "id": 1, "price": 2.5, "ok": true, "at": "2026-01-01T00:00:00Z",
            "day": "2026-01-01", "tags": ["a"], "address": {"city": "Lille"}, "any": null
        })];
        let inferred = RecordSchema::infer(&records);
        let back = RecordSchema::from_file(&inferred.to_file()).unwrap();
        assert_eq!(back.fields, inferred.fields);
        assert_eq!(back.source, SchemaSource::Inferred);
    }

    #[test]
    fn only_casts_that_read_back_the_same_apply() {
        let schema = user_schema();
        let row = |r: Value| {
            let c = schema.conform(&r);
            (c.row, c.violations.len(), c.casts.len())
        };
        let (r, bad, casts) = row(json!({"id": "123", "score": "2.5", "active": "true"}));
        assert_eq!((bad, casts), (0, 3));
        assert_eq!(
            (&r["id"], &r["score"], &r["active"]),
            (&json!(123), &json!(2.5), &json!(true))
        );
        let (r, bad, casts) = row(json!({"name": 42, "role": false}));
        assert_eq!((bad, casts), (0, 2));
        assert_eq!((&r["name"], &r["role"]), (&json!("42"), &json!("false")));

        // Each of these would change the value: quarantined, not cast.
        for r in [
            json!({"id": "007"}),
            json!({"id": "1.0"}),
            json!({"id": " 1"}),
            json!({"score": "NaN"}),
            json!({"score": "1e999"}),
            json!({"score": "12345678901234567890"}),
            json!({"score": "0.30000000000000000001"}),
            json!({"score": 9007199254740993u64}),
            json!({"active": "yes"}),
            json!({"updated_at": 1767225600}),
        ] {
            assert_eq!(row(r.clone()).1, 1, "{r}");
        }
        assert_eq!(
            row(json!({"score": 9007199254740992u64})).1,
            0,
            "2^53 is exact"
        );
        for score in ["2.50", "-0.125", "3"] {
            assert_eq!(row(json!({"score": score})).1, 0, "{score}");
        }

        let strict = RecordSchema {
            cast: false,
            ..user_schema()
        };
        let c = strict.conform(&json!({"id": "123", "name": 42}));
        assert_eq!(c.violations.len(), 2);
        assert!(c.casts.is_empty());
    }
}
