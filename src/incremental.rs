//! Cursor values, watermarks and the early-stop guard.

use std::cmp::Ordering;

use chrono::{DateTime, Utc};
use serde_json::Value;

/// A record field by dotted path (`updated_at`, `meta.updated_at`).
pub fn field<'a>(record: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(record, |v, k| v.get(k))
        .filter(|v| !v.is_null())
}

/// A comparable cursor value: a timestamp, a number, or a string.
#[derive(Debug, Clone, PartialEq)]
pub enum CursorValue {
    Time(DateTime<Utc>),
    Number(f64),
    Text(String),
}

impl CursorValue {
    pub fn from_json(v: &Value) -> Option<CursorValue> {
        match v {
            Value::Number(n) => n.as_f64().map(CursorValue::Number),
            Value::String(s) => Some(CursorValue::parse(s)),
            _ => None,
        }
    }

    /// Parses a stored watermark or a string field.
    pub fn parse(s: &str) -> CursorValue {
        // The same timestamp forms the schema accepts, offset or not.
        if let Some(t) = crate::schema::parse_timestamp(s) {
            return CursorValue::Time(t);
        }
        if let Ok(n) = s.parse::<f64>() {
            return CursorValue::Number(n);
        }
        CursorValue::Text(s.to_string())
    }

    fn text(&self) -> String {
        match self {
            CursorValue::Time(t) => t.to_rfc3339(),
            CursorValue::Number(n) => n.to_string(),
            CursorValue::Text(s) => s.clone(),
        }
    }
}

impl PartialOrd for CursorValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (CursorValue::Time(a), CursorValue::Time(b)) => a.partial_cmp(b),
            (CursorValue::Number(a), CursorValue::Number(b)) => a.partial_cmp(b),
            (a, b) => a.text().partial_cmp(&b.text()),
        }
    }
}

