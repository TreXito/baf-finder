//! CROSS-SWEEP golden parity — the one every other golden could not cover.
//!
//! Every other fixture replays a SINGLE dump, so all state that lives across
//! sweeps went untested by construction. Worse, the single-dump generators
//! substituted the current dump's byKey for prevByKey (genSniper.ts says so in
//! its own comment) and livefind.ts did the same, so both sides of every
//! Rust-vs-TS comparison carried the identical deviation and it cancelled out.
//!
//! This test replays prod's real two-sweep structure against `sniper/two_dump.json`:
//!   * sweep A primes byKeyA over the carried-over half (alerting off, no flips);
//!   * sweep B prices each NEW candidate against **byKeyA** (index.ts:574/588);
//!   * byKeyB spans the whole dump and is seen ONLY by the dominance/lbin lanes
//!     (index.ts:849/854);
//!   * `seen` and the relist tracker persist across BOTH sweeps (index.ts:25,
//!     sniper.ts:83) — a fresh-per-sweep tracker silently disables the
//!     relist-spam guard, which is exactly what this pins.

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

/// Rebuild a DecodedAuction from a golden candidate/carried entry.
fn decode_entry(e: &Value, idx: &PriceIndex, key_mismatch: &mut usize) -> DecodedAuction {
    let attrs: ItemAttributes = serde_json::from_value(e["attrs"].clone()).unwrap();
    let a = ActiveAuction {
        uuid: e["uuid"].as_str().unwrap().to_string(),
        starting_bid: e["startingBid"].as_f64().unwrap(),
        auctioneer: e["auctioneer"].as_str().map(String::from),
        item_name: e["itemName"].as_str().unwrap_or("").to_string(),
    };
    let key = idx.final_key(&attrs);
    if key != e["key"].as_str().unwrap() {
        *key_mismatch += 1;
    }
    DecodedAuction { a, attrs, key }
}

fn mk_bykey(items: &[DecodedAuction], idx: &PriceIndex) -> HashMap<String, Vec<Bin>> {
    let mut m: HashMap<String, Vec<Bin>> = HashMap::new();
    for d in items {
        let price =
            (d.a.starting_bid - idx.minor_feature_value(&d.attrs)).max(d.a.starting_bid * 0.5);
        m.entry(d.key.clone()).or_default().push(Bin {
            uuid: d.a.uuid.clone(),
            price,
        });
    }
    for list in m.values_mut() {
        list.sort_by(|x, y| x.price.partial_cmp(&y.price).unwrap());
    }
    m
}

#[test]
fn sniper_two_dump_golden_parity() {
    let gdir = concat!(env!("CARGO_MANIFEST_DIR"), "/../goldens");
    let doc: Value = serde_json::from_str(
        &std::fs::read_to_string(format!("{gdir}/sniper/two_dump.json")).unwrap(),
    )
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

    let mut key_mismatch = 0usize;
    let carried: Vec<DecodedAuction> = doc["carried"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| decode_entry(e, &idx, &mut key_mismatch))
        .collect();
    let gcands = doc["candidates"].as_array().unwrap();
    let new_in_b: Vec<DecodedAuction> = gcands
        .iter()
        .map(|c| decode_entry(&c["candidate"], &idx, &mut key_mismatch))
        .collect();
    assert_eq!(
        key_mismatch, 0,
        "{key_mismatch} finalKey mismatches vs golden"
    );
    assert_eq!(
        carried.len() as u64,
        doc["split"]["carriedOver"].as_u64().unwrap(),
        "carried split"
    );
    assert_eq!(
        new_in_b.len() as u64,
        doc["split"]["newInB"].as_u64().unwrap(),
        "new split"
    );

    // ---- sweep A: priming. Builds the map the NEW candidates will price against. ----
    let bykey_a = mk_bykey(&carried, &idx);

    // ---- sweep B ----
    // Persist across both sweeps, exactly as prod's module-level state does.
    let mut seen: HashSet<String> = HashSet::new();
    let mut relist = RelistTracker::default();
    let mut fired: HashMap<String, Flip> = HashMap::new();
    let mut screen_by: HashMap<String, char> = HashMap::new();
    let mut went_to_lbin: HashSet<String> = HashSet::new();
    let mut lbin_candidates: Vec<DecodedAuction> = Vec::new();

    for d in &new_in_b {
        screen_by.insert(d.a.uuid.clone(), screen_auction(d, &idx, &model));
        // prevByKey (byKeyA) — NOT this dump's own map.
        if let Some(f) = eval_clean_snipe(
            d,
            &idx,
            &bykey_a,
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
            &bykey_a,
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

    // byKeyB spans the whole dump; only these two lanes may see it.
    let all: Vec<DecodedAuction> = carried.iter().chain(new_in_b.iter()).cloned().collect();
    let bykey_b = mk_bykey(&all, &idx);
    for f in eval_dominance_flips(
        &lbin_candidates,
        &bykey_b,
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
        &bykey_b,
        &mut seen,
        &mut relist,
        last_updated,
        &idx,
        now_ms,
    ) {
        fired.insert(f.uuid.clone(), f);
    }

    let s = doc["summary"].as_object().unwrap();
    assert_eq!(
        bykey_a.len() as u64,
        s["byKeyAKeys"].as_u64().unwrap(),
        "byKeyA key count"
    );
    assert_eq!(
        bykey_b.len() as u64,
        s["byKeyBKeys"].as_u64().unwrap(),
        "byKeyB key count"
    );
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
        let d = &new_in_b[i];
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
    // See sniper_golden: CONF_MARGIN_PENALTY < 1 changes high-ROI confidence on
    // purpose, so TS parity only holds at the original strength.
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
        "{mismatches}/{} two-dump candidates mismatched",
        gcands.len()
    );
}
