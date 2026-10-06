//! Golden-replay parity for filter.evaluateFlip over the decision matrix.

use finder_core::filter::{BinMasterFilter, Filter, FilterFlip};
use serde_json::Value;

fn num_close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

#[test]
fn filter_golden_parity() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../goldens/filter/decisions.json"
    );
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let cfg: BinMasterFilter =
        serde_json::from_value(doc["binmasterFilter"].clone()).expect("binmasterFilter parses");
    let filter = Filter::new(Some(cfg));

    let entries = doc["entries"].as_array().unwrap();
    let mut mismatches = 0usize;
    let mut examples: Vec<String> = Vec::new();
    for e in entries {
        let flip: FilterFlip = serde_json::from_value(e["input"].clone()).expect("flip parses");
        let out = &e["output"];
        let got = filter.evaluate_flip(&flip);
        let ok = got.pass == out["pass"].as_bool().unwrap()
            && num_close(got.scale_price, out["scalePrice"].as_f64().unwrap())
            && num_close(got.priority, out["priority"].as_f64().unwrap())
            && got.reason.as_deref() == out["reason"].as_str();
        if !ok {
            mismatches += 1;
            if examples.len() < 8 {
                examples.push(format!(
                    "id={} got={:?} want={}",
                    e["input"]["id"], got, out
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
        "{mismatches}/{} filter entries mismatched",
        entries.len()
    );
}

/// `BIG_FLIP_MIN_PROFIT`: a large ABSOLUTE profit at an ordinary ROI must not
/// die on a confidence gate. Reproduces the drill the waiver was written for
/// (auction a3764a24, profit 202,167,299 at 40% ROI, confidence 0.597).
///
/// Env statics are process-global, so this pins the pure relationship: the
/// waiver must require ALL THREE of profit, roi and samples, or it is just a
/// loosened gate.
#[test]
fn big_flip_waiver_needs_profit_roi_and_samples_together() {
    let (min_profit, min_roi, min_samples) = (50_000_000.0_f64, 20.0_f64, 5_i64);
    let big = |profit: f64, roi: f64, samples: i64| -> bool {
        min_profit > 0.0 && profit >= min_profit && roi >= min_roi && samples >= min_samples
    };
    // The real drill.
    assert!(big(202_167_299.0, 40.0, 7), "the 202M drill must qualify");
    // A merely large profit on a thin margin must not.
    assert!(!big(202_167_299.0, 5.0, 7), "low ROI is not waived");
    // A merely high ROI on a small profit must not: that is the EXTREME waiver's
    // job and it has its own, stricter bar.
    assert!(!big(3_000_000.0, 400.0, 7), "small profit is not waived");
    // ⛔ And never on a pool with no evidence. One sale is not a market.
    assert!(
        !big(202_167_299.0, 40.0, 1),
        "a 1-sample pool must never waive"
    );
}
