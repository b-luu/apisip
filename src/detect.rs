//! Turns a spec into stream definitions: which endpoints return data, how they
//! page, how they can be fetched incrementally, how to authenticate, and how
//! fast they may be called.
//!
//! Detection is best-effort. It writes plain TOML that the user can correct,
//! and `apisip check` reports what does not work.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

use crate::config::{
    ApiKeyLocation, AuthConfig, Incremental, Pagination, RateLimit, Sort, StreamConfig,
};
use crate::spec::{Operation, Spec, schema_type};

#[derive(Debug, Clone)]
pub struct Detection {
    pub title: String,
    pub env_prefix: String,
    pub base_url: Option<String>,
    pub auth: Vec<AuthOption>,
    pub rate_limits: RateLimitFinding,
    pub streams: Vec<DetectedStream>,
    pub skipped: Vec<Skipped>,
}

#[derive(Debug, Clone)]
pub struct AuthOption {
    /// Scheme name in the spec.
    pub scheme: String,
    pub description: String,
    /// `None` when apisip v1 cannot use this scheme.
    pub config: Option<AuthConfig>,
    pub unsupported: Option<String>,
    /// Variables the config references, with whether each one is a secret.
    pub vars: Vec<EnvVar>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub name: String,
    pub label: String,
    pub secret: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RateLimitFinding {
    pub tiers: Vec<Tier>,
    /// The spec declares rate-limit response headers: the API reports its budget at runtime.
    pub headers_declared: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tier {
    pub plan: Option<String>,
    pub limit: RateLimit,
    pub source: String,
}

impl RateLimitFinding {
    /// Distinct named plans, when the spec documents more than one.
    pub fn plans(&self) -> Vec<&Tier> {
        let mut seen = BTreeSet::new();
        let named: Vec<&Tier> = self
            .tiers
            .iter()
            .filter(|t| {
                t.plan
                    .as_ref()
                    .is_some_and(|p| seen.insert(p.to_lowercase()))
            })
            .collect();
        if named.len() >= 2 { named } else { Vec::new() }
    }

    /// The single limit to use when no plan choice is needed: the most conservative one.
    pub fn default_limit(&self) -> Option<RateLimit> {
        self.tiers
            .iter()
            .map(|t| t.limit.clone())
            .min_by(|a, b| a.per_second().total_cmp(&b.per_second()))
    }
}

#[derive(Debug, Clone)]
pub struct DetectedStream {
    pub config: StreamConfig,
    pub method: &'static str,
    /// Inlined schema of one record, when the spec declares it.
    pub record_schema: Option<Value>,
    /// Top-level record fields, for the primary-key prompt.
    pub fields: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Skipped {
    pub path: String,
    pub reason: String,
}

pub fn detect(spec: &Spec) -> Detection {
    let env_prefix = env_prefix(spec.title());
    let mut streams: Vec<DetectedStream> = Vec::new();
    let mut skipped = Vec::new();
    let mut names = BTreeSet::new();

    for op in spec.operations() {
        if op.method != "get" {
            continue;
        }
        if op.path.contains('{') {
            skipped.push(Skipped {
                path: op.path.to_string(),
                reason: "needs a parent stream (path parameters)".into(),
            });
            continue;
        }
        match detect_stream(spec, &op) {
            Ok(mut s) => {
                let mut name = stream_name(op.path);
                if !names.insert(name.clone()) {
                    name = op
                        .op
                        .get("operationId")
                        .and_then(Value::as_str)
                        .map(snake)
                        .filter(|n| !names.contains(n))
                        .unwrap_or_else(|| format!("{name}_{}", names.len()));
                    names.insert(name.clone());
                }
                s.config.name = name;
                streams.push(s);
            }
            Err(reason) => skipped.push(Skipped {
                path: op.path.to_string(),
                reason,
            }),
        }
    }

    Detection {
        title: spec.title().to_string(),
        auth: detect_auth(spec, &env_prefix),
        env_prefix,
        base_url: spec.base_url(),
        rate_limits: detect_rate_limits(spec),
        streams,
        skipped,
    }
}

fn detect_stream(spec: &Spec, op: &Operation) -> Result<DetectedStream, String> {
    let schema = spec.response_schema(op.op);
    let (records, record_schema) = match &schema {
        Some(s) => records_location(s).ok_or("returns a single object, not a list")?,
        None => (String::new(), None),
    };
    let record_schema = record_schema.filter(|s| s.get("properties").is_some());
    let fields: Vec<String> = record_schema
        .as_ref()
        .and_then(|s| s.get("properties"))
        .and_then(Value::as_object)
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default();

    let headers = spec.response_headers(op.op);
    let pagination = detect_pagination(op, schema.as_ref(), &records, &headers);
    let incremental = detect_incremental(op, &fields, record_schema.as_ref());

    let mut used: BTreeSet<String> = BTreeSet::new();
    match &pagination {
        Some(Pagination::PageNumber {
            param, size_param, ..
        }) => {
            used.insert(param.clone());
            used.extend(size_param.clone());
        }
        Some(Pagination::Offset {
            param, limit_param, ..
        }) => {
            used.insert(param.clone());
            used.insert(limit_param.clone());
        }
        Some(Pagination::Cursor {
            param, size_param, ..
        }) => {
            used.insert(param.clone());
            used.extend(size_param.clone());
        }
        _ => {}
    }
    if let Some(inc) = &incremental {
        used.extend(inc.param.clone());
        if let Some(sort) = &inc.sort {
            used.insert(sort.param.clone());
            used.extend(sort.params.keys().cloned());
        }
    }

    let mut params = BTreeMap::new();
    for p in op.query_params() {
        let Some(name) = p.get("name").and_then(Value::as_str) else {
            continue;
        };
        if used.contains(name) || !p.get("required").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let default = p.get("schema").unwrap_or(p).get("default");
        match default {
            Some(Value::String(s)) => {
                params.insert(name.to_string(), s.clone());
            }
            Some(v @ (Value::Number(_) | Value::Bool(_))) => {
                params.insert(name.to_string(), v.to_string());
            }
            _ => return Err(format!("requires query parameter `{name}`")),
        }
    }

    let primary_key = fields
        .iter()
        .find(|f| f.eq_ignore_ascii_case("id") || f.as_str() == "_id")
        .map(|f| vec![f.clone()])
        .unwrap_or_default();

    Ok(DetectedStream {
        config: StreamConfig {
            name: String::new(),
            path: op.path.to_string(),
            enabled: true,
            records,
            primary_key,
            params,
            pagination,
            incremental,
        },
        method: op.method,
        record_schema,
        fields,
    })
}

/// Where the record array sits in a response: the body itself, or one array
/// property of a wrapping object. Returns the JSON pointer and the item schema.
pub fn records_location(schema: &Value) -> Option<(String, Option<Value>)> {
    if schema_type(schema) == Some("array") {
        return Some((String::new(), schema.get("items").cloned()));
    }
    let props = schema.get("properties")?.as_object()?;
    let arrays: Vec<(&String, &Value)> = props
        .iter()
        .filter(|(_, v)| schema_type(v) == Some("array"))
        .collect();
    let pick = match arrays.as_slice() {
        [] => None,
        [one] => Some(*one),
        many => {
            const PREFERRED: [&str; 9] = [
                "data", "items", "results", "records", "entries", "values", "list", "content",
                "objects",
            ];
            many.iter()
                .find(|(k, _)| PREFERRED.contains(&k.to_ascii_lowercase().as_str()))
                .or_else(|| many.first())
                .copied()
        }
    }?;
    Some((
        format!("/{}", escape_pointer(pick.0)),
        pick.1.get("items").cloned(),
    ))
}

/// The record schema of a configured stream, looked up again in the spec.
pub fn stream_record_schema(spec: &Spec, stream: &StreamConfig) -> Option<Value> {
    let op = spec.operation("get", &stream.path)?;
    let schema = spec.response_schema(op.op)?;
    let item = if stream.records.is_empty() {
        if schema_type(&schema) == Some("array") {
            schema.get("items").cloned()
        } else {
            records_location(&schema).and_then(|(_, s)| s)
        }
    } else {
        let mut s = &schema;
        for part in stream.records.trim_start_matches('/').split('/') {
            s = s.get("properties")?.get(unescape_pointer(part))?;
        }
        s.get("items").cloned()
    }?;
    item.get("properties").is_some().then_some(item)
}

fn escape_pointer(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

fn unescape_pointer(s: &str) -> String {
    s.replace("~1", "/").replace("~0", "~")
}

fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '$')
        .collect::<String>()
        .to_ascii_lowercase()
}

struct QueryParam<'a> {
    norm: String,
    name: &'a str,
    schema: &'a Value,
    description: &'a str,
}

fn query_params<'a>(op: &'a Operation) -> Vec<QueryParam<'a>> {
    op.query_params()
        .filter_map(|p| {
            let name = p.get("name")?.as_str()?;
            Some(QueryParam {
                norm: norm(name),
                name,
                schema: p.get("schema").unwrap_or(p),
                description: p.get("description").and_then(Value::as_str).unwrap_or(""),
            })
        })
        .collect()
}

