//! Cursor values, watermarks and the early-stop guard.

use std::cmp::Ordering;
use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde_json::Value;

/// A record field by dotted path (`updated_at`, `meta.updated_at`).
pub fn field<'a>(record: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(record, |v, k| v.get(k))
        .filter(|v| !v.is_null())
}

/// A comparable cursor value: a timestamp, a number, or a string.
///
/// Integers are kept exact: as `f64`, ids and nanosecond epochs above 2^53
/// would round, and neighbours would compare equal.
#[derive(Debug, Clone)]
pub enum CursorValue {
    Time(DateTime<Utc>),
    Int(i128),
    Number(f64),
    Text(String),
}

impl CursorValue {
    pub fn from_json(v: &Value) -> Option<CursorValue> {
        match v {
            Value::Number(n) => n
                .as_i64()
                .map(i128::from)
                .or_else(|| n.as_u64().map(i128::from))
                .map(CursorValue::Int)
                .or_else(|| n.as_f64().map(CursorValue::Number)),
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
        if let Ok(i) = s.parse::<i128>() {
            return CursorValue::Int(i);
        }
        if let Ok(n) = s.parse::<f64>() {
            return CursorValue::Number(n);
        }
        CursorValue::Text(s.to_string())
    }

    fn text(&self) -> String {
        match self {
            CursorValue::Time(t) => t.to_rfc3339(),
            CursorValue::Int(i) => i.to_string(),
            CursorValue::Number(n) => n.to_string(),
            CursorValue::Text(s) => s.clone(),
        }
    }
}

impl PartialOrd for CursorValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (CursorValue::Time(a), CursorValue::Time(b)) => a.partial_cmp(b),
            (CursorValue::Int(a), CursorValue::Int(b)) => Some(a.cmp(b)),
            (CursorValue::Number(a), CursorValue::Number(b)) => a.partial_cmp(b),
            (CursorValue::Int(i), CursorValue::Number(f)) => int_cmp_float(*i, *f),
            (CursorValue::Number(f), CursorValue::Int(i)) => {
                int_cmp_float(*i, *f).map(Ordering::reverse)
            }
            (a, b) => a.text().partial_cmp(&b.text()),
        }
    }
}

/// `==` agrees with the ordering, so `5` and `5.0` are the same cursor.
impl PartialEq for CursorValue {
    fn eq(&self, other: &Self) -> bool {
        self.partial_cmp(other) == Some(Ordering::Equal)
    }
}

/// Compares an integer with a float without rounding the integer first.
fn int_cmp_float(i: i128, f: f64) -> Option<Ordering> {
    const LIMIT: f64 = -(i128::MIN as f64);
    if f.is_nan() {
        None
    } else if f >= LIMIT {
        Some(Ordering::Less)
    } else if f < -LIMIT {
        Some(Ordering::Greater)
    } else if f.fract() == 0.0 {
        Some(i.cmp(&(f as i128)))
    } else {
        // A float with a fraction is below 2^53, where rounding `i` cannot cross it.
        (i as f64).partial_cmp(&f)
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
    cap: Option<DateTime<Utc>>,
    future: usize,
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
            CursorValue::Int(_) | CursorValue::Number(_) => Kind::Number,
            CursorValue::Text(_) => Kind::Text,
        }
    }

    /// Whether two values are of the same kind, so that comparing them means something.
    pub fn same_kind(&self, other: &CursorValue) -> bool {
        self.kind() == other.kind()
    }
}

impl Range {
    /// A range whose values must be of the same kind as this watermark, and
    /// whose timestamps must not be later than `cap`.
    pub fn new(watermark: Option<&str>, cap: Option<DateTime<Utc>>) -> Range {
        Range {
            kind: watermark.map(|w| CursorValue::parse(w).kind()),
            cap,
            ..Range::default()
        }
    }

    /// Returns false when the value is left out: not a cursor, or past the cap.
    pub fn observe(&mut self, v: &Value) -> bool {
        let Some(c) = CursorValue::from_json(v) else {
            return false;
        };
        if beyond(&c, self.cap) {
            self.future += 1;
            return false;
        }
        let r = &mut self.by_kind[c.kind() as usize];
        r.count += 1;
        if r.min.as_ref().is_none_or(|(m, _)| c < *m) {
            r.min = Some((c.clone(), raw(v)));
        }
        if r.max.as_ref().is_none_or(|(m, _)| c > *m) {
            r.max = Some((c, raw(v)));
        }
        true
    }

