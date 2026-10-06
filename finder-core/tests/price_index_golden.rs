//! Golden-replay parity for the priceIndex money core: build the index from the
//! 30,942-ref slice with the pinned clock + fixed bazaar, then replay all 267
//! queries and compare every output field numerically.

use finder_core::bazaar::Bazaar;
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{base_key, candidate_features, PriceIndex, Reference};
use serde_json::Value;
use std::collections::HashMap;

fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            // Tight relative tolerance (1e-9) to absorb last-ULP float→ordering
            // drift in derived ratios (confidence/volumePerDay/trendPct); prices
            // are exact so they pass at 0 diff. Well inside PORT.md's ≤0.01%.
            (Some(x), Some(y)) => {
                if x.is_nan() && y.is_nan() {
                    true
                } else {
                    (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0)
                }
            }
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

/// f64 → JSON number (NaN/±Inf → null; the golden encodes those as sentinels, so
/// a mismatch is reported rather than a panic).
fn nv(x: f64) -> Value {
    serde_json::Number::from_f64(x)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn opt_nv(x: Option<f64>) -> Value {
    x.map(nv).unwrap_or(Value::Null)
}

fn to_val<T: serde::Serialize>(x: Option<T>) -> Value {
    match x {
        Some(v) => serde_json::to_value(v).unwrap(),
        None => Value::Null,
    }
}

#[test]
fn price_index_golden_parity() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../goldens/priceIndex");
    let doc: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{dir}/queries.json")).unwrap())
            .unwrap();
    let now_ms = doc["pinnedNowMs"].as_i64().unwrap();

    // bazaar map (input); lastRefresh = pinnedNowMs so bazaarReady() is true.
    let prices: HashMap<String, f64> = doc["bazaar"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_f64().unwrap()))
        .collect();
    let bazaar = Bazaar::from_prices(prices, now_ms);

    // refs slice.
    let refs: Vec<Reference> = std::fs::read_to_string(format!("{dir}/refs.jsonl"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("ref line parses"))
        .collect();
    eprintln!("loaded {} refs", refs.len());

    let idx = PriceIndex::build(refs, bazaar, now_ms);

    // keyCount sanity (warn first; query diffs below pinpoint the cause).
    if let Some(kc) = doc["keyCount"].as_u64() {
        if idx.key_count() as u64 != kc {
            eprintln!("WARN keyCount: got {} want {kc}", idx.key_count());
        }
    }

    let queries = doc["queries"].as_array().unwrap();
    let mut mismatches = 0usize;
    let mut examples: Vec<String> = Vec::new();
    for q in queries {
        let a: ItemAttributes = serde_json::from_value(q["input"].clone()).expect("input parses");
        let bk = base_key(&a);
        let fk = idx.final_key(&a);
        // The golden dumps sigFeatures sorted (order is non-semantic — finalKey
        // sorts it before use). Sort ours for comparison; the library stays faithful.
        let mut sig = idx.sig_features(&a);
        sig.sort();
        let got = serde_json::json!({
            "baseKey": bk,
            "finalKey": fk,
            "candidateFeatures": candidate_features(&a),
            "sigFeatures": sig,
            "sigSignature": idx.sig_signature(&bk),
            "minorFeatureValue": nv(idx.minor_feature_value(&a)),
            "adjustedPriceAt100M": nv(idx.adjusted_price(100_000_000.0, &a)),
            "baseValueForBaseKey": nv(idx.base_value_for(&bk)),
            "baseValueForId": nv(idx.base_value_for(&a.id)),
            "highForBase": nv(idx.high_for_base(&bk)),
            "soldCountForBase": idx.sold_count_for_base(&bk),
            "cheapMedian": opt_nv(idx.cheap_median(&fk)),
            "thinKeyEvidence": to_val(idx.thin_key_evidence(&fk)),
            "baseTrendPct": nv(idx.base_trend_pct(&bk)),
            "zeroStarBaseline": opt_nv(idx.zero_star_baseline(&a)),
            "cleanSnipe": to_val(idx.clean_snipe(&a)),
            "priceFor": to_val(idx.price_for(&a)),
            "dominanceFloor": to_val(idx.dominance_floor(&a)),
        });
        let want = &q["output"];
        if !json_eq(&got, want) {
            mismatches += 1;
            if examples.len() < 8 {
                // show only the fields that differ
                let mut diff = serde_json::Map::new();
                if let (Some(g), Some(w)) = (got.as_object(), want.as_object()) {
                    for (k, gv) in g {
                        let wv = w.get(k).unwrap_or(&Value::Null);
                        if !json_eq(gv, wv) {
                            diff.insert(k.clone(), serde_json::json!({"got": gv, "want": wv}));
                        }
                    }
                }
                examples.push(format!(
                    "set={} bk={bk}: {}",
                    q["set"].as_str().unwrap_or("?"),
                    Value::Object(diff)
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
        "{mismatches}/{} priceIndex queries mismatched",
        queries.len()
    );
}