fn find_param<'a, 'b>(params: &'b [QueryParam<'a>], names: &[&str]) -> Option<&'b QueryParam<'a>> {
    names
        .iter()
        .find_map(|n| params.iter().find(|p| p.norm == *n))
}

fn page_size(p: &QueryParam) -> u32 {
    let max = p.schema.get("maximum").and_then(Value::as_u64);
    let default = p.schema.get("default").and_then(Value::as_u64);
    match (max, default) {
        (Some(m), _) => m.min(1000) as u32,
        (None, Some(d)) => d.clamp(100, 1000) as u32,
        (None, None) => 100,
    }
}

const SIZE_PARAMS: [&str; 14] = [
    "perpage",
    "pagesize",
    "limit",
    "size",
    "count",
    "pagelen",
    "pagelength",
    "maxresults",
    "max",
    "top",
    "$top",
    "take",
    "rows",
    "pagelimit",
];

fn detect_pagination(
    op: &Operation,
    schema: Option<&Value>,
    records: &str,
    headers: &[(String, Value)],
) -> Option<Pagination> {
    let params = query_params(op);
    let size = find_param(&params, &SIZE_PARAMS);
    let records_key = records.trim_start_matches('/');

    // Cursor: a token parameter, fed from the response body or the last record.
    const CURSOR_PARAMS: [&str; 15] = [
        "cursor",
        "pagetoken",
        "nextpagetoken",
        "nexttoken",
        "startingafter",
        "continuation",
        "continuationtoken",
        "pagecursor",
        "next",
        "after",
        "marker",
        "nextcursor",
        "token",
        "startcursor",
        "scrollid",
    ];
    if let Some(p) = find_param(&params, &CURSOR_PARAMS) {
        const NEXT_FIELDS: [&str; 14] = [
            "nextcursor",
            "nextpagetoken",
            "nexttoken",
            "cursor",
            "next",
            "continuation",
            "continuationtoken",
            "endcursor",
            "nextmarker",
            "after",
            "nextpage",
            "marker",
            "startcursor",
            "scrollid",
        ];
        let cursor_path = schema.and_then(|s| find_field(s, &NEXT_FIELDS, records_key, 3));
        let last_record_field =
            (cursor_path.is_none() && p.norm == "startingafter").then(|| "id".to_string());
        if cursor_path.is_some() || last_record_field.is_some() {
            return Some(Pagination::Cursor {
                param: p.name.to_string(),
                cursor_path,
                last_record_field,
                size_param: size.map(|s| s.name.to_string()),
                size: size.map(page_size),
            });
        }
    }

    const PAGE_PARAMS: [&str; 6] = ["page", "pagenumber", "pageno", "p", "pageindex", "pagenum"];
    if let Some(p) = find_param(&params, &PAGE_PARAMS) {
        let start = match (
            p.schema.get("minimum").and_then(Value::as_u64),
            p.schema.get("default").and_then(Value::as_u64),
        ) {
            (Some(0), _) | (None, Some(0)) => 0,
            _ => 1,
        };
        let size = size.filter(|s| s.norm != "limit" || find_param(&params, &["offset"]).is_none());
        return Some(Pagination::PageNumber {
            param: p.name.to_string(),
            size_param: size.map(|s| s.name.to_string()),
            size: size.map(page_size),
            start,
        });
    }

    const OFFSET_PARAMS: [&str; 5] = ["offset", "skip", "$skip", "startindex", "first"];
    if let (Some(p), Some(limit)) = (find_param(&params, &OFFSET_PARAMS), size) {
        return Some(Pagination::Offset {
            param: p.name.to_string(),
            limit_param: limit.name.to_string(),
            limit: page_size(limit),
        });
    }

    const NEXT_URL_FIELDS: [&str; 9] = [
        "next",
        "nexturl",
        "nextpageurl",
        "nextlink",
        "@odatanextlink",
        "odatanextlink",
        "nextpage",
        "nexthref",
        "href",
    ];
    if let Some(s) = schema
        && let Some(path) = find_next_url(s, &NEXT_URL_FIELDS, records_key)
    {
        return Some(Pagination::NextUrl { next_path: path });
    }

    if headers.iter().any(|(h, _)| h == "link") {
        return Some(Pagination::LinkHeader);
    }
    None
}