    fn chosen(&self) -> &KindRange {
        let kind = self.kind.unwrap_or_else(|| {
            // Ties go to the first kind: a timestamp beats a number beats text.
            [Kind::Time, Kind::Number, Kind::Text]
                .into_iter()
                .rev()
                // Capped timestamps still vote: a stray text value must not win.
                .max_by_key(|k| {
                    self.by_kind[*k as usize].count + if *k == Kind::Time { self.future } else { 0 }
                })
                .unwrap_or(Kind::Time)
        });
        &self.by_kind[kind as usize]
    }

    /// Cursor values left out because their kind differs from the chosen one.
    pub fn ignored(&self) -> usize {
        let total: usize = self.by_kind.iter().map(|r| r.count).sum();
        total - self.chosen().count
    }

    /// Timestamps left out because they are later than the cap.
    pub fn future(&self) -> usize {
        self.future
    }

    pub fn min_raw(&self) -> Option<String> {
        self.chosen().min.as_ref().map(|(_, r)| r.clone())
    }

    pub fn max_raw(&self) -> Option<String> {
        self.chosen().max.as_ref().map(|(_, r)| r.clone())
    }
}

/// Whether a cursor is a timestamp later than the cap. One far-future value
/// (bad data, a sentinel date) would otherwise become the watermark, and
/// every later record would never be fetched. Numbers and text are not capped.
pub fn beyond(c: &CursorValue, cap: Option<DateTime<Utc>>) -> bool {
    matches!((c, cap), (CursorValue::Time(t), Some(cap)) if *t > cap)
}

/// Most fingerprints kept for one watermark. Past this, none are kept and
/// the next run writes the boundary records again.
pub const MAX_BOUNDARY: usize = 10_000;

/// A stable fingerprint of a record as received: FNV-1a over its JSON with
/// object keys sorted, apisip's own `_apisip_*` fields left out.
pub fn fingerprint(record: &Value) -> String {
    fn canonical(v: &Value, top: bool) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m
                    .keys()
                    .filter(|k| !(top && k.starts_with("_apisip_")))
                    .collect();
                keys.sort();
                Value::Object(
                    keys.into_iter()
                        .map(|k| (k.clone(), canonical(&m[k], false)))
                        .collect(),
                )
            }
            Value::Array(a) => Value::Array(a.iter().map(|x| canonical(x, false)).collect()),
            other => other.clone(),
        }
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in canonical(record, true).to_string().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The records an earlier run wrote at the watermark. The watermark check
/// is inclusive, so the next run gets them again: unchanged ones are skipped.
#[derive(Debug)]
pub struct Boundary {
    watermark: CursorValue,
    written: HashSet<String>,
}

impl Boundary {
    pub fn new(watermark: &str, fingerprints: &[String]) -> Option<Boundary> {
        (!fingerprints.is_empty()).then(|| Boundary {
            watermark: CursorValue::parse(watermark),
            written: fingerprints.iter().cloned().collect(),
        })
    }

    /// Whether this record is at the watermark and was written unchanged.
    pub fn unchanged(&self, record: &Value, cursor: &str) -> bool {
        field(record, cursor)
            .and_then(CursorValue::from_json)
            .is_some_and(|c| c == self.watermark)
            && self.written.contains(&fingerprint(record))
    }
}

/// Collects the fingerprints of the written records at the highest cursor.
#[derive(Debug, Default)]
pub struct BoundaryTracker {
    max: Option<CursorValue>,
    fingerprints: Vec<String>,
    pub overflow: bool,
}

impl BoundaryTracker {
    pub fn observe(&mut self, record: &Value, cursor: &Value) {
        let Some(c) = CursorValue::from_json(cursor) else {
            return;
        };
        match self.max.as_ref().map(|m| c.partial_cmp(m)) {
            Some(Some(Ordering::Less)) | Some(None) => return,
            Some(Some(Ordering::Equal)) => {}
            _ => {
                self.max = Some(c);
                self.fingerprints.clear();
                self.overflow = false;
            }
        }
        if self.fingerprints.len() < MAX_BOUNDARY {
            self.fingerprints.push(fingerprint(record));
        } else {
            self.overflow = true;
        }
    }

