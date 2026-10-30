//! Loading OpenAPI 3.0 / 3.1 and Swagger 2.0 documents, and walking them.
//!
//! The spec is kept as a plain `serde_json::Value`: one walker covers every
//! version, and unknown vendor extensions survive untouched.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    Swagger2,
    OpenApi30,
    OpenApi31,
}

#[derive(Debug, Clone)]
pub struct Spec {
    pub root: Value,
    /// Where the spec came from: a URL or a file path. Used to resolve
    /// relative server URLs.
    pub origin: String,
}

#[derive(Debug, Clone)]
pub struct Operation<'a> {
    pub method: &'static str,
    pub path: &'a str,
    pub op: &'a Value,
    /// Path-level and operation-level parameters, refs resolved, operation-level winning.
    pub params: Vec<Value>,
}

impl Operation<'_> {
    pub fn param(&self, name: &str) -> Option<&Value> {
        self.params
            .iter()
            .find(|p| p.get("name").and_then(Value::as_str) == Some(name))
    }

    pub fn query_params(&self) -> impl Iterator<Item = &Value> {
        self.params
            .iter()
            .filter(|p| p.get("in").and_then(Value::as_str) == Some("query"))
    }

    pub fn description(&self) -> String {
        let s = |k: &str| self.op.get(k).and_then(Value::as_str).unwrap_or("");
        format!("{}\n{}", s("summary"), s("description"))
    }
}

/// Fetches a spec from a URL, or reads it from a file relative to `base_dir`.
pub async fn fetch_text(src: &str, base_dir: &Path) -> Result<String> {
    if src.starts_with("http://") || src.starts_with("https://") {
        let resp = reqwest::Client::builder()
            .user_agent(concat!("apisip/", env!("CARGO_PKG_VERSION")))
            .build()?
            .get(src)
            .header(
                "accept",
                "application/json, application/yaml;q=0.9, */*;q=0.5",
            )
            .send()
            .await
            .with_context(|| format!("fetching spec {src}"))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("fetching spec {src}: HTTP {status}");
        }
        Ok(resp.text().await?)
    } else {
        let path = src.strip_prefix("file://").unwrap_or(src);
        let path = base_dir.join(path);
        std::fs::read_to_string(&path).with_context(|| format!("reading spec {}", path.display()))
    }
}

/// Nodes past which one `Spec::inline` call follows no more refs. GitHub's largest
/// record schema is about 9,400 nodes; Stripe's expandable objects pass it.
const INLINE_BUDGET: usize = 20_000;

/// YAML 1.2 booleans and last duplicate key wins, as with serde_yaml_ng. No node or event
/// cap: real specs such as GitHub's pass 250,000 nodes. No alias/anchor ratio check: specs
/// reuse one anchor (a shared error response) across hundreds of operations. Depth, scalar
/// bytes, anchors, merge keys and alias replay stay bounded.
fn yaml_options() -> serde_saphyr::Options {
    serde_saphyr::options! {
        budget: serde_saphyr::budget! {
            max_events: usize::MAX,
            max_nodes: usize::MAX,
            max_depth: 128,
            enforce_alias_anchor_ratio: false,
        },
        duplicate_keys: serde_saphyr::options::DuplicateKeyPolicy::LastWins,
        strict_booleans: true,
    }
}

impl Spec {
    pub async fn load(src: &str, base_dir: &Path) -> Result<Spec> {
        let text = fetch_text(src, base_dir).await?;
        Spec::parse(&text, src)
    }

