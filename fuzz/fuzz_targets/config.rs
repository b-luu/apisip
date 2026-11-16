#![no_main]

use apisip::config::Config;
use libfuzzer_sys::fuzz_target;

// An arbitrary apisip.toml.
fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = Config::parse(text);
    }
});