/// Finds a property whose normalized name is in `names`, searching nested
/// objects up to `depth`, skipping the record array. Returns a JSON pointer.
fn find_field(schema: &Value, names: &[&str], skip: &str, depth: usize) -> Option<String> {
    let props = schema.get("properties")?.as_object()?;
    for n in names {
        if let Some((k, _)) = props
            .iter()
            .find(|(k, _)| k.as_str() != skip && norm(k) == *n)
        {
            return Some(format!("/{}", escape_pointer(k)));
        }
    }
    if depth == 0 {
        return None;
    }
    for (k, v) in props {
        if k == skip || schema_type(v) != Some("object") {
            continue;
        }
        if let Some(inner) = find_field(v, names, "", depth - 1) {
            return Some(format!("/{}{inner}", escape_pointer(k)));
        }
    }
    None
}

/// Like `find_field`, restricted to string-typed properties (URLs), and
/// accepting `links.next` / `_links.next.href` shapes.
fn find_next_url(schema: &Value, names: &[&str], skip: &str) -> Option<String> {
    fn walk(
        schema: &Value,
        names: &[&str],
        skip: &str,
        depth: usize,
        under_next: bool,
    ) -> Option<String> {
        let props = schema.get("properties")?.as_object()?;
        for (k, v) in props {
            if k == skip {
                continue;
            }
            let n = norm(k);
            let named = names.contains(&n.as_str()) && (n != "href" || under_next);
            if named && schema_type(v) == Some("string") {
                return Some(format!("/{}", escape_pointer(k)));
            }
            if depth > 0 && schema_type(v) == Some("object") {
                let next = under_next || n == "next";
                let parent = matches!(
                    n.as_str(),
                    "links" | "_links" | "paging" | "pagination" | "meta" | "next"
                );
                if (parent || next)
                    && let Some(inner) = walk(v, names, "", depth - 1, next)
                {
                    return Some(format!("/{}{inner}", escape_pointer(k)));
                }
            }
        }
        None
    }
    walk(schema, names, skip, 3, false)
}

