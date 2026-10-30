//! One endpoint as a stream: where its records sit, how it pages, and how it
//! can be fetched incrementally.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::DetectedStream;
use super::names::norm;
use crate::config::{Incremental, Pagination, Sort, StreamConfig};
use crate::spec::{Operation, Spec, schema_type};

pub(super) fn detect_stream(spec: &Spec, op: &Operation) -> Result<DetectedStream, String> {
    if let Some(reason) = spec.no_json_body(op.op) {
        return Err(reason.to_string());
    }
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
    let pagination = detect_pagination(spec, op, schema.as_ref(), &records, &headers);
    let incremental = detect_incremental(spec, op, &fields, record_schema.as_ref());

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
        let default = spec.resolve(p.get("schema").unwrap_or(p)).get("default");
        match default {
            Some(Value::String(s)) if s.contains("${") => {
                return Err(format!(
                    "query parameter `{name}` has a default with `${{`, which apisip would fill from the environment"
                ));
            }
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
            cast: Default::default(),
        },
        method: op.method,
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

/// Whether the spec's response schema for a stream hit the inlining budget.
pub fn schema_truncated(spec: &Spec, stream: &StreamConfig) -> bool {
    spec.operation("get", &stream.path)
        .and_then(|op| spec.response_schema_bounded(op.op))
        .is_some_and(|(_, cut)| cut)
}

fn escape_pointer(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

fn unescape_pointer(s: &str) -> String {
    s.replace("~1", "/").replace("~0", "~")
}

struct QueryParam<'a> {
    norm: String,
    name: &'a str,
    schema: &'a Value,
    items: Option<&'a Value>,
    description: &'a str,
}

fn query_params<'a>(spec: &'a Spec, op: &Operation<'a>) -> Vec<QueryParam<'a>> {
    op.query_params()
        .filter_map(|p| {
            let name = p.get("name")?.as_str()?;
            let schema = spec.resolve(p.get("schema").unwrap_or(p));
            Some(QueryParam {
                norm: norm(name),
                name,
                schema,
                items: schema.get("items").map(|i| spec.resolve(i)),
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
    spec: &Spec,
    op: &Operation,
    schema: Option<&Value>,
    records: &str,
    headers: &[(String, &Value)],
) -> Option<Pagination> {
    let params = query_params(spec, op);
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
    spec: &Spec,
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
    let params = query_params(spec, op);

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

fn enum_values(p: &QueryParam) -> Vec<String> {
    let e = p
        .schema
        .get("enum")
        .or_else(|| p.items.and_then(|i| i.get("enum")));
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
                enum_values(p)
                    .into_iter()
                    .find(|v| v.to_ascii_lowercase().starts_with("desc"))
                    .map(|v| (p.name.to_string(), v))
            });
        let values = enum_values(sp);
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
