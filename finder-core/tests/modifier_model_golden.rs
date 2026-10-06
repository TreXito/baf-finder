//! Golden-replay parity for modifierModel: rebuild the model over the ref slice,
//! then estimateFor each query. Also checks model item count (modelStats.items).

use finder_core::bazaar::Bazaar;
use finder_core::modifier_model::ModifierModel;
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{base_key, PriceIndex, Reference};
use serde_json::Value;
use std::collections::HashMap;

fn num_close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => num_close(x, y),
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
fn modifier_model_golden_parity() {
    // priceIndex slice (same refs/bazaar/clock).
    let pi_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../goldens/priceIndex");
    let pdoc: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{pi_dir}/queries.json")).unwrap())
            .unwrap();
    let now_ms = pdoc["pinnedNowMs"].as_i64().unwrap();
    let prices: HashMap<String, f64> = pdoc["bazaar"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_f64().unwrap()))
        .collect();
    let bazaar = Bazaar::from_prices(prices, now_ms);
    let refs: Vec<Reference> = std::fs::read_to_string(format!("{pi_dir}/refs.jsonl"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let idx = PriceIndex::build(refs.clone(), bazaar, now_ms);
    let model = ModifierModel::rebuild(&refs, &idx, now_ms);

    let doc: Value = serde_json::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../goldens/modifierModel/estimates.json"
        ))
        .unwrap(),
    )
    .unwrap();
    if let Some(items) = doc["modelStats"]["items"].as_u64() {
        assert_eq!(
            model.model_count() as u64,
            items,
            "model item count mismatch"
        );
    }

    let queries = doc["queries"].as_array().unwrap();
    let mut mismatches = 0usize;
    let mut examples: Vec<String> = Vec::new();
    for q in queries {
        let a: ItemAttributes = serde_json::from_value(q["input"].clone()).unwrap();
        // NOTE: unlike the priceIndex golden, the modifierModel golden dumps
        // sigFeatures in raw candidateFeatures order (unsorted).
        let got = serde_json::json!({
            "baseKey": base_key(&a),
            "sigFeatures": idx.sig_features(&a),
            "estimateFor": match model.estimate_for(&a, &idx) {
                Some(e) => serde_json::to_value(e).unwrap(),
                None => Value::Null,
            },
        });
        if !json_eq(&got, &q["output"]) {
            mismatches += 1;
            if examples.len() < 8 {
                examples.push(format!(
                    "set={} out got={} want={}",
                    q["set"], got, q["output"]
                ));
            }
        }
    }
    for ex in &examples {
        eprintln!("--- MISMATCH {ex}");
    }
    assert_eq!(
        mismatches,
        0,
        "{mismatches}/{} modifierModel queries mismatched",
        queries.len()
    );
}
