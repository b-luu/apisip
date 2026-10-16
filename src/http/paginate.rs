//! Pagination as a continuation: each response is turned into the next URL,
//! or into the end of the stream.

use serde_json::Value;
use url::Url;

use super::Page;
use crate::config::Pagination;

/// Finds the records in a response body. `pointer` is the configured JSON
/// pointer; when empty, an array body is the records, an object body with a
/// single array field uses that field, and any other object is one record.
pub fn extract_records(body: &Value, pointer: &str) -> Vec<Value> {
    let target = if pointer.is_empty() {
        body
    } else {
        match body.pointer(pointer) {
            Some(v) => v,
            None => return Vec::new(),
        }
    };
    match target {
        Value::Array(a) => a.clone(),
        Value::Null => Vec::new(),
        Value::Object(o) if pointer.is_empty() => {
            let mut arrays = o.values().filter(|v| v.is_array());
            match (arrays.next(), arrays.next()) {
                (Some(Value::Array(a)), None) => a.clone(),
                _ => vec![target.clone()],
            }
        }
        other => vec![other.clone()],
    }
}

#[derive(Debug)]
pub struct Paginator {
    pagination: Option<Pagination>,
    /// Next page number or offset.
    position: u64,
    seen: std::collections::HashSet<String>,
    last_fingerprint: Option<String>,
}

impl Paginator {
    pub fn new(pagination: Option<Pagination>) -> Paginator {
        let position = match &pagination {
            Some(Pagination::PageNumber { start, .. }) => *start,
            _ => 0,
        };
        Paginator {
            pagination,
            position,
            seen: Default::default(),
            last_fingerprint: None,
        }
    }

    /// The first request: `url` plus the paging parameters of page one.
    pub fn first(&mut self, mut url: Url) -> Url {
        match &self.pagination {
            Some(Pagination::PageNumber {
                param,
                size_param,
                size,
                ..
            }) => {
                set_param(&mut url, param, &self.position.to_string());
                if let (Some(p), Some(s)) = (size_param, size) {
                    set_param(&mut url, p, &s.to_string());
                }
            }
            Some(Pagination::Offset {
                param,
                limit_param,
                limit,
            }) => {
                set_param(&mut url, param, "0");
                set_param(&mut url, limit_param, &limit.to_string());
            }
            Some(Pagination::Cursor {
                size_param: Some(p),
                size: Some(s),
                ..
            }) => set_param(&mut url, p, &s.to_string()),
            _ => {}
        }
        self.seen.insert(url.to_string());
        url
    }

    /// The URL of the page after `page`, or `None` at the end.
    pub fn next(&mut self, page: &Page, records: &[Value]) -> Option<Url> {
        let next = self.compute_next(page, records)?;
        // Guards against APIs that repeat the last page forever.
        let fingerprint = format!(
            "{}:{}",
            records.len(),
            records.first().map(|r| r.to_string()).unwrap_or_default()
        );
        if !records.is_empty() && self.last_fingerprint.as_deref() == Some(fingerprint.as_str()) {
            tracing::debug!("page repeats the previous one, stopping");
            return None;
        }
        self.last_fingerprint = Some(fingerprint);
        if !self.seen.insert(next.to_string()) {
            tracing::debug!("next page {next} was already fetched, stopping");
            return None;
        }
        Some(next)
    }

    fn compute_next(&mut self, page: &Page, records: &[Value]) -> Option<Url> {
        // A Link rel="next" header wins for every style: it is explicit.
        if let Some(link) = link_next(page) {
            return Some(link);
        }
        if records.is_empty() || has_more(&page.body) == Some(false) {
            return None;
        }
        let mut url = page.url.clone();
        match &self.pagination {
            None | Some(Pagination::LinkHeader) => None,
            Some(Pagination::PageNumber { param, size, .. }) => {
                if size.is_some_and(|s| (records.len() as u64) < s as u64) {
                    return None;
                }
                self.position += 1;
                set_param(&mut url, param, &self.position.to_string());
                Some(url)
            }
            Some(Pagination::Offset {
                param,
                limit_param: _,
                limit,
            }) => {
                if (records.len() as u64) < *limit as u64 {
                    return None;
                }
                self.position += records.len() as u64;
                set_param(&mut url, param, &self.position.to_string());
                Some(url)
            }
            Some(Pagination::Cursor {
                param,
                cursor_path,
                last_record_field,
                size,
                ..
            }) => {
                let token = match (cursor_path, last_record_field) {
                    (Some(p), _) => page.body.pointer(p).cloned(),
                    (None, Some(f)) => {
                        if size.is_some_and(|s| (records.len() as u64) < s as u64) {
                            return None;
                        }
                        records
                            .last()
                            .and_then(|r| crate::incremental::field(r, f))
                            .cloned()
                    }
                    (None, None) => None,
                };
                let token = scalar(token?)?;
                if let Some(u) = as_url(&page.url, &token) {
                    return Some(u);
                }
                set_param(&mut url, param, &token);
                Some(url)
            }
            Some(Pagination::NextUrl { next_path }) => {
                let token = scalar(page.body.pointer(next_path)?.clone())?;
                page.url.join(&token).ok()
            }
        }
    }
}

