//! Golden-replay parity for the nbt port: decode every real `item_bytes` in
//! `goldens/nbt/page0.json` and assert the resulting `ItemAttributes` equals the
//! attrs the real TS produced. Comparison is numeric-aware (int vs float and map
//! key order never cause a false mismatch).

use finder_core::nbt::decode_item_bytes;
use serde_json::Value;

/// Semantic JSON equality: numbers compared by f64 value (so 5 == 5.0), object
/// key order ignored, arrays order-sensitive (the TS sorts them, so must we).
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => x == y || (x.is_nan() && y.is_nan()),
            _ => false,
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| json_eq(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).map(|w| json_eq(v, w)).unwrap_or(false))
        }
        _ => false,
    }
}

#[test]
fn nbt_golden_parity() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../goldens/nbt/page0.json");
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("golden present"))
        .expect("golden parses");
    let entries = doc["entries"].as_array().expect("entries array");
    assert!(entries.len() >= 900, "expected the full page-0 golden");

    let mut mismatches = 0usize;
    let mut examples: Vec<String> = Vec::new();
    for (idx, e) in entries.iter().enumerate() {
        let ib = e["item_bytes"].as_str().expect("item_bytes string");
        let expected = &e["attrs"];
        let got = match decode_item_bytes(ib) {
            Some(a) => serde_json::to_value(&a).unwrap(),
            None => Value::Null,
        };
        if !json_eq(&got, expected) {
            mismatches += 1;
            if examples.len() < 6 {
                examples.push(format!(
                    "idx {idx} id={:?}\n  exp={}\n  got={}",
                    expected.get("id"),
                    expected,
                    got
                ));
            }
        }
    }
    // NBT_EXTRA_FIELDS reads ExtraAttributes keys the TS parser never looked at,
    // so any fixture carrying one gains an `extras` entry by design. This golden
    // asserts TS PARITY, so it only holds in the default (off) mode. NBT_COUNT is
    // NOT excused here: `count` is omitted from the wire unless it is a stack,
    // and no page-0 fixture is stacked, so the shape must still match exactly.
    if *finder_core::config::NBT_EXTRA_FIELDS {
        eprintln!("NBT_EXTRA_FIELDS=1: skipping TS-parity assertion ({mismatches} entries differ by design)");
        return;
    }
    for ex in &examples {
        eprintln!("--- MISMATCH {ex}");
    }
    assert_eq!(
        mismatches,
        0,
        "{mismatches}/{} nbt golden entries mismatched",
        entries.len()
    );
}
