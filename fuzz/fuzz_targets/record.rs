#![no_main]

use std::sync::LazyLock;

use apisip::config::Cast;
use apisip::schema::RecordSchema;
use libfuzzer_sys::fuzz_target;
use serde_json::{Value, json};

// One shape of each kind the spec can declare.
static SCHEMA: LazyLock<RecordSchema> = LazyLock::new(|| {
    RecordSchema::from_spec(&json!({
        "type": "object",
        "properties": {
            "id": {"type": "integer"},
            "price": {"type": "number"},
            "name": {"type": "string"},
            "active": {"type": "boolean"},
            "created_at": {"type": "string", "format": "date-time"},
            "day": {"type": "string", "format": "date"},
            "tags": {"type": "array", "items": {"type": "string"}},
            "owner": {
                "type": "object",
                "properties": {
                    "id": {"type": "integer"},
                    "scores": {"type": "array", "items": {"type": "number"}}
                }
            },
            "meta": {}
        }
    }))
    .expect("an object schema")
});

// An API record conformed to a spec schema under each cast mode, and to a
// schema inferred from the record itself.
fuzz_target!(|data: &[u8]| {
    let Ok(record) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    for cast in [Cast::Lossless, Cast::Exact, Cast::None] {
        let mut schema = SCHEMA.clone();
        schema.cast = cast;
        schema.conform(&record);
    }
    let inferred = RecordSchema::infer(std::slice::from_ref(&record));
    inferred.conform(&record);
});
