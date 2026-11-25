#![no_main]

use std::cmp::Ordering;

use apisip::incremental::CursorValue;
use apisip::schema::parse_timestamp;
use libfuzzer_sys::fuzz_target;

// Values of different kinds compare as text, which is not consistent with
// the numeric and time orderings, so only same-kind values are checked.
fn kind(v: &CursorValue) -> u8 {
    match v {
        CursorValue::Time(_) => 0,
        CursorValue::Int(_) | CursorValue::Number(_) => 1,
        CursorValue::Text(_) => 2,
    }
}

// Up to three cursor values, one per line: each must parse, and the
// ordering must be antisymmetric and transitive.
fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let values: Vec<CursorValue> = text
        .splitn(3, '\n')
        .map(|s| {
            parse_timestamp(s);
            CursorValue::parse(s)
        })
        .collect();
    for a in &values {
        for b in &values {
            if kind(a) != kind(b) {
                continue;
            }
            assert_eq!(
                a.partial_cmp(b),
                b.partial_cmp(a).map(Ordering::reverse),
                "{a:?} vs {b:?}"
            );
            for c in &values {
                if kind(a) == kind(c) && a < b && b < c {
                    assert!(a < c, "{a:?} < {b:?} < {c:?} but not {a:?} < {c:?}");
                }
            }
        }
    }
});