const CURSOR_FIELDS: [&str; 22] = [
    "updatedat",
    "updated",
    "modifiedat",
    "modified",
    "lastmodified",
    "lastmodifiedat",
    "lastmodifieddate",
    "datemodified",
    "lastupdated",
    "lastupdatedat",
    "updatetime",
    "updatedtime",
    "changedat",
    "modificationdate",
    "systemmodstamp",
    "modifiedtime",
    "updatedon",
    "modifiedon",
    "updateddate",
    "modifieddate",
    "lastmodifiedtime",
    "updatedatutc",
];

fn detect_incremental(
    op: &Operation,
    fields: &[String],
    record_schema: Option<&Value>,
) -> Option<Incremental> {
    let cursor = CURSOR_FIELDS.iter().find_map(|c| {
        fields.iter().find(|f| {
            norm(f) == *c
                && record_schema
                    .and_then(|s| s.pointer(&format!("/properties/{}", escape_pointer(f))))
                    .and_then(schema_type)
                    .is_some_and(|t| matches!(t, "string" | "integer" | "number"))
        })
    })?;
    let params = query_params(op);

    let is_filter = |n: &str| {
        if n == "since" || n == "updatedsince" || n == "modifiedsince" {
            return true;
        }
        let topical = n.contains("updat") || n.contains("modif") || n.contains("chang");
        let lower_bound = [
            "since", "after", "from", "gte", "gt", "min", "start", "begin",
        ];
        topical
            && (lower_bound.iter().any(|b| n.ends_with(b))
                || ["min", "from", "start", "since", "after"]
                    .iter()
                    .any(|b| n.starts_with(b)))
            && !["before", "until", "lte", "lt", "max", "end", "to"]
                .iter()
                .any(|b| n.ends_with(b))
    };
    if let Some(p) = params.iter().find(|p| is_filter(&p.norm)) {
        return Some(Incremental {
            cursor: cursor.clone(),
            param: Some(p.name.to_string()),
            sort: None,
        });
    }

    Some(Incremental {
        cursor: cursor.clone(),
        param: None,
        sort: detect_sort(&params, cursor),
    })
}