    /// The fingerprints at `max`, the watermark this run commits. Empty when
    /// they do not fit, or when `max` is not the value tracked here.
    pub fn finish(self, max: &str) -> Vec<String> {
        match self.max {
            Some(m) if !self.overflow && m == CursorValue::parse(max) => self.fingerprints,
            _ => Vec::new(),
        }
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
        let mut r = Range::new(None, None);
        for v in dates.iter().chain([&json!("N/A")]) {
            r.observe(v);
        }
        assert_eq!(r.max_raw().as_deref(), Some("2026-01-02T00:00:00Z"));
        assert_eq!(r.ignored(), 1);
        // With a watermark, its kind wins even when outnumbered.
        let mut r = Range::new(Some("2025-12-31T00:00:00Z"), None);
        for v in [json!("N/A"), json!("unknown"), dates[0].clone()] {
            r.observe(&v);
        }
        assert_eq!(r.max_raw().as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(r.ignored(), 2);
    }

    #[test]
    fn a_far_future_timestamp_never_becomes_the_watermark() {
        let cap = crate::schema::parse_timestamp("2026-02-01T00:00:00Z");
        let mut r = Range::new(None, cap);
        assert!(r.observe(&json!("2026-01-01T00:00:00Z")));
        assert!(!r.observe(&json!("2099-01-01T00:00:00Z")));
        assert!(
            r.observe(&json!("2026-02-01T00:00:00Z")),
            "the cap itself is kept"
        );
        assert_eq!(r.max_raw().as_deref(), Some("2026-02-01T00:00:00Z"));
        assert_eq!((r.future(), r.ignored()), (1, 0));
        // Numbers and text are not capped.
        let mut r = Range::new(None, cap);
        assert!(r.observe(&json!(4_102_444_800_u64)));
        assert!(r.observe(&json!("zzz")));
        assert_eq!(r.future(), 0);
        // Capped timestamps still outvote a stray text value.
        let mut r = Range::new(None, cap);
        r.observe(&json!("2099-01-01T00:00:00Z"));
        r.observe(&json!("N/A"));
        assert_eq!(r.max_raw(), None);
        assert_eq!((r.future(), r.ignored()), (1, 1));
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

    #[test]
    fn a_fingerprint_ignores_key_order_and_apisip_fields() {
        // Pinned: stored fingerprints must stay valid across releases.
        assert_eq!(fingerprint(&json!({"b": [2], "a": 1})), "ebd041daaff112f9");
        assert_eq!(
            fingerprint(&json!({"a": 1, "b": [2], "_apisip_extracted_at": "now"})),
            "ebd041daaff112f9"
        );
        assert_ne!(
            fingerprint(&json!({"a": 1, "b": {"_apisip_x": 1}})),
            fingerprint(&json!({"a": 1, "b": {}})),
            "only apisip's own top-level fields are left out"
        );
    }

    #[test]
    fn only_unchanged_records_at_the_watermark_are_skipped() {
        let a = json!({"id": 1, "name": "a", "updated_at": "2026-01-01T00:00:00Z"});
        let b = Boundary::new("2026-01-01T00:00:00Z", &[fingerprint(&a)]).unwrap();
        assert!(b.unchanged(&a, "updated_at"));
        let edited = json!({"id": 1, "name": "a2", "updated_at": "2026-01-01T00:00:00Z"});
        assert!(!b.unchanged(&edited, "updated_at"));
        let later = json!({"id": 1, "name": "a", "updated_at": "2026-01-01T00:00:01Z"});
        assert!(!b.unchanged(&later, "updated_at"));
        assert!(Boundary::new("2026-01-01T00:00:00Z", &[]).is_none());
    }

    #[test]
    fn the_tracker_keeps_the_records_at_the_highest_cursor() {
        let mut t = BoundaryTracker::default();
        for r in recs(&["2026-01-02", "2026-01-01", "2026-01-02", "2026-01-03"]) {
            t.observe(&r, &r["updated_at"]);
        }
        let top = recs(&["2026-01-03"]).pop().unwrap();
        assert_eq!(t.finish("2026-01-03"), vec![fingerprint(&top)]);

        let mut t = BoundaryTracker::default();
        for r in recs(&["2026-01-03"]) {
            t.observe(&r, &r["updated_at"]);
        }
        assert!(t.finish("2026-01-04").is_empty(), "not the committed max");
    }

    #[test]
    fn past_the_cap_no_fingerprint_is_kept() {
        let mut t = BoundaryTracker::default();
        for i in 0..=MAX_BOUNDARY {
            let r = json!({"id": i, "updated_at": "2026-01-01"});
            t.observe(&r, &r["updated_at"]);
        }
        assert!(t.overflow);
        assert!(t.finish("2026-01-01").is_empty());

        // A higher value starts over.
        let mut t = BoundaryTracker::default();
        for i in 0..=MAX_BOUNDARY {
            let r = json!({"id": i, "updated_at": "2026-01-01"});
            t.observe(&r, &r["updated_at"]);
        }
        let r = json!({"id": 0, "updated_at": "2026-01-02"});
        t.observe(&r, &r["updated_at"]);
        assert!(!t.overflow);
        assert_eq!(t.finish("2026-01-02").len(), 1);
    }

    // 2^53 + 1: the first integer an f64 cannot hold.
    const W: u64 = 9_007_199_254_740_993;

    fn ids(v: &[u64]) -> Vec<Value> {
        v.iter().map(|i| json!({"id": i})).collect()
    }

    #[test]
    fn integer_cursors_above_2_53_compare_exactly() {
        let at = |i: u64| CursorValue::from_json(&json!(i)).unwrap();
        assert!(at(W) > at(W - 1));
        assert_eq!(CursorValue::parse(&W.to_string()), at(W));
        assert!(
            CursorValue::parse("1761700000000000001") > CursorValue::parse("1761700000000000000")
        );
        assert!(
            CursorValue::from_json(&json!(i64::MIN)) < CursorValue::from_json(&json!(u64::MAX))
        );

        let mut r = Range::new(None, None);
        r.observe(&json!(W - 1));
        r.observe(&json!(W));
        assert_eq!(r.max_raw(), Some(W.to_string()));
        assert_eq!(r.min_raw(), Some((W - 1).to_string()));
    }

    #[test]
    fn integers_and_floats_compare_by_value() {
        let int = CursorValue::Int;
        let num = CursorValue::Number;
        assert_eq!(int(5), num(5.0));
        assert!(int(i128::from(W)) > num(9_007_199_254_740_992.0));
        assert!(num(9_007_199_254_740_992.0) < int(i128::from(W)));
        assert!(int(3) < num(3.5) && int(4) > num(3.5));
        assert!(int(-3) > num(-3.5));
        assert!(int(i128::MAX) < num(f64::INFINITY));
        assert!(int(i128::MIN) > num(f64::NEG_INFINITY));
        assert_eq!(int(1).partial_cmp(&num(f64::NAN)), None);
    }

    #[test]
    fn a_record_one_above_an_integer_watermark_is_newer() {
        let at = json!({"id": W});
        let b = Boundary::new(&W.to_string(), &[fingerprint(&at)]).unwrap();
        assert!(b.unchanged(&at, "id"));
        // As an f64, W rounds to W - 1. The fingerprint matches on purpose:
        // only the cursor can tell the record from the boundary.
        let next = json!({"id": W});
        let b = Boundary::new(&(W - 1).to_string(), &[fingerprint(&next)]).unwrap();
        assert!(!b.unchanged(&next, "id"));
    }

    #[test]
    fn early_stop_cuts_exactly_at_an_integer_watermark() {
        let mut g = EarlyStop::new("id", Some(&W.to_string()));
        let (kept, stop) = g.page(ids(&[W + 2, W + 1, W, W - 1]));
        assert_eq!((kept, stop), (ids(&[W + 2, W + 1, W]), false));
        assert!(!g.unreliable);
    }

    #[test]
    fn the_sort_guard_sees_an_increase_of_one_above_2_53() {
        let mut g = EarlyStop::new("id", Some(&(W - 10).to_string()));
        g.page(ids(&[W - 1, W]));
        assert!(
            g.unreliable,
            "an increase of 1 above 2^53 is a sort violation"
        );
    }

    #[test]
    fn the_tracker_keeps_only_the_exact_integer_max() {
        let mut t = BoundaryTracker::default();
        for r in ids(&[W, W - 1]) {
            t.observe(&r, &r["id"]);
        }
        assert_eq!(
            t.finish(&W.to_string()),
            vec![fingerprint(&json!({"id": W}))]
        );
    }
}