    /// Parses JSON first, then YAML (a superset of JSON, but slower and laxer).
    pub fn parse(text: &str, origin: &str) -> Result<Spec> {
        let root: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => serde_saphyr::from_str_with_options(text, yaml_options())
                .context("the spec is neither valid JSON nor YAML")?,
        };
        let spec = Spec {
            root,
            origin: origin.to_string(),
        };
        if spec.root.get("openapi").is_none() && spec.root.get("swagger").is_none() {
            bail!(
                "not an OpenAPI or Swagger document: no `openapi` or `swagger` field at the top level"
            );
        }
        if spec.root.get("paths").and_then(Value::as_object).is_none() {
            bail!("the spec has no `paths`");
        }
        Ok(spec)
    }

    pub fn version(&self) -> Version {
        if self.root.get("swagger").is_some() {
            return Version::Swagger2;
        }
        match self.root.get("openapi").and_then(Value::as_str) {
            Some(v) if v.starts_with("3.0") => Version::OpenApi30,
            _ => Version::OpenApi31,
        }
    }

    pub fn title(&self) -> &str {
        self.root
            .pointer("/info/title")
            .and_then(Value::as_str)
            .unwrap_or("API")
    }

    /// Follows a `$ref` chain to its target. Only local refs (`#/...`) are supported.
    pub fn resolve<'a>(&'a self, mut v: &'a Value) -> &'a Value {
        for _ in 0..32 {
            match v.get("$ref").and_then(Value::as_str) {
                Some(r) => match self.lookup(r) {
                    Some(target) => v = target,
                    None => return v,
                },
                None => return v,
            }
        }
        v
    }

    fn lookup(&self, reference: &str) -> Option<&Value> {
        let pointer = reference.strip_prefix('#')?;
        let decoded = percent_decode(pointer);
        self.root.pointer(&decoded)
    }

    /// Returns a deep copy of a schema with every local `$ref` inlined and
    /// `allOf` merged. A recursive ref is replaced by an empty (free-form) schema.
    pub fn inline(&self, v: &Value) -> Value {
        self.inline_bounded(v).0
    }

    /// As `inline`, and whether the copy hit `INLINE_BUDGET` nodes. Past the
    /// budget, a ref is no longer followed and becomes an empty (free-form)
    /// schema: refs that fan out can make the copy exponential in the size of
    /// the spec. Nodes written in the spec itself are still copied, so a `type`
    /// next to a cut ref survives.
    pub fn inline_bounded(&self, v: &Value) -> (Value, bool) {
        let mut stack = Vec::new();
        let (mut nodes, mut cut) = (0, false);
        let out = self.inline_inner(v, &mut stack, 0, &mut nodes, &mut cut);
        (out, cut)
    }

    /// A copy of a parameter or header with its own ref, its `schema` and that
    /// schema's `items` resolved one level. Unlike `inline`, nothing deeper is
    /// expanded, so the copy is never larger than the spec.
    pub fn shallow(&self, v: &Value) -> Value {
        let mut out = self.resolve(v).clone();
        if let Some(mut schema) = out.get("schema").map(|s| self.resolve(s).clone()) {
            if let Some(items) = schema.get("items").map(|i| self.resolve(i).clone()) {
                schema["items"] = items;
            }
            out["schema"] = schema;
        }
        out
    }

    fn inline_inner(
        &self,
        v: &Value,
        stack: &mut Vec<String>,
        depth: usize,
        nodes: &mut usize,
        cut: &mut bool,
    ) -> Value {
        // A long string counts as one node per 64 bytes, so copies of it stay bounded too.
        *nodes += 1 + v.as_str().map_or(0, |t| t.len() / 64);
        if depth > 64 {
            return Value::Object(Map::new());
        }
        match v {
            Value::Object(map) => {
                if let Some(r) = map.get("$ref").and_then(Value::as_str) {
                    if stack.iter().any(|s| s == r) {
                        return Value::Object(Map::new());
                    }
                    if *nodes > INLINE_BUDGET {
                        *cut = true;
                        return Value::Object(Map::new());
                    }
                    let Some(target) = self.lookup(r) else {
                        return Value::Object(Map::new());
                    };
                    stack.push(r.to_string());
                    let out = self.inline_inner(target, stack, depth + 1, nodes, cut);
                    stack.pop();
                    return out;
                }
                let mut out = Map::new();
                for (k, child) in map {
                    *nodes += k.len() / 64;
                    out.insert(
                        k.clone(),
                        self.inline_inner(child, stack, depth + 1, nodes, cut),
                    );
                }
                if let Some(Value::Array(parts)) = out.remove("allOf") {
                    merge_all_of(&mut out, parts);
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|i| self.inline_inner(i, stack, depth + 1, nodes, cut))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    /// The API base URL: `servers[0]` for 3.x, `schemes`/`host`/`basePath` for 2.0.
    /// Relative URLs are resolved against the spec's own URL.
    pub fn base_url(&self) -> Option<String> {
        let raw = match self.version() {
            Version::Swagger2 => {
                let scheme = self
                    .root
                    .pointer("/schemes/0")
                    .and_then(Value::as_str)
                    .unwrap_or("https");
                let base_path = self
                    .root
                    .get("basePath")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match self.root.get("host").and_then(Value::as_str) {
                    Some(host) => format!("{scheme}://{host}{base_path}"),
                    None => base_path.to_string(),
                }
            }
            _ => {
                let server = self.root.pointer("/servers/0")?;
                let mut url = server.get("url")?.as_str()?.to_string();
                if let Some(vars) = server.get("variables").and_then(Value::as_object) {
                    for (name, var) in vars {
                        if let Some(default) = var.get("default").and_then(Value::as_str) {
                            url = url.replace(&format!("{{{name}}}"), default);
                        }
                    }
                }
                url
            }
        };
        let raw = raw.trim_end_matches('/').to_string();
        if raw.starts_with("http://") || raw.starts_with("https://") {
            return Some(raw);
        }
        // Relative server URL: resolve against the spec URL, when it has one.
        let origin = url::Url::parse(&self.origin).ok()?;
        if !origin.scheme().starts_with("http") {
            return None;
        }
        let joined = origin.join(if raw.is_empty() { "/" } else { &raw }).ok()?;
        Some(joined.as_str().trim_end_matches('/').to_string())
    }

    pub fn operations(&self) -> Vec<Operation<'_>> {
        const METHODS: [&str; 8] = [
            "get", "put", "post", "delete", "options", "head", "patch", "trace",
        ];
        let mut out = Vec::new();
        let Some(paths) = self.root.get("paths").and_then(Value::as_object) else {
            return out;
        };
        for (path, item) in paths {
            let item = self.resolve(item);
            let path_params: Vec<Value> = item
                .get("parameters")
                .and_then(Value::as_array)
                .map(|ps| ps.iter().map(|p| self.shallow(p)).collect())
                .unwrap_or_default();
            for method in METHODS {
                let Some(op) = item.get(method) else { continue };
                let mut params = path_params.clone();
                if let Some(ps) = op.get("parameters").and_then(Value::as_array) {
                    for p in ps {
                        let p = self.shallow(p);
                        let key = |v: &Value| (v.get("name").cloned(), v.get("in").cloned());
                        params.retain(|existing| key(existing) != key(&p));
                        params.push(p);
                    }
                }
                out.push(Operation {
                    method,
                    path,
                    op,
                    params,
                });
            }
        }
        out
    }

    pub fn operation(&self, method: &str, path: &str) -> Option<Operation<'_>> {
        self.operations()
            .into_iter()
            .find(|o| o.method == method && o.path == path)
    }

    /// The success response object (200, else the first 2xx, else `default`).
    pub fn success_response<'a>(&'a self, op: &'a Value) -> Option<&'a Value> {
        let responses = op.get("responses")?.as_object()?;
        let pick = responses
            .get("200")
            .or_else(|| {
                responses
                    .iter()
                    .find(|(code, _)| code.starts_with('2'))
                    .map(|(_, v)| v)
            })
            .or_else(|| responses.get("default"))?;
        Some(self.resolve(pick))
    }

    /// Why the success response cannot hold records, if it declares no body
    /// (OpenAPI 3: no `content`; Swagger 2.0: no `schema`) or no JSON one.
    pub fn no_json_body(&self, op: &Value) -> Option<&'static str> {
        let Some(resp) = self.success_response(op) else {
            return Some("declares no success response");
        };
        match self.version() {
            // Swagger 2.0 defines a missing `schema` as "no content returned".
            Version::Swagger2 if resp.get("schema").is_none() => Some("declares no response body"),
            Version::Swagger2 => {
                let produces = op
                    .get("produces")
                    .or_else(|| self.root.get("produces"))
                    .and_then(Value::as_array);
                match produces {
                    // An empty list says nothing about the media type.
                    Some(types)
                        if !types.is_empty()
                            && !types.iter().filter_map(Value::as_str).any(json_media_type) =>
                    {
                        Some("does not return JSON")
                    }
                    _ => None,
                }
            }
            _ => match resp.get("content").and_then(Value::as_object) {
                None => Some("declares no response body"),
                Some(c) if !c.keys().any(|k| json_media_type(k)) => Some("does not return JSON"),
                Some(_) => None,
            },
        }
    }

    /// The inlined JSON schema of the success response, if the spec declares one.
    pub fn response_schema(&self, op: &Value) -> Option<Value> {
        self.response_schema_bounded(op).map(|(v, _)| v)
    }

    /// As `response_schema`, and whether it was cut at the inlining budget.
    pub fn response_schema_bounded(&self, op: &Value) -> Option<(Value, bool)> {
        let resp = self.success_response(op)?;
        let schema = match self.version() {
            Version::Swagger2 => resp.get("schema")?,
            _ => {
                let content = resp.get("content")?.as_object()?;
                let media = content
                    .iter()
                    .find(|(k, _)| k.to_ascii_lowercase().contains("json"))
                    .or_else(|| content.iter().next())?
                    .1;
                media.get("schema")?
            }
        };
        Some(self.inline_bounded(schema))
    }

    /// Names of the headers the success response declares (lowercased).
    pub fn response_headers(&self, op: &Value) -> Vec<(String, Value)> {
        self.success_response(op)
            .and_then(|r| r.get("headers"))
            .and_then(Value::as_object)
            .map(|h| {
                h.iter()
                    .map(|(k, v)| (k.to_ascii_lowercase(), self.shallow(v)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Declared security schemes, refs resolved, in declaration order.
    pub fn security_schemes(&self) -> Vec<(String, Value)> {
        let schemes = match self.version() {
            Version::Swagger2 => self.root.get("securityDefinitions"),
            _ => self.root.pointer("/components/securitySchemes"),
        };
        schemes
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), self.resolve(v).clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Scheme names from the top-level `security` requirement, in order.
    pub fn default_security(&self) -> Vec<String> {
        self.root
            .get("security")
            .and_then(Value::as_array)
            .map(|reqs| {
                reqs.iter()
                    .filter_map(Value::as_object)
                    .flat_map(|m| m.keys().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Type of a schema, accepting the 3.1 form `type: [T, "null"]`.
pub fn schema_type(schema: &Value) -> Option<&str> {
    match schema.get("type")? {
        Value::String(s) => Some(s),
        Value::Array(ts) => ts.iter().filter_map(Value::as_str).find(|t| *t != "null"),
        _ => None,
    }
}

fn merge_all_of(out: &mut Map<String, Value>, parts: Vec<Value>) {
    for part in parts {
        let Value::Object(part) = part else { continue };
        for (k, v) in part {
            match (k.as_str(), out.get_mut(&k)) {
                ("properties", Some(Value::Object(existing))) => {
                    if let Value::Object(props) = v {
                        existing.extend(props);
                    }
                }
                ("required", Some(Value::Array(existing))) => {
                    if let Value::Array(req) = v {
                        existing.extend(req);
                    }
                }
                (_, Some(_)) => {}
                (_, None) => {
                    out.insert(k, v);
                }
            }
        }
    }
    if out.contains_key("properties") && !out.contains_key("type") {
        out.insert("type".into(), Value::String("object".into()));
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `application/json`, `application/vnd.api+json`, `*/*`, in any case.
fn json_media_type(t: &str) -> bool {
    let t = t.to_ascii_lowercase();
    t.contains("json") || t.starts_with("*/*")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn yaml_with_integer_response_codes() {
        let spec = Spec::parse(
            "openapi: 3.0.3\ninfo: {title: t, version: '1'}\npaths:\n  /a:\n    get:\n      responses:\n        200:\n          description: ok\n",
            "x.yaml",
        )
        .unwrap();
        let op = spec.operation("get", "/a").unwrap();
        assert!(spec.success_response(op.op).is_some());
    }

    #[test]
    fn yaml_anchors_aliases_and_merge_keys() {
        let spec = Spec::parse(
            "openapi: 3.0.3\npaths: {}\ncomponents:\n  schemas:\n    Id: &id {type: integer, format: int64}\n    Page:\n      type: object\n      properties:\n        id: *id\n        size: {<<: *id, maximum: 100}\n        ratio: {type: string, type: number, default: 1.5, minimum: 0}\n        on: {type: boolean, default: yes}\n",
            "x.yaml",
        )
        .unwrap();
        let props = &spec.root["components"]["schemas"]["Page"]["properties"];
        assert_eq!(props["id"], json!({"type": "integer", "format": "int64"}));
        assert_eq!(props["ratio"]["type"], "number");
        assert!(props["ratio"]["default"].is_f64());
        assert!(props["ratio"]["minimum"].is_u64());
        // YAML 1.2: `on` and `yes` stay strings.
        assert_eq!(props["on"]["default"], "yes");
        assert_eq!(
            props["size"],
            json!({"type": "integer", "format": "int64", "maximum": 100})
        );
        assert!(props["size"]["maximum"].is_u64());
    }

    #[test]
    fn large_yaml_spec() {
        let items: String = (0..300_000).map(|i| format!("  - {i}\n")).collect();
        let spec = Spec::parse(
            &format!("openapi: 3.0.3\npaths: {{}}\nx-big:\n{items}"),
            "x.yaml",
        )
        .unwrap();
        assert_eq!(spec.root["x-big"].as_array().unwrap().len(), 300_000);
    }

    #[test]
    fn yaml_anchor_reused_by_many_operations() {
        let paths: String = (0..200)
            .map(|i| format!("  /p{i}:\n    get:\n      responses:\n        '500': *err\n"))
            .collect();
        let spec = Spec::parse(
            &format!("openapi: 3.0.3\nx-err: &err {{description: Error}}\npaths:\n{paths}"),
            "x.yaml",
        )
        .unwrap();
        assert_eq!(
            spec.root["paths"]["/p199"]["get"]["responses"]["500"]["description"],
            "Error"
        );
    }

    #[test]
    fn yaml_alias_bomb_is_refused() {
        let mut text =
            String::from("openapi: 3.0.3\npaths: {}\nl0: &l0 [x, x, x, x, x, x, x, x, x, x]\n");
        for i in 1..10 {
            let p = i - 1;
            text += &format!(
                "l{i}: &l{i} [*l{p}, *l{p}, *l{p}, *l{p}, *l{p}, *l{p}, *l{p}, *l{p}, *l{p}, *l{p}]\n"
            );
        }
        let err = format!("{:#}", Spec::parse(&text, "x.yaml").unwrap_err());
        assert!(err.contains("budget breached"), "{err}");
    }

    #[test]
    fn invalid_yaml_error_names_the_line() {
        let err = Spec::parse("openapi: 3.0.3\npaths:\n  /a: [unclosed\n", "x.yaml").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("neither valid JSON nor YAML"), "{msg}");
        assert!(msg.contains("line 3"), "{msg}");
    }

    #[test]
    fn json_bodies_are_recognised_in_any_case_and_from_produces() {
        let v3 = Spec::parse(
            &json!({"openapi": "3.0.3", "info": {"title": "t", "version": "1"}, "paths": {
                "/a": {"get": {"responses": {"200": {"description": "ok",
                    "content": {"Application/JSON; charset=utf-8": {"schema": {"type": "array"}}}}}}}
            }})
            .to_string(),
            "x.json",
        )
        .unwrap();
        assert_eq!(v3.no_json_body(v3.operation("get", "/a").unwrap().op), None);
        let ok = json!({"200": {"description": "ok", "schema": {"type": "array"}}});
        let v2 = Spec::parse(
            &json!({"swagger": "2.0", "info": {"title": "t", "version": "1"},
                "produces": ["application/xml"], "paths": {
                "/xml": {"get": {"responses": ok}},
                "/json": {"get": {"produces": ["application/json"], "responses": ok}},
                "/unknown": {"get": {"produces": [], "responses": ok}}
            }})
            .to_string(),
            "x.json",
        )
        .unwrap();
        let op = |p| v2.operation("get", p).unwrap().op;
        assert_eq!(v2.no_json_body(op("/xml")), Some("does not return JSON"));
        assert_eq!(v2.no_json_body(op("/json")), None);
        assert_eq!(v2.no_json_body(op("/unknown")), None);
    }

    #[test]
    fn inline_follows_chains_and_breaks_cycles() {
        let spec = Spec::parse(
            &json!({
                "openapi": "3.1.0",
                "paths": {},
                "components": {"schemas": {
                    "A": {"$ref": "#/components/schemas/B"},
                    "B": {"type": "object", "properties": {"self": {"$ref": "#/components/schemas/B"}, "n": {"type": "integer"}}},
                }}
            })
            .to_string(),
            "x",
        )
        .unwrap();
        let out = spec.inline(&json!({"$ref": "#/components/schemas/A"}));
        assert_eq!(out["properties"]["n"]["type"], "integer");
        assert_eq!(out["properties"]["self"], json!({}));
    }

    /// Each schema refers to the next one twice, so the inlined copy doubles
    /// at every level.
    fn doubling(levels: usize, leaf: Value) -> Spec {
        let mut schemas = Map::new();
        for i in 0..levels {
            let next = json!({"$ref": format!("#/components/schemas/S{}", i + 1)});
            schemas.insert(
                format!("S{i}"),
                json!({"type": "object", "properties": {"a": next, "b": next}}),
            );
        }
        schemas.insert(format!("S{levels}"), leaf);
        let root = json!({"openapi": "3.1.0", "paths": {}, "components": {"schemas": schemas}});
        Spec::parse(&root.to_string(), "x").unwrap()
    }

    fn nodes(v: &Value) -> usize {
        1 + match v {
            Value::Object(m) => m.values().map(nodes).sum(),
            Value::Array(a) => a.iter().map(nodes).sum(),
            _ => 0,
        }
    }

    #[test]
    fn inline_stops_at_the_node_budget() {
        let root = json!({"$ref": "#/components/schemas/S0"});
        let (small, cut) = doubling(4, json!({"type": "integer"})).inline_bounded(&root);
        assert!(!cut);
        assert_eq!(
            small.pointer("/properties/a/properties/b/properties/a/properties/b"),
            Some(&json!({"type": "integer"}))
        );
        // 15 levels build about 160,000 nodes, 30 levels about 5 billion.
        for levels in [15, 30] {
            let start = std::time::Instant::now();
            let (out, cut) = doubling(levels, json!({"type": "integer"})).inline_bounded(&root);
            assert!(cut, "{levels} levels");
            assert!(nodes(&out) <= INLINE_BUDGET + 1_000, "{levels} levels");
            assert!(start.elapsed() < std::time::Duration::from_secs(1));
        }
    }

    #[test]
    fn inline_counts_string_bytes_against_the_budget() {
        // 1,024 copies of a 20 KB value or key: few nodes, 20 MB of text.
        let long = "x".repeat(20_000);
        let mut props = Map::new();
        props.insert(long.clone(), json!({"type": "integer"}));
        let leaves = [
            json!({"type": "integer", "description": long}),
            json!({"type": "object", "properties": props}),
        ];
        for leaf in leaves {
            let (out, cut) =
                doubling(10, leaf).inline_bounded(&json!({"$ref": "#/components/schemas/S0"}));
            assert!(cut);
            assert!(out.to_string().len() < 64 * INLINE_BUDGET + 1_000_000);
        }
    }

    fn bare() -> Spec {
        Spec::parse(
            r#"{"openapi": "3.1.0", "paths": {}, "components": {"schemas": {"T": {"type": "integer"}}}}"#,
            "x",
        )
        .unwrap()
    }

    fn t() -> Value {
        json!({"$ref": "#/components/schemas/T"})
    }

    /// `items`, then a ref: whether the budget cut that ref.
    fn ref_cut_after(items: Vec<Value>) -> bool {
        let mut items = items;
        items.push(t());
        let (out, cut) = bare().inline_bounded(&Value::Array(items.clone()));
        let last = out.as_array().unwrap().last().unwrap();
        assert_eq!(cut, *last == json!({}), "{last}");
        cut
    }

    #[test]
    fn inline_budget_boundary_is_exact() {
        // The array, `n` integers and the ref: the ref is followed while that is
        // at most INLINE_BUDGET nodes.
        assert!(!ref_cut_after(vec![json!(1); INLINE_BUDGET - 2]));
        assert!(ref_cut_after(vec![json!(1); INLINE_BUDGET - 1]));
        // Nodes written in the spec are never cut, only refs.
        let (out, cut) = bare().inline_bounded(&Value::Array(vec![json!(1); INLINE_BUDGET + 5]));
        assert!(!cut);
        assert_eq!(out[INLINE_BUDGET + 4], json!(1));
    }

    #[test]
    fn inline_counts_each_64_bytes_of_a_string_or_key() {
        // A 64-byte string costs 2 nodes: the array, 49,999 strings and the ref fill the budget.
        let mut items = vec![json!("x".repeat(64)); (INLINE_BUDGET - 2) / 2];
        assert!(!ref_cut_after(items.clone()));
        items.push(json!(1));
        assert!(ref_cut_after(items));
        // A 64-byte key costs one node on top of its value.
        let keyed = |n: usize| {
            let mut m = Map::from_iter((0..n).map(|i| (format!("{i:064}"), json!(1))));
            m.insert("z".into(), t());
            bare().inline_bounded(&Value::Object(m))
        };
        let (out, cut) = keyed((INLINE_BUDGET - 2) / 2);
        assert!(!cut);
        assert_eq!(out["z"], json!({"type": "integer"}));
        let (out, cut) = keyed(INLINE_BUDGET / 2);
        assert!(cut);
        assert_eq!(out["z"], json!({}));
    }

    #[test]
    fn inline_keeps_the_total_across_short_keys() {
        let mut m = Map::from_iter((0..=INLINE_BUDGET).map(|i| (format!("k{i}"), json!(1))));
        m.insert("z".into(), t());
        assert!(bare().inline_bounded(&Value::Object(m)).1);
    }

    #[test]
    fn parameters_and_headers_are_not_expanded() {
        // 200 parameters and headers that each refer to a schema that doubles
        // 30 times: only their own ref, `schema` and `items` are resolved.
        let s0 = json!({"$ref": "#/components/schemas/S0"});
        let mut spec = doubling(30, json!({"type": "integer"})).root;
        let params: Vec<Value> = (0..200)
            .map(|i| json!({"name": format!("p{i}"), "in": "query", "schema": {"type": "array", "items": s0}}))
            .collect();
        let headers = Map::from_iter((0..200).map(|i| (format!("x-h{i}"), json!({"schema": s0}))));
        spec["paths"] = json!({"/things": {"parameters": params, "get": {"responses": {"200": {
            "description": "ok", "headers": headers}}}}});
        let spec = Spec::parse(&spec.to_string(), "x").unwrap();
        let start = std::time::Instant::now();
        let op = spec.operation("get", "/things").unwrap();
        let unexpanded = json!({"$ref": "#/components/schemas/S1"});
        assert_eq!(op.params.len(), 200);
        assert_eq!(
            op.params[0].pointer("/schema/items/properties/a"),
            Some(&unexpanded)
        );
        let headers = spec.response_headers(op.op);
        assert_eq!(headers.len(), 200);
        assert_eq!(
            headers[0].1.pointer("/schema/properties/a"),
            Some(&unexpanded)
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn inline_keeps_the_shape_around_a_cut_ref() {
        // A list envelope whose record schema is too large: the array type and
        // the sibling field must survive, or the stream looks like a single object.
        let item = json!({"$ref": "#/components/schemas/S0"});
        let envelope = json!({"type": "object", "properties": {
            "data": {"items": item, "type": "array"},
            "has_more": {"type": "boolean"}
        }});
        let (out, cut) = doubling(30, json!({"type": "integer"})).inline_bounded(&envelope);
        assert!(cut);
        assert_eq!(out.pointer("/properties/data/type"), Some(&json!("array")));
        assert_eq!(
            out.pointer("/properties/has_more"),
            Some(&json!({"type": "boolean"}))
        );
        assert_eq!(
            out.pointer("/properties/data/items/properties/a/type"),
            Some(&json!("object"))
        );
    }

    #[test]
    fn inline_stops_at_depth_64_through_arrays_and_refs() {
        let spec = bare();
        let nested = |depth: usize| (0..depth).fold(json!(1), |v, _| json!([v]));
        assert_eq!(spec.inline(&nested(64)), nested(64));
        // Depth 65 is cut to an empty schema, and nothing below it is kept.
        let mut cursor = spec.inline(&nested(70));
        for _ in 0..65 {
            cursor = cursor[0].clone();
        }
        assert_eq!(cursor, json!({}));
        // Object members deepen the copy too.
        let nested_obj = |depth: usize| (0..depth).fold(json!(1), |v, _| json!({"k": v}));
        assert_eq!(spec.inline(&nested_obj(64)), nested_obj(64));
        assert_eq!(
            spec.inline(&nested_obj(70)).pointer(&"/k".repeat(65)),
            Some(&json!({}))
        );
        // Refs deepen the copy too: a chain of 70 distinct refs ends past the limit.
        let mut schemas = Map::new();
        for i in 0..70 {
            let next = format!("#/components/schemas/S{}", i + 1);
            schemas.insert(format!("S{i}"), json!({"$ref": next}));
        }
        schemas.insert("S70".into(), json!({"type": "integer"}));
        let root = json!({"openapi": "3.1.0", "paths": {}, "components": {"schemas": schemas}});
        let spec = Spec::parse(&root.to_string(), "x").unwrap();
        let out = spec.inline(&json!({"$ref": "#/components/schemas/S0"}));
        assert_eq!(out, json!({}));
    }

    #[test]
    fn all_of_is_merged() {
        let spec = Spec::parse(
            &json!({
                "openapi": "3.0.0", "paths": {},
                "components": {"schemas": {"Base": {"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}}}
            })
            .to_string(),
            "x",
        )
        .unwrap();
        let out = spec.inline(&json!({"allOf": [{"$ref": "#/components/schemas/Base"}, {"properties": {"name": {"type": "string"}}}]}));
        assert_eq!(out["type"], "object");
        assert_eq!(out["properties"]["id"]["type"], "integer");
        assert_eq!(out["properties"]["name"]["type"], "string");
    }

    #[test]
    fn base_url_variants() {
        let v3 = Spec::parse(
            &json!({"openapi": "3.0.0", "paths": {}, "servers": [{"url": "https://{region}.example.com/v1/", "variables": {"region": {"default": "eu"}}}]}).to_string(),
            "x",
        )
        .unwrap();
        assert_eq!(v3.base_url().as_deref(), Some("https://eu.example.com/v1"));
        let rel = Spec::parse(
            &json!({"openapi": "3.0.0", "paths": {}, "servers": [{"url": "/api/v3"}]}).to_string(),
            "https://petstore3.example.org/api/v3/openapi.json",
        )
        .unwrap();
        assert_eq!(
            rel.base_url().as_deref(),
            Some("https://petstore3.example.org/api/v3")
        );
        let v2 = Spec::parse(
            &json!({"swagger": "2.0", "paths": {}, "host": "api.example.com", "basePath": "/v2", "schemes": ["https"]}).to_string(),
            "x",
        )
        .unwrap();
        assert_eq!(v2.base_url().as_deref(), Some("https://api.example.com/v2"));
    }

    #[test]
    fn rejects_non_specs() {
        assert!(Spec::parse("{\"hello\": 1}", "x").is_err());
        assert!(Spec::parse("<html>", "x").is_err());
    }
}