fn enum_values(schema: &Value) -> Vec<String> {
    let e = schema
        .get("enum")
        .or_else(|| schema.get("items").and_then(|i| i.get("enum")));
    e.and_then(Value::as_array)
        .map(|vs| {
            vs.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn detect_sort(params: &[QueryParam], cursor: &str) -> Option<Sort> {
    const SORT_PARAMS: [&str; 8] = [
        "sort",
        "sortby",
        "orderby",
        "ordering",
        "sortfield",
        "order",
        "sortkey",
        "$orderby",
    ];
    const DIR_PARAMS: [&str; 8] = [
        "direction",
        "order",
        "sortorder",
        "sortdirection",
        "dir",
        "orderdirection",
        "sortdir",
        "sortorder",
    ];
    let cursor_n = norm(cursor);
    // `updated` matches `updated_at`; `modified` matches `last_modified`.
    let matches_cursor = |v: &str| {
        let n = norm(v.trim_start_matches(['-', '+']));
        let n = n.trim_end_matches("desc").trim_end_matches("asc");
        !n.is_empty() && (n == cursor_n || cursor_n.starts_with(n) || cursor_n.ends_with(n))
    };

    for sp in SORT_PARAMS
        .iter()
        .filter_map(|n| params.iter().find(|p| p.norm == *n))
    {
        let direction = DIR_PARAMS
            .iter()
            .filter_map(|n| params.iter().find(|p| p.norm == *n && p.name != sp.name))
            .find_map(|p| {
                enum_values(p.schema)
                    .into_iter()
                    .find(|v| v.to_ascii_lowercase().starts_with("desc"))
                    .map(|v| (p.name.to_string(), v))
            });
        let values = enum_values(sp.schema);
        if !values.is_empty() {
            let desc = values.iter().find(|v| {
                matches_cursor(v) && (v.starts_with('-') || v.to_ascii_lowercase().contains("desc"))
            });
            if let Some(v) = desc {
                return Some(Sort {
                    param: sp.name.to_string(),
                    value: v.clone(),
                    params: BTreeMap::new(),
                });
            }
            if let Some(v) = values
                .iter()
                .find(|v| matches_cursor(v) && !v.to_ascii_lowercase().contains("asc"))
            {
                if let Some((dir, dv)) = &direction {
                    return Some(Sort {
                        param: sp.name.to_string(),
                        value: v.clone(),
                        params: BTreeMap::from([(dir.clone(), dv.clone())]),
                    });
                }
                if values.iter().any(|o| o.starts_with('-')) {
                    return Some(Sort {
                        param: sp.name.to_string(),
                        value: format!("-{v}"),
                        params: BTreeMap::new(),
                    });
                }
            }
            continue;
        }
        // No enum: trust the description only when it spells the descending form.
        let d = sp.description;
        if d.contains(&format!("-{cursor}"))
            || (d.contains(cursor) && d.contains("prefix") && d.contains('-'))
        {
            return Some(Sort {
                param: sp.name.to_string(),
                value: format!("-{cursor}"),
                params: BTreeMap::new(),
            });
        }
        for form in [
            format!("{cursor}:desc"),
            format!("{cursor} desc"),
            format!("{cursor}_desc"),
        ] {
            if d.contains(&form) {
                return Some(Sort {
                    param: sp.name.to_string(),
                    value: form,
                    params: BTreeMap::new(),
                });
            }
        }
        if d.contains(cursor)
            && let Some((dir, dv)) = direction
        {
            return Some(Sort {
                param: sp.name.to_string(),
                value: cursor.to_string(),
                params: BTreeMap::from([(dir, dv)]),
            });
        }
    }
    None
}

fn detect_auth(spec: &Spec, prefix: &str) -> Vec<AuthOption> {
    let mut schemes = spec.security_schemes();
    let preferred = spec.default_security();
    schemes.sort_by_key(|(name, _)| {
        preferred
            .iter()
            .position(|p| p == name)
            .unwrap_or(usize::MAX)
    });

    let kind_of = |s: &Value| -> &'static str {
        let t = s.get("type").and_then(Value::as_str).unwrap_or("");
        let scheme = s
            .get("scheme")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        match (t, scheme.as_str()) {
            ("http", "bearer") => "bearer",
            ("http", "basic") | ("basic", _) => "basic",
            ("apiKey", _) => "api_key",
            ("oauth2", _) => "oauth2",
            _ => "other",
        }
    };
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, s) in &schemes {
        *counts.entry(kind_of(s)).or_default() += 1;
    }

    schemes
        .iter()
        .map(|(name, s)| {
            let kind = kind_of(s);
            let p = if counts.get(kind).copied().unwrap_or(0) > 1 {
                format!("{prefix}_{}", snake(name).to_ascii_uppercase())
            } else {
                prefix.to_string()
            };
            let var = |suffix: &str, label: &str, secret: bool| EnvVar {
                name: format!("{p}_{suffix}"),
                label: label.to_string(),
                secret,
            };
            let reference = |v: &EnvVar| format!("${{{}}}", v.name);
            let description = s
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            let scheme = Some(name.clone());
            let mut opt = AuthOption {
                scheme: name.clone(),
                description,
                config: None,
                unsupported: None,
                vars: Vec::new(),
            };
            match kind {
                "bearer" => {
                    let t = var("TOKEN", "Bearer token", true);
                    opt.config = Some(AuthConfig::Bearer {
                        scheme,
                        token: reference(&t),
                    });
                    opt.vars = vec![t];
                }
                "basic" => {
                    let u = var("USERNAME", "Username", false);
                    let pw = var("PASSWORD", "Password", true);
                    opt.config = Some(AuthConfig::Basic {
                        scheme,
                        username: reference(&u),
                        password: reference(&pw),
                    });
                    opt.vars = vec![u, pw];
                }
                "api_key" => {
                    let location = match s.get("in").and_then(Value::as_str) {
                        Some("query") => ApiKeyLocation::Query,
                        Some("cookie") => ApiKeyLocation::Cookie,
                        _ => ApiKeyLocation::Header,
                    };
                    let k = var("API_KEY", "API key", true);
                    opt.config = Some(AuthConfig::ApiKey {
                        scheme,
                        name: s
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("X-API-Key")
                            .to_string(),
                        location,
                        value: reference(&k),
                    });
                    opt.vars = vec![k];
                }
                "oauth2" => {
                    let flow = s.pointer("/flows/clientCredentials").or_else(|| {
                        (s.get("flow").and_then(Value::as_str) == Some("application")).then_some(s)
                    });
                    match flow
                        .and_then(|f| f.get("tokenUrl").and_then(Value::as_str).map(|u| (f, u)))
                    {
                        Some((flow, token_url)) => {
                            let id = var("CLIENT_ID", "OAuth2 client id", false);
                            let secret = var("CLIENT_SECRET", "OAuth2 client secret", true);
                            let scopes = flow
                                .get("scopes")
                                .and_then(Value::as_object)
                                .map(|m| m.keys().cloned().collect())
                                .unwrap_or_default();
                            opt.config = Some(AuthConfig::Oauth2ClientCredentials {
                                scheme,
                                token_url: token_url.to_string(),
                                client_id: reference(&id),
                                client_secret: reference(&secret),
                                scopes,
                            });
                            opt.vars = vec![id, secret];
                        }
                        None => {
                            opt.unsupported = Some(
                                "OAuth2 without a client-credentials flow is not supported yet"
                                    .into(),
                            );
                        }
                    }
                }
                _ => {
                    opt.unsupported = Some(format!(
                        "scheme type `{}` is not supported yet",
                        s.get("type").and_then(Value::as_str).unwrap_or("?")
                    ));
                }
            }
            opt
        })
        .collect()
}

static RATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(\d[\d,]*)\s*(?:api\s+)?(?:requests?|calls?|reqs?|hits?)\s*(?:per|/|an?|each|every)\s*(\d+\s*)?(seconds?|secs?|minutes?|mins?|hours?|hrs?|days?|s|m|h|d)\b",
    )
    .unwrap()
});