/// Raw JSON value of a cursor as written to state and sent as a filter.
pub fn raw(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Tracks the lowest and highest cursor of the records seen.
///
/// Values of one kind only count: the kind of the stored watermark, or else
/// the kind seen most often. A stray `"N/A"` in a timestamp cursor would
/// otherwise sort above every date as text and freeze the stream.
#[derive(Debug, Default, Clone)]
pub struct Range {
    kind: Option<Kind>,
    by_kind: [KindRange; 3],
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Time = 0,
    Number = 1,
    Text = 2,
}

#[derive(Debug, Default, Clone)]
struct KindRange {
    count: usize,
    min: Option<(CursorValue, String)>,
    max: Option<(CursorValue, String)>,
}

impl CursorValue {
    fn kind(&self) -> Kind {
        match self {
            CursorValue::Time(_) => Kind::Time,
            CursorValue::Number(_) => Kind::Number,
            CursorValue::Text(_) => Kind::Text,
        }
    }

    /// Whether two values are of the same kind, so that comparing them means something.
    pub fn same_kind(&self, other: &CursorValue) -> bool {
        self.kind() == other.kind()
    }
}

impl Range {
    /// A range whose values must be of the same kind as this watermark.
    pub fn new(watermark: Option<&str>) -> Range {
        Range {
            kind: watermark.map(|w| CursorValue::parse(w).kind()),
            ..Range::default()
        }
    }

    pub fn observe(&mut self, v: &Value) {
        let Some(c) = CursorValue::from_json(v) else {
            return;
        };
        let r = &mut self.by_kind[c.kind() as usize];
        r.count += 1;
        if r.min.as_ref().is_none_or(|(m, _)| c < *m) {
            r.min = Some((c.clone(), raw(v)));
        }
        if r.max.as_ref().is_none_or(|(m, _)| c > *m) {
            r.max = Some((c, raw(v)));
        }
    }

    fn chosen(&self) -> &KindRange {
        let kind = self.kind.unwrap_or_else(|| {
            // Ties go to the first kind: a timestamp beats a number beats text.
            [Kind::Time, Kind::Number, Kind::Text]
                .into_iter()
                .rev()
                .max_by_key(|k| self.by_kind[*k as usize].count)
                .unwrap_or(Kind::Time)
        });
        &self.by_kind[kind as usize]
    }

    /// Cursor values left out because their kind differs from the chosen one.
    pub fn ignored(&self) -> usize {
        let total: usize = self.by_kind.iter().map(|r| r.count).sum();
        total - self.chosen().count
    }

    pub fn min_raw(&self) -> Option<String> {
        self.chosen().min.as_ref().map(|(_, r)| r.clone())
    }

    pub fn max_raw(&self) -> Option<String> {
        self.chosen().max.as_ref().map(|(_, r)| r.clone())
    }
}

/// Client-side incremental for endpoints sorted by cursor, newest first:
/// drops records older than the watermark and stops paging once a whole
/// page is older. If the cursor ever goes up, the sort is not what the spec
/// promised: the guard trips and the rest of the run is a full fetch.
#[derive(Debug)]
pub struct EarlyStop {
    cursor: String,
    watermark: Option<CursorValue>,
    previous: Option<CursorValue>,
    pub unreliable: bool,
}

impl EarlyStop {
    pub fn new(cursor: &str, watermark: Option<&str>) -> EarlyStop {
        EarlyStop {
            cursor: cursor.to_string(),
            watermark: watermark.map(CursorValue::parse),
            previous: None,
            unreliable: false,
        }
    }

    /// The record's cursor, when it is of the kind of the watermark (or of
    /// the first cursor seen). Other values are kept but never compared.
    fn cursor_of(&self, record: &Value) -> Option<CursorValue> {
        let c = field(record, &self.cursor).and_then(CursorValue::from_json)?;
        let reference = self.watermark.as_ref().or(self.previous.as_ref());
        reference.is_none_or(|w| w.same_kind(&c)).then_some(c)
    }

    /// Filters one page. Returns the records to keep and whether to stop paging.
    pub fn page(&mut self, records: Vec<Value>) -> (Vec<Value>, bool) {
        for r in &records {
            if let Some(c) = self.cursor_of(r) {
                if self.previous.as_ref().is_some_and(|p| c > *p) && !self.unreliable {
                    tracing::warn!(
                        "cursor `{}` increased within a descending sort: the API does not sort as \
                         documented, so this run fetches everything",
                        self.cursor
                    );
                    self.unreliable = true;
                }
                self.previous = Some(c);
            }
        }
        let Some(wm) = &self.watermark else {
            return (records, false);
        };
        if self.unreliable {
            return (records, false);
        }
        let total = records.len();
        let kept: Vec<Value> = records
            .into_iter()
            .filter(|r| match self.cursor_of(r) {
                Some(c) => c >= *wm,
                None => true,
            })
            .collect();
        // One page past the first old record is fetched on purpose: it gives
        // the guard one more page to check the sort on.
        let stop = total > 0 && kept.is_empty();
        (kept, stop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn recs(ts: &[&str]) -> Vec<Value> {
        ts.iter()
            .map(|t| json!({"id": t, "updated_at": t}))
            .collect()
    }

    #[test]
    fn early_stop_cuts_at_the_watermark_inclusively() {
        let mut g = EarlyStop::new("updated_at", Some("2026-01-03T00:00:00Z"));
        let (kept, stop) = g.page(recs(&["2026-01-05T00:00:00Z", "2026-01-04T00:00:00Z"]));
        assert_eq!((kept.len(), stop), (2, false));
        let (kept, stop) = g.page(recs(&["2026-01-03T00:00:00Z", "2026-01-02T00:00:00Z"]));
        // The record equal to the watermark is kept (at least once).
        assert_eq!(kept, recs(&["2026-01-03T00:00:00Z"]));
        assert!(!stop);
        let (kept, stop) = g.page(recs(&["2026-01-01T00:00:00Z"]));
        assert!(kept.is_empty() && stop);
        assert!(!g.unreliable);
    }

    #[test]
    fn guard_trips_when_the_sort_is_a_lie() {
        let mut g = EarlyStop::new("updated_at", Some("2026-01-03T00:00:00Z"));
        let (kept, stop) = g.page(recs(&[
            "2026-01-05T00:00:00Z",
            "2026-01-01T00:00:00Z",
            "2026-01-09T00:00:00Z",
        ]));
        assert!(g.unreliable);
        assert_eq!((kept.len(), stop), (3, false));
        let (kept, stop) = g.page(recs(&["2026-01-01T00:00:00Z"]));
        assert_eq!((kept.len(), stop), (1, false));
    }

    #[test]
    fn no_watermark_keeps_everything() {
        let mut g = EarlyStop::new("updated_at", None);
        let (kept, stop) = g.page(recs(&["2026-01-02T00:00:00Z", "2026-01-01T00:00:00Z"]));
        assert_eq!((kept.len(), stop), (2, false));
    }

    #[test]
    fn cursor_values_compare_by_kind() {
        let a = CursorValue::parse("2026-01-01T10:00:00+02:00");
        let b = CursorValue::parse("2026-01-01T09:00:00Z");
        assert!(a < b, "timestamps compare in UTC");
        assert!(CursorValue::from_json(&json!(10)) > CursorValue::from_json(&json!(9)));
        assert!(CursorValue::parse("10") > CursorValue::parse("9"));
        assert!(
            CursorValue::parse("2026-01-02T10:00:00")
                .same_kind(&CursorValue::parse("2026-01-01T10:00:00Z")),
            "a timestamp without an offset is still a timestamp"
        );
        let mut r = Range::default();
        for v in [json!(3), json!(1), json!(7)] {
            r.observe(&v);
        }
        assert_eq!(
            (r.min_raw().unwrap(), r.max_raw().unwrap()),
            ("1".into(), "7".into())
        );
        assert_eq!(field(&json!({"a": {"b": 1}}), "a.b"), Some(&json!(1)));
    }

    #[test]
    fn a_stray_text_value_never_becomes_the_watermark() {
        let dates = [json!("2026-01-01T00:00:00Z"), json!("2026-01-02T00:00:00Z")];
        // No watermark yet: the kind seen most often wins.
        let mut r = Range::new(None);
        for v in dates.iter().chain([&json!("N/A")]) {
            r.observe(v);
        }
        assert_eq!(r.max_raw().as_deref(), Some("2026-01-02T00:00:00Z"));
        assert_eq!(r.ignored(), 1);
        // With a watermark, its kind wins even when outnumbered.
        let mut r = Range::new(Some("2025-12-31T00:00:00Z"));
        for v in [json!("N/A"), json!("unknown"), dates[0].clone()] {
            r.observe(&v);
        }
        assert_eq!(r.max_raw().as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(r.ignored(), 2);
    }

    #[test]
    fn early_stop_ignores_cursors_of_another_kind() {
        let mut g = EarlyStop::new("updated_at", Some("2026-01-03T00:00:00Z"));
        let page = vec![
            json!({"id": 1, "updated_at": "2026-01-05T00:00:00Z"}),
            json!({"id": 2, "updated_at": "N/A"}),
            json!({"id": 3, "updated_at": "2026-01-04T00:00:00Z"}),
        ];
        let (kept, stop) = g.page(page);
        assert_eq!((kept.len(), stop), (3, false));
        assert!(!g.unreliable, "text vs time is not a sort violation");
    }
}