fn scalar(v: Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A cursor value that is itself a URL (absolute, or a path) is followed as-is.
fn as_url(current: &Url, token: &str) -> Option<Url> {
    if token.starts_with("http://") || token.starts_with("https://") || token.starts_with('/') {
        current.join(token).ok()
    } else {
        None
    }
}

fn has_more(body: &Value) -> Option<bool> {
    [
        "/has_more",
        "/hasMore",
        "/meta/has_more",
        "/pagination/has_more",
    ]
    .iter()
    .find_map(|p| body.pointer(p)?.as_bool())
}

fn link_next(page: &Page) -> Option<Url> {
    for value in page.headers.get_all(reqwest::header::LINK) {
        let value = value.to_str().ok()?;
        for part in value.split(',') {
            let (target, params) = part.split_once(';').unwrap_or((part, ""));
            let is_next = params.split(';').any(|p| {
                let p = p.trim().to_ascii_lowercase().replace(' ', "");
                p == "rel=\"next\"" || p == "rel=next"
            });
            if is_next {
                let target = target.trim().trim_start_matches('<').trim_end_matches('>');
                return page.url.join(target).ok();
            }
        }
    }
    None
}

/// Sets (or replaces) one query parameter.
pub fn set_param(url: &mut Url, key: &str, value: &str) {
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != key)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut q = url.query_pairs_mut();
    q.clear();
    for (k, v) in &pairs {
        q.append_pair(k, v);
    }
    q.append_pair(key, value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};
    use serde_json::json;

    fn page(url: &str, body: Value) -> Page {
        Page {
            url: Url::parse(url).unwrap(),
            status: 200,
            headers: HeaderMap::new(),
            body,
        }
    }

    fn q(url: &Url, key: &str) -> Option<String> {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    }

    #[test]
    fn records_extraction_is_lenient() {
        assert_eq!(extract_records(&json!([1, 2]), "").len(), 2);
        assert_eq!(
            extract_records(&json!({"data": [1, 2, 3], "total": 3}), "").len(),
            3
        );
        assert_eq!(
            extract_records(&json!({"id": 1}), ""),
            vec![json!({"id": 1})]
        );
        assert_eq!(extract_records(&json!({"a": [1], "b": [2]}), "").len(), 1);
        assert_eq!(
            extract_records(&json!({"r": {"items": [1]}}), "/r/items").len(),
            1
        );
        assert!(extract_records(&json!({"r": null}), "/r").is_empty());
        assert!(extract_records(&json!({}), "/missing").is_empty());
    }

    #[test]
    fn page_number_stops_on_short_page() {
        let mut p = Paginator::new(Some(Pagination::PageNumber {
            param: "page".into(),
            size_param: Some("per_page".into()),
            size: Some(2),
            start: 1,
        }));
        let u = p.first(Url::parse("http://x.test/users?status=all").unwrap());
        assert_eq!(q(&u, "page").as_deref(), Some("1"));
        assert_eq!(q(&u, "per_page").as_deref(), Some("2"));
        assert_eq!(q(&u, "status").as_deref(), Some("all"));
        let full = [json!({"id": 1}), json!({"id": 2})];
        let u2 = p.next(&page(u.as_str(), json!(full)), &full).unwrap();
        assert_eq!(q(&u2, "page").as_deref(), Some("2"));
        let short = [json!({"id": 3})];
        assert!(p.next(&page(u2.as_str(), json!(short)), &short).is_none());
    }

    #[test]
    fn page_number_without_size_stops_on_empty_or_repeated_page() {
        let mut p = Paginator::new(Some(Pagination::PageNumber {
            param: "page".into(),
            size_param: None,
            size: None,
            start: 0,
        }));
        let u = p.first(Url::parse("http://x.test/a").unwrap());
        let recs = [json!({"id": 1})];
        let u2 = p.next(&page(u.as_str(), json!(recs)), &recs).unwrap();
        // The API ignores the page param and returns page 0 again.
        assert!(p.next(&page(u2.as_str(), json!(recs)), &recs).is_none());
        assert!(p.next(&page(u2.as_str(), json!([])), &[]).is_none());
    }

    #[test]
    fn offset_advances_by_records() {
        let mut p = Paginator::new(Some(Pagination::Offset {
            param: "offset".into(),
            limit_param: "limit".into(),
            limit: 2,
        }));
        let u = p.first(Url::parse("http://x.test/a").unwrap());
        assert_eq!(q(&u, "offset").as_deref(), Some("0"));
        let recs = [json!(1), json!(2)];
        let u2 = p.next(&page(u.as_str(), json!(recs)), &recs).unwrap();
        assert_eq!(q(&u2, "offset").as_deref(), Some("2"));
        assert_eq!(q(&u2, "limit").as_deref(), Some("2"));
    }

    #[test]
    fn cursor_from_body_from_last_record_and_as_url() {
        let mut p = Paginator::new(Some(Pagination::Cursor {
            param: "cursor".into(),
            cursor_path: Some("/meta/next".into()),
            last_record_field: None,
            size_param: None,
            size: None,
        }));
        let u = p.first(Url::parse("http://x.test/a").unwrap());
        let recs = [json!(1)];
        let body = json!({"data": [1], "meta": {"next": "abc"}});
        let u2 = p.next(&page(u.as_str(), body), &recs).unwrap();
        assert_eq!(q(&u2, "cursor").as_deref(), Some("abc"));
        let body = json!({"data": [2], "meta": {"next": "/a?cursor=def&x=1"}});
        let u3 = p.next(&page(u2.as_str(), body), &[json!(2)]).unwrap();
        assert_eq!(u3.as_str(), "http://x.test/a?cursor=def&x=1");
        let body = json!({"data": [3], "meta": {"next": null}});
        assert!(p.next(&page(u3.as_str(), body), &[json!(3)]).is_none());

        let mut p = Paginator::new(Some(Pagination::Cursor {
            param: "starting_after".into(),
            cursor_path: None,
            last_record_field: Some("id".into()),
            size_param: Some("limit".into()),
            size: Some(2),
        }));
        let u = p.first(Url::parse("http://x.test/a").unwrap());
        let recs = [json!({"id": "a"}), json!({"id": "b"})];
        let body = json!({"data": recs, "has_more": true});
        let u2 = p.next(&page(u.as_str(), body), &recs).unwrap();
        assert_eq!(q(&u2, "starting_after").as_deref(), Some("b"));
        let body = json!({"data": recs, "has_more": false});
        assert!(p.next(&page(u2.as_str(), body), &recs).is_none());
    }

    #[test]
    fn next_url_and_link_header() {
        let mut p = Paginator::new(Some(Pagination::NextUrl {
            next_path: "/next".into(),
        }));
        let u = p.first(Url::parse("http://x.test/a").unwrap());
        let body = json!({"results": [1], "next": "http://x.test/a?page=2"});
        let u2 = p.next(&page(u.as_str(), body), &[json!(1)]).unwrap();
        assert_eq!(u2.as_str(), "http://x.test/a?page=2");

        let mut p = Paginator::new(None);
        let u = p.first(Url::parse("http://x.test/a").unwrap());
        let mut pg = page(u.as_str(), json!([1]));
        pg.headers.insert(
            "link",
            HeaderValue::from_static(
                "<http://x.test/a?page=1>; rel=\"prev\", <http://x.test/a?page=3>; rel=\"next\"",
            ),
        );
        assert_eq!(
            p.next(&pg, &[json!(1)]).unwrap().as_str(),
            "http://x.test/a?page=3"
        );
        // Without the header, an unpaginated stream ends after one page.
        assert!(p.next(&page(u.as_str(), json!([1])), &[json!(1)]).is_none());
    }
}