const RATE_HEADERS: [&str; 8] = [
    "x-ratelimit-limit",
    "x-rate-limit-limit",
    "ratelimit-limit",
    "ratelimit-policy",
    "ratelimit",
    "x-ratelimit-remaining",
    "ratelimit-remaining",
    "x-rate-limit-remaining",
];

pub fn detect_rate_limits(spec: &Spec) -> RateLimitFinding {
    let mut f = RateLimitFinding::default();

    // 1. Vendor extensions anywhere in the document.
    walk_extensions(&spec.root, 0, &mut f.tiers);

    // 2. Declared rate-limit response headers, on success and 429 responses.
    for op in spec.operations() {
        let mut headers = spec.response_headers(op.op);
        if let Some(r) = op.op.pointer("/responses/429") {
            let r = spec.resolve(r);
            if let Some(h) = r.get("headers").and_then(Value::as_object) {
                headers.extend(
                    h.iter()
                        .map(|(k, v)| (k.to_ascii_lowercase(), spec.inline(v))),
                );
            }
            if let Some(d) = r.get("description").and_then(Value::as_str) {
                parse_text(d, "429 response description", &mut f.tiers);
            }
        }
        for (name, h) in &headers {
            if RATE_HEADERS.contains(&name.as_str()) {
                f.headers_declared = true;
            }
            if let Some(d) = h.get("description").and_then(Value::as_str) {
                parse_text(d, &format!("header {name}"), &mut f.tiers);
            }
        }
    }

    // 3. Description text.
    if let Some(d) = spec
        .root
        .pointer("/info/description")
        .and_then(Value::as_str)
    {
        parse_text(d, "info.description", &mut f.tiers);
    }
    if f.tiers.is_empty() {
        for op in spec.operations() {
            parse_text(
                &op.description(),
                &format!("{} {}", op.method.to_uppercase(), op.path),
                &mut f.tiers,
            );
        }
    }

    let mut seen = Vec::new();
    f.tiers.retain(|t| {
        let key = (t.plan.clone().map(|p| p.to_lowercase()), t.limit.clone());
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    f
}

fn walk_extensions(v: &Value, depth: usize, out: &mut Vec<Tier>) {
    if depth > 6 {
        return;
    }
    match v {
        Value::Object(m) => {
            for (k, child) in m {
                let n = norm(k);
                if n.starts_with('x') && (n.contains("ratelimit") || n.contains("throttl")) {
                    parse_extension(child, None, k, out);
                } else {
                    walk_extensions(child, depth + 1, out);
                }
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|i| walk_extensions(i, depth + 1, out)),
        _ => {}
    }
}

fn parse_extension(v: &Value, plan: Option<String>, source: &str, out: &mut Vec<Tier>) {
    match v {
        Value::String(s) => {
            let before = out.len();
            parse_text(s, source, out);
            if let Some(p) = plan {
                for t in &mut out[before..] {
                    t.plan.get_or_insert(p.clone());
                }
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|i| parse_extension(i, plan.clone(), source, out)),
        Value::Object(m) => {
            let get = |keys: &[&str]| {
                m.iter()
                    .find(|(k, _)| keys.contains(&norm(k).as_str()))
                    .map(|(_, v)| v)
            };
            let plan = get(&["plan", "tier", "name", "label"])
                .and_then(Value::as_str)
                .map(str::to_string)
                .or(plan);
            for (k, child) in m {
                let n = norm(k);
                if let Some(unit) = n
                    .strip_prefix("requestsper")
                    .or_else(|| n.strip_prefix("callsper"))
                    .or_else(|| n.strip_prefix("per"))
                    && let (Some(period), Some(calls)) = (parse_period_word(unit), child.as_u64())
                {
                    out.push(tier(plan.clone(), calls, period, source));
                }
            }
            let calls = get(&[
                "limit",
                "max",
                "maxcalls",
                "maxrequests",
                "requests",
                "calls",
                "rate",
                "count",
                "quota",
            ])
            .and_then(Value::as_u64);
            let period = get(&[
                "period",
                "window",
                "interval",
                "per",
                "unit",
                "duration",
                "timewindow",
                "windowseconds",
                "seconds",
            ])
            .and_then(|p| match p {
                Value::String(s) => {
                    parse_period_word(&norm(s)).or_else(|| humantime::parse_duration(s).ok())
                }
                Value::Number(n) => n.as_u64().map(Duration::from_secs),
                _ => None,
            });
            if let (Some(c), Some(p)) = (calls, period) {
                out.push(tier(plan.clone(), c, p, source));
            }
            for (k, child) in m {
                if child.is_object() || child.is_array() {
                    let child_plan = if ["tiers", "plans", "limits"].contains(&norm(k).as_str()) {
                        plan.clone()
                    } else if child.is_object() && !k.starts_with('x') {
                        Some(k.clone())
                    } else {
                        plan.clone()
                    };
                    parse_extension(child, child_plan, source, out);
                }
            }
        }
        _ => {}
    }
}

fn tier(plan: Option<String>, calls: u64, period: Duration, source: &str) -> Tier {
    Tier {
        plan,
        limit: RateLimit {
            max_calls: calls.min(u32::MAX as u64) as u32,
            period,
        },
        source: source.to_string(),
    }
}

fn parse_period_word(w: &str) -> Option<Duration> {
    let w = w.trim().to_ascii_lowercase();
    let secs = match w.as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86400,
        _ => return None,
    };
    Some(Duration::from_secs(secs))
}

/// Finds "N requests per <unit>" in prose, with a plan name when the line names one.
fn parse_text(text: &str, source: &str, out: &mut Vec<Tier>) {
    for line in text.lines() {
        for m in RATE_RE.captures_iter(line) {
            let Ok(calls) = m[1].replace(',', "").parse::<u64>() else {
                continue;
            };
            let mult: u64 = m
                .get(2)
                .and_then(|g| g.as_str().trim().parse().ok())
                .unwrap_or(1);
            let Some(unit) = parse_period_word(&m[3]) else {
                continue;
            };
            let prefix = &line[..m.get(0).unwrap().start()];
            out.push(tier(
                plan_name(line, prefix),
                calls,
                unit * mult as u32,
                source,
            ));
        }
    }
}

fn plan_name(line: &str, prefix: &str) -> Option<String> {
    let cleaned: String = prefix
        .chars()
        .map(|c| {
            if matches!(c, '*' | '_' | '|' | '#' | '`' | ':' | '-' | '(' | ')') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    let lower = cleaned.to_lowercase();
    let listed = {
        let t = line.trim_start();
        t.starts_with(['-', '*', '|', '+'])
            || t.chars().next().is_some_and(|c| c.is_ascii_digit()) && t.contains(". ")
    };
    let named = lower.contains("plan") || lower.contains("tier");
    if words.is_empty() || words.len() > 4 || !(named || listed) {
        return None;
    }
    let name: Vec<&str> = words
        .into_iter()
        .filter(|w| {
            !matches!(
                w.to_lowercase().as_str(),
                "plan" | "tier" | "plans" | "tiers" | "the" | "on"
            )
        })
        .collect();
    if name.is_empty() || name.iter().any(|w| w.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    Some(name.join(" "))
}

/// `/v1/users/search` -> `users_search`.
pub fn stream_name(path: &str) -> String {
    let parts: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .filter(|s| {
            let l = s.to_ascii_lowercase();
            !(l == "api"
                || l == "rest"
                || (l.starts_with('v')
                    && l[1..].chars().all(|c| c.is_ascii_digit() || c == '.')
                    && l.len() > 1))
        })
        .map(snake)
        .collect();
    let name = parts.join("_");
    if name.is_empty() { "root".into() } else { name }
}

pub fn snake(s: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            if prev_lower {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
            prev_lower = false;
        } else if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        } else {
            if !out.ends_with('_') && !out.is_empty() {
                out.push('_');
            }
            prev_lower = false;
        }
    }
    out.trim_matches('_').to_string()
}

/// Env var prefix from the spec title: "Acme Billing REST API v2" -> "ACME_BILLING".
pub fn env_prefix(title: &str) -> String {
    const NOISE: [&str; 8] = [
        "api",
        "apis",
        "openapi",
        "swagger",
        "rest",
        "spec",
        "specification",
        "the",
    ];
    let words: Vec<String> = title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .filter(|w| !NOISE.contains(&w.to_ascii_lowercase().as_str()))
        .filter(|w| !w.chars().all(|c| c.is_ascii_digit()))
        .filter(|w| {
            let l = w.to_ascii_lowercase();
            !(l.starts_with('v') && l.len() > 1 && l[1..].chars().all(|c| c.is_ascii_digit()))
        })
        .take(2)
        .map(|w| w.to_ascii_uppercase())
        .collect();
    if words.is_empty() {
        "API".into()
    } else {
        words.join("_")
    }
}

#[cfg(test)]
mod tests;
