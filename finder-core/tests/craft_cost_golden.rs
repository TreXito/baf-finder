//! Golden-replay parity for craftCost: craftCeiling / zeroStarBaseline over the
//! same reference slice, plus the starMaterialCost(0..12) table.

use finder_core::bazaar::Bazaar;
use finder_core::craft_cost::{craft_ceiling, star_material_cost};
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{base_key, PriceIndex, Reference};
use serde_json::Value;
use std::collections::HashMap;

fn num_close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

/// Compare an f64-or-null output against the golden value.
fn opt_eq(got: Option<f64>, want: &Value) -> bool {
    match (got, want) {
        (None, Value::Null) => true,
        (Some(g), Value::Number(n)) => n.as_f64().map(|w| num_close(g, w)).unwrap_or(false),
        _ => false,
    }
}

fn build_index(dir: &str) -> (PriceIndex, Value) {
    let doc: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/queries.json")).unwrap())
            .unwrap();
    let now_ms = doc["pinnedNowMs"].as_i64().unwrap();
    let prices: HashMap<String, f64> = doc["bazaar"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_f64().unwrap()))
        .collect();
    let bazaar = Bazaar::from_prices(prices, now_ms);
    let refs: Vec<Reference> = std::fs::read_to_string(format!("{dir}/refs.jsonl"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    (PriceIndex::build(refs, bazaar, now_ms), doc)
}

#[test]
fn craft_cost_golden_parity() {
    // Reuse the priceIndex slice/bazaar (same snapshot, cutoff, pinned clock).
    let (idx, _) = build_index(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../goldens/priceIndex"
    ));

    let doc: Value = serde_json::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../goldens/craftCost/matrix.json"
        ))
        .unwrap(),
    )
    .unwrap();

    // starMaterialCost table.
    for row in doc["starMaterialCostTable"].as_array().unwrap() {
        let level = row["level"].as_f64().unwrap();
        let want = row["cost"].as_f64().unwrap();
        let got = star_material_cost(level, &idx);
        assert!(
            num_close(got, want),
            "starMaterialCost({level}) got {got} want {want}"
        );
    }

    let entries = doc["entries"].as_array().unwrap();
    let mut mismatches = 0usize;
    let mut examples: Vec<String> = Vec::new();
    for e in entries {
        let a: ItemAttributes = serde_json::from_value(e["input"].clone()).unwrap();
        let out = &e["output"];
        let bk = base_key(&a);
        let mut bad = Vec::new();
        if bk != out["baseKey"].as_str().unwrap() {
            bad.push(format!("baseKey got {bk} want {}", out["baseKey"]));
        }
        if !opt_eq(craft_ceiling(&a, &idx), &out["craftCeiling"]) {
            bad.push(format!(
                "craftCeiling got {:?} want {}",
                craft_ceiling(&a, &idx),
                out["craftCeiling"]
            ));
        }
        if !opt_eq(idx.zero_star_baseline(&a), &out["zeroStarBaseline"]) {
            bad.push(format!(
                "zeroStarBaseline got {:?} want {}",
                idx.zero_star_baseline(&a),
                out["zeroStarBaseline"]
            ));
        }
        if !num_close(
            idx.base_value_for(&a.id),
            out["baseValueForId"].as_f64().unwrap(),
        ) {
            bad.push(format!(
                "baseValueForId got {} want {}",
                idx.base_value_for(&a.id),
                out["baseValueForId"]
            ));
        }
        if !bad.is_empty() {
            mismatches += 1;
            if examples.len() < 8 {
                examples.push(format!("id={} : {}", a.id, bad.join("; ")));
            }
        }
    }
    // VARIANT_SIG deliberately diverges from TS: collapsing a worthless variant
    // into the base key means sales of e.g. "Hyperion + 1k rune" now count as
    // evidence for what a Hyperion is worth, which nudges base values up ~1-2%.
    // This golden asserts TS PARITY, so it only holds in the default (off) mode.
    if *finder_core::config::VARIANT_SIG {
        eprintln!(
            "VARIANT_SIG=1: skipping TS-parity assertion ({mismatches} entries differ by design)"
        );
        return;
    }
    for ex in &examples {
        eprintln!("--- MISMATCH {ex}");
    }
    assert_eq!(
        mismatches,
        0,
        "{mismatches}/{} craftCost entries mismatched",
        entries.len()
    );
}
