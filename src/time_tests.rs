//! Golden tests for every time and duration format apisip reads or writes.
//! They pin the outputs of the date-time and duration crates apisip uses, so
//! a change of crate cannot change a file, a path or a config value.

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};

use crate::config::Config;
use crate::http::retry::retry_after;
use crate::schema::parse_timestamp;
use crate::sink::Layout;
use crate::sink::writer::Stamp;
use crate::state::{Manifest, State};

#[test]
fn state_timestamps_load_and_write_back_as_before() {
    for at in [
        "2026-11-08T21:30:00Z",
        "2026-11-08T21:30:00.123Z",
        "2026-11-08T21:30:00.123456Z",
        "2026-11-08T21:30:00.123456789Z",
    ] {
        let text = format!(r#"{{"streams":{{"users":{{"last_committed_at":"{at}"}}}}}}"#);
        let state: State = serde_json::from_str(&text).unwrap();
        assert_eq!(serde_json::to_string(&state).unwrap(), text);
    }
    // Inputs another writer could leave: written back in the canonical form.
    for (at, written) in [
        (
            "2026-11-08T21:30:00.1234567Z",
            "2026-11-08T21:30:00.123456700Z",
        ),
        ("2026-11-08T21:30:00.120Z", "2026-11-08T21:30:00.120Z"),
        ("2026-11-08T21:30:00.100000Z", "2026-11-08T21:30:00.100Z"),
        ("2026-11-08T21:30:00+00:00", "2026-11-08T21:30:00Z"),
        ("2026-11-08T22:30:00+01:00", "2026-11-08T21:30:00Z"),
    ] {
        let text = format!(r#"{{"streams":{{"users":{{"last_committed_at":"{at}"}}}}}}"#);
        let state: State = serde_json::from_str(&text).unwrap();
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            text.replace(at, written),
            "{at}"
        );
    }
}

#[test]
fn manifest_timestamps_load_and_write_back_as_before() {
    let text = r#"{"run_id":"20261108T213000123Z-00ab","started_at":"2026-11-08T21:30:00.123Z","finished_at":"2026-11-08T21:31:00.000001Z","status":"success","full_refresh":false,"format":"jsonl","streams":{}}"#;
    let m: Manifest = serde_json::from_str(text).unwrap();
    assert_eq!(serde_json::to_string(&m).unwrap(), text);
}

fn duration_of(text: &str) -> Option<Duration> {
    Config::parse(&format!(
        "[source]\nspec='a'\nbase_url='b'\nrate_limit = {{ max_calls = 1, period = {text:?} }}\n[destination]\n"
    ))
    .ok()
    .map(|c| c.source.rate_limit.unwrap().period)
}

/// What each spelling the old duration parser accepted loads to.
pub(crate) const DURATIONS: &[(&str, u64, u32)] = &[
    ("0", 0, 0),
    ("1ns", 0, 1),
    ("1nsec", 0, 1),
    ("1usec", 0, 1_000),
    ("1us", 0, 1_000),
    ("1ms", 0, 1_000_000),
    ("1msec", 0, 1_000_000),
    ("1s", 1, 0),
    ("1sec", 1, 0),
    ("1second", 1, 0),
    ("2seconds", 2, 0),
    ("1m", 60, 0),
    ("1min", 60, 0),
    ("1minute", 60, 0),
    ("2minutes", 120, 0),
    ("1h", 3_600, 0),
    ("1hr", 3_600, 0),
    ("1hour", 3_600, 0),
    ("2hours", 7_200, 0),
    ("1d", 86_400, 0),
    ("1day", 86_400, 0),
    ("2days", 172_800, 0),
    ("1w", 604_800, 0),
    ("1week", 604_800, 0),
    ("2weeks", 1_209_600, 0),
    ("1M", 2_630_016, 0),
    ("1month", 2_630_016, 0),
    ("2months", 5_260_032, 0),
    ("1y", 31_557_600, 0),
    ("1year", 31_557_600, 0),
    ("2years", 63_115_200, 0),
    ("2h 30m", 9_000, 0),
    ("1h30m", 5_400, 0),
    ("15 minutes", 900, 0),
    ("7days", 604_800, 0),
];

#[test]
fn config_durations_load_to_the_same_value() {
    for (text, secs, nanos) in DURATIONS {
        assert_eq!(
            duration_of(text),
            Some(Duration::new(*secs, *nanos)),
            "{text}"
        );
    }
    for text in ["", "1", "m", "1x", "-1s"] {
        assert_eq!(duration_of(text), None, "{text:?}");
    }
}

#[test]
fn timestamp_cursors_parse_as_before() {
    let at = |s: &str| Some(s.parse().unwrap());
    for (text, utc) in [
        ("2026-01-01T00:00:00Z", "2026-01-01T00:00:00Z"),
        ("2026-01-01T00:00:00z", "2026-01-01T00:00:00Z"),
        ("2026-01-01T05:30:00+05:30", "2026-01-01T00:00:00Z"),
        ("2026-01-01T00:00:00.5Z", "2026-01-01T00:00:00.5Z"),
        (
            "2026-01-01T00:00:00.123456789Z",
            "2026-01-01T00:00:00.123456789Z",
        ),
        ("2026-01-01 00:00:00Z", "2026-01-01T00:00:00Z"),
        ("2026-01-01T00:00:00", "2026-01-01T00:00:00Z"),
        ("2026-01-01T00:00:00.25", "2026-01-01T00:00:00.25Z"),
        ("2026-01-01 00:00:00", "2026-01-01T00:00:00Z"),
        ("2026-01-01 00:00:00.000001", "2026-01-01T00:00:00.000001Z"),
        ("2026-01-01t00:00:00Z", "2026-01-01T00:00:00Z"),
        ("2026-01-01T00:00:00-00:00", "2026-01-01T00:00:00Z"),
        ("2026-01-01T00:00:00.123+05:30", "2025-12-31T18:30:00.123Z"),
        // The offset-less forms are lenient about widths and spaces.
        ("2026-1-1T00:00:00", "2026-01-01T00:00:00Z"),
        ("2026-01-01T0:0:0", "2026-01-01T00:00:00Z"),
        ("+2026-01-01T00:00:00", "2026-01-01T00:00:00Z"),
        ("2026-01-01  00:00:00", "2026-01-01T00:00:00Z"),
        (" 2026-01-01T00:00:00", "2026-01-01T00:00:00Z"),
        ("0999-01-01T00:00:00", "0999-01-01T00:00:00Z"),
    ] {
        assert_eq!(parse_timestamp(text), at(utc), "{text}");
    }
    // A leap second reads as the last second of the minute or the next one,
    // depending on the parser; it only has to parse.
    assert!(parse_timestamp("2016-12-31T23:59:60Z").is_some());
    for text in [
        "2026-01-01T00:00Z",
        "20260101T000000Z",
        "2026-01-01T00:00:00Z[UTC]",
        "2026-01-01",
        "2026-01-01T00:00:00.",
        "2026-01-01T00:00:00 Z",
        "2026-01-01T00:00:00+0530",
        "12026-01-01T00:00:00",
        "2026-01-01T24:00:00",
        "2026-01-01T00:00:00 ",
        "N/A",
        "",
    ] {
        assert_eq!(parse_timestamp(text), None, "{text}");
    }
}

#[test]
fn run_ids_and_partitions_keep_their_format() {
    let started = "2026-11-08T21:30:05.123456789Z".parse().unwrap();
    let layout = Layout::new(started);
    let (prefix, suffix) = layout.run_id.split_once('-').unwrap();
    assert_eq!(prefix, "20261108T213005123Z");
    assert_eq!(suffix.len(), 4);
    assert_eq!(
        Layout::started_at(&layout.run_id),
        Some("2026-11-08T21:30:05.123Z".parse().unwrap())
    );
    assert_eq!(layout.data_dir("users", false), "users/dt=2026-11-08");
    assert_eq!(Layout::started_at("not-a-run"), None);
}

#[test]
fn extracted_at_keeps_microseconds_and_z() {
    for (at, text) in [
        (
            "2026-11-08T21:30:05.123456789Z",
            "2026-11-08T21:30:05.123456Z",
        ),
        ("2026-11-08T21:30:05Z", "2026-11-08T21:30:05.000000Z"),
    ] {
        let stamp = Stamp {
            run_id: "r",
            extracted: at.parse().unwrap(),
        };
        assert_eq!(stamp.extracted_text(), text);
    }
}

#[test]
fn retry_after_reads_http_dates() {
    let mut h = HeaderMap::new();
    for v in [
        "Wed, 21 Oct 2015 07:28:00 GMT",
        "Wed, 21 Oct 2015 07:28:00 +0000",
    ] {
        h.insert("retry-after", HeaderValue::from_static(v));
        assert_eq!(retry_after(&h), Some(Duration::ZERO), "{v}");
    }
    h.insert("retry-after", HeaderValue::from_static("soon"));
    assert_eq!(retry_after(&h), None);
}

#[test]
fn timestamp_cursors_keep_the_old_edge_cases() {
    let at = |s: &str| Some(s.parse().unwrap());
    for (text, utc) in [
        ("26-01-24 12:00:32", "0026-01-24T12:00:32Z"),
        ("-0001-1-13 00:29:32", "-000001-01-13T00:29:32Z"),
        ("2026-1-12T 00:32:24", "2026-01-12T00:32:24Z"),
        ("2026-01-0100:00:00", "2026-01-01T00:00:00Z"),
        ("2026- 1- 2 3: 4: 5", "2026-01-02T03:04:05Z"),
    ] {
        assert_eq!(parse_timestamp(text), at(utc), "{text}");
    }
    for text in [
        "2026-01-01T00:00:00.\u{661}\u{662}\u{663}\u{664}\u{665}Z",
        "\u{662}\u{660}\u{662}\u{666}-01-01T00:00:00",
        "2026-01-01T00:00:00+24:00",
        "2026-01-01T00:00:00-25:00",
        "2026-01-23:02:32",
    ] {
        assert_eq!(parse_timestamp(text), None, "{text}");
    }
}
