#![no_main]

use arbitrary::{Arbitrary, Error, Unstructured};
use libfuzzer_sys::fuzz_target;
use std::collections::HashMap;

extern crate fwd;
use fwd::server::refresh::docker::JsonValue;

/// InputNumber is a JSON number, i.e., a finite 64-bit floating point value
/// that is not NaN. We need to define our own little wrapper here so that we
/// can convince Arbitrary to only make finite f64s.
///
/// Ideally we would actually wrap serde_json::Number but there are rules
/// about mixing 3rd party traits with 3rd party types.
#[derive(Debug, PartialEq)]
struct InputNumber(f64);

impl<'a> Arbitrary<'a> for InputNumber {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self, Error> {
        let value = f64::arbitrary(u)?;
        if value.is_finite() {
            Ok(InputNumber(value))
        } else {
            Err(Error::IncorrectFormat) // REJECT
        }
    }

    #[inline]
    fn size_hint(depth: usize) -> (usize, Option<usize>) {
        f64::size_hint(depth)
    }
}

/// TestInput is basically serde_json::Value, except (a) it has a HashMap and
/// not serde_json's special `Map` structure, and (b) it has `InputNumber`
/// instead of `json_serde::Number` for reasons described above.
#[derive(Debug, PartialEq, Arbitrary)]
enum TestInput {
    Null,
    Bool(bool),
    Number(InputNumber),
    String(String),
    Object(HashMap<String, TestInput>),
    Array(Vec<TestInput>),
}

fn convert(value: &TestInput) -> serde_json::Value {
    match value {
        TestInput::Null => serde_json::Value::Null,
        TestInput::Bool(b) => serde_json::Value::Bool(*b),
        TestInput::Number(n) => serde_json::Value::Number(
            serde_json::Number::from_f64(n.0).expect("Unable to make an f64"),
        ),
        TestInput::String(s) => serde_json::Value::String(s.clone()),
        TestInput::Object(o) => {
            let mut out = serde_json::map::Map::new();
            for (k, v) in o.into_iter() {
                out.insert(k.clone(), convert(v));
            }
            serde_json::Value::Object(out)
        }
        TestInput::Array(v) => {
            serde_json::Value::Array(v.into_iter().map(convert).collect())
        }
    }
}

fuzz_target!(|data: TestInput| {
    // Convert the arbitrary TestInput into an arbitrary serde_json::Value,
    // then use serde_json to write out arbitrary JSON.
    let converted = convert(&data).to_string();

    // Parse the JSON that serde_json produced. This fuzz test should ensure
    // that we can parse anything that serde_json can produce.
    let _ = JsonValue::parse(converted.as_bytes());
});
