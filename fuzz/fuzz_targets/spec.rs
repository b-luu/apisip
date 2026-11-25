#![no_main]

use apisip::detect::{detect, stream_record_schema};
use apisip::schema::RecordSchema;
use apisip::spec::Spec;
use libfuzzer_sys::fuzz_target;

// A spec as JSON or YAML, then detection and the record schema of each
// stream, which follow `$ref`s. The schema then conforms fuzzed data: the
// item schema itself and the whole spec, both nested JSON.
fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(spec) = Spec::parse(text, "fuzz.yaml") else {
        return;
    };
    let detection = detect(&spec);
    for s in &detection.streams {
        if let Some(item) = stream_record_schema(&spec, &s.config)
            && let Some(schema) = RecordSchema::from_spec(&item)
        {
            schema.conform(&item);
            schema.conform(&spec.root);
        }
    }
});
