//! Golden-replay parity for the sniper decision engine: rebuild index+model,
//! reconstruct the 1000 decoded auctions from the golden, replay the exact
//! single-box pipeline (per-candidate snipe→median, then dominance+lbin over
//! lbinCandidates, with accumulating seen/relist state), and compare each
//! candidate's screen verdict + decision.

use finder_core::bazaar::Bazaar;
use finder_core::modifier_model::ModifierModel;
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{PriceIndex, Reference};
use finder_core::sniper::*;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

fn num_close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}
fn nv(x: f64) -> Value {
    serde_json::Number::from_f64(x)
        .map(Value::Number)
        .unwrap_or(Value::Null)
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
fn sniper_golden_parity() {
    let gdir = concat!(env!("CARGO_MANIFEST_DIR"), "/../goldens");
    let doc: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{gdir}/sniper/dump.json")).unwrap())
            .unwrap();
    let now_ms = doc["pinnedNowMs"].as_i64().unwrap() as f64;
    let last_updated = doc["source"]["lastUpdated"].as_f64().unwrap();

    let prices: HashMap<String, f64> = doc["bazaar"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_f64().unwrap()))
        .collect();
    let bazaar = Bazaar::from_prices(prices, now_ms as i64);
    let refs: Vec<Reference> = std::fs::read_to_string(format!("{gdir}/priceIndex/refs.jsonl"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let idx = PriceIndex::build(refs.clone(), bazaar, now_ms as i64);
    let model = ModifierModel::rebuild(&refs, &idx, now_ms as i64);

    // Reconstruct decoded auctions in golden order.
    let gcands = doc["candidates"].as_array().unwrap();
    let mut decoded: Vec<DecodedAuction> = Vec::with_capacity(gcands.len());
    let mut key_mismatch = 0usize;
    for c in gcands {
        let cand = &c["candidate"];
        let attrs: ItemAttributes = serde_json::from_value(cand["attrs"].clone()).unwrap();
        let a = ActiveAuction {
            uuid: cand["uuid"].as_str().unwrap().to_string(),
            starting_bid: cand["startingBid"].as_f64().unwrap(),
            auctioneer: cand["auctioneer"].as_str().map(String::from),
            item_name: cand["itemName"].as_str().unwrap_or("").to_string(),
        };
        let key = idx.final_key(&attrs);
        if key != cand["key"].as_str().unwrap() {
            key_mismatch += 1;
        }
        decoded.push(DecodedAuction { a, attrs, key });
    }
    assert_eq!(
        key_mismatch, 0,
        "{key_mismatch} finalKey mismatches vs golden"
    );

    // Full-dump live-BIN map (minor-adjusted), sorted price asc.
    let mut bykey: HashMap<String, Vec<Bin>> = HashMap::new();
    for d in &decoded {
        let price =
            (d.a.starting_bid - idx.minor_feature_value(&d.attrs)).max(d.a.starting_bid * 0.5);
        bykey.entry(d.key.clone()).or_default().push(Bin {
            uuid: d.a.uuid.clone(),
            price,
        });
    }
    for list in bykey.values_mut() {
        list.sort_by(|x, y| x.price.partial_cmp(&y.price).unwrap());
    }

    // Pipeline.
    let mut seen: HashSet<String> = HashSet::new();
    let mut relist = RelistTracker::default();
    let mut fired: HashMap<String, Flip> = HashMap::new();
    let mut screen_by: HashMap<String, char> = HashMap::new();
    let mut went_to_lbin: HashSet<String> = HashSet::new();
    let mut lbin_candidates: Vec<DecodedAuction> = Vec::new();

    for d in &decoded {
        screen_by.insert(d.a.uuid.clone(), screen_auction(d, &idx, &model));
        if let Some(f) = eval_clean_snipe(
            d,
            &idx,
            &bykey,
            &mut seen,
            &mut relist,
            last_updated,
            now_ms,
        ) {
            fired.insert(d.a.uuid.clone(), f);
            continue;
        }
        let (flip, priceable) = eval_median_flip(
            d,
            &idx,
            &model,
            &bykey,
            &mut seen,
            &mut relist,
            last_updated,
            now_ms,
        );
        if let Some(f) = flip {
            fired.insert(d.a.uuid.clone(), f);
            continue;
        }
        if !priceable {
            went_to_lbin.insert(d.a.uuid.clone());
            lbin_candidates.push(d.clone());
        }
    }
    for f in eval_dominance_flips(
        &lbin_candidates,
        &bykey,
        &mut seen,
        &mut relist,
        last_updated,
        &idx,
        now_ms,
    ) {
        fired.insert(f.uuid.clone(), f);
    }
    for f in eval_lbin_flips(
        &lbin_candidates,
        &bykey,
        &mut seen,
        &mut relist,
        last_updated,
        &idx,
        now_ms,
    ) {
        fired.insert(f.uuid.clone(), f);
    }

    // Summary sanity.
    if let Some(s) = doc["summary"].as_object() {
        assert_eq!(
            fired.len() as u64,
            s["flipsTotal"].as_u64().unwrap(),
            "flipsTotal"
        );
        assert_eq!(
            lbin_candidates.len() as u64,
            s["lbinCandidates"].as_u64().unwrap(),
            "lbinCandidates"
        );
        let mut sc = HashMap::new();
        for v in screen_by.values() {
            *sc.entry(*v).or_insert(0u64) += 1;
        }
        for (k, want) in s["screenCounts"].as_object().unwrap() {
            let got = *sc.get(&k.chars().next().unwrap()).unwrap_or(&0);
            assert_eq!(got, want.as_u64().unwrap(), "screenCount {k}");
        }
    }

    let reject_reason = |d: &DecodedAuction| -> String {
        if screen_by.get(&d.a.uuid) == Some(&'r') {
            "screen_reject".into()
        } else if went_to_lbin.contains(&d.a.uuid) {
            "lbin_candidate_no_flip".into()
        } else {
            "priceable_no_flip".into()
        }
    };

    let mut mismatches = 0usize;
    let mut examples: Vec<String> = Vec::new();
    for (i, c) in gcands.iter().enumerate() {
        let d = &decoded[i];
        let want_screen = c["screen"].as_str().unwrap();
        let got_screen = screen_by[&d.a.uuid].to_string();
        let decision = if let Some(f) = fired.get(&d.a.uuid) {
            serde_json::json!({
                "accept": true, "lane": f.finder, "targetPrice": nv(f.reference),
                "price": nv(f.price), "profit": nv(f.profit), "roiPct": nv(f.roi_pct),
                "confidence": nv(f.confidence), "samples": f.samples, "key": f.key,
                "guard": f.guard, "foundAfterRefreshMs": nv(f.found_after_refresh_ms),
                "foundAtMs": nv(f.found_at_ms),
            })
        } else {
            serde_json::json!({"accept": false, "lane": Value::Null, "reason": reject_reason(d)})
        };
        if got_screen != want_screen || !json_eq(&decision, &c["decision"]) {
            mismatches += 1;
            if examples.len() < 8 {
                examples.push(format!(
                    "id={} screen got {got_screen} want {want_screen}\n   got={decision}\n   want={}",
                    d.attrs.id, c["decision"]
                ));
            }
        }
    }
    // CONF_MARGIN_PENALTY < 1 deliberately changes confidence on high-ROI flips,
    // which is the whole point of the knob. This golden asserts TS PARITY, so it
    // only holds at the original strength.
    if *finder_core::config::CONF_MARGIN_PENALTY < 1.0 {
        eprintln!(
            "CONF_MARGIN_PENALTY={}: skipping TS-parity assertion ({mismatches} differ by design)",
            *finder_core::config::CONF_MARGIN_PENALTY
        );
        return;
    }
    for ex in &examples {
        eprintln!("--- MISMATCH {ex}");
    }
    assert_eq!(
        mismatches,
        0,
        "{mismatches}/{} sniper candidates mismatched",
        gcands.len()
    );
}
