//! Golden-replay parity for `attrsFromInventorySlot` (the /trex sellinv path):
//! replay every real inventory-slot JSON in `goldens/inventorySlot/slots.json`
//! through `attrs_from_inventory_slot` and assert the resulting `ItemAttributes`
//! equals what the TS produced. The generator already proved slot == item_bytes
//! decode in TS (mismatchVsDecode=0), so this closes the loop: Rust JSON path ==
//! TS JSON path == item_bytes decode.

use finder_core::nbt::attrs_from_inventory_slot;
use serde_json::Value;

/// Numeric-aware JSON equality (5 == 5.0, NaN == NaN, object key order ignored).
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
fn inventory_slot_golden_parity() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../goldens/inventorySlot/slots.json"
    );
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("golden present"))
        .expect("golden parses");
    assert_eq!(
        doc["source"]["mismatchVsDecode"].as_i64(),
        Some(0),
        "generator sanity: slot path must equal item_bytes decode"
    );
    let entries = doc["entries"].as_array().expect("entries array");
    assert!(
        entries.len() >= 900,
        "expected the full page-0 inventory golden"
    );

    let mut mismatches = 0usize;
    let mut examples: Vec<String> = Vec::new();
    for (idx, e) in entries.iter().enumerate() {
        let slot = &e["slot"];
        let expected = &e["attrs"];
        let got = match attrs_from_inventory_slot(slot) {
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
    // NBT_EXTRA_FIELDS deliberately diverges from TS: it reads ExtraAttributes
    // keys the TS parser never looked at, so an item carrying one (this fixture
    // has a BUCKET_OF_DYE with `dye_donated`) gains an `extras` entry. This
    // golden asserts TS PARITY, so it only holds in the default (off) mode.
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
        "{mismatches}/{} inventory-slot golden entries mismatched",
        entries.len()
    );
}
