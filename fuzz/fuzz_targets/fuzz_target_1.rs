#![no_main]

use libfuzzer_sys::fuzz_target;

extern crate fwd;
use fwd::server::refresh::docker::JsonValue;

fuzz_target!(|data: &[u8]| {
    // fuzzed code goes here
    let _ = JsonValue::parse(data);
});
