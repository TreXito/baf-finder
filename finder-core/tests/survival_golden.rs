//! Real-data cross-check for the Kaplan-Meier sell-through estimator.
//!
//! `goldens/survival/tts_survival.json` is a slice of live prod data (8 items,
//! 3,050 observations, 21-day window, captured 2026-08-05) whose expected values
//! were computed by an INDEPENDENT Python implementation against the same
//! vectors. It is the cross-implementation check the unit tests in
//! `survival.rs` cannot give: those pin hand-computed toy cases, this pins the
//! shapes the market actually produces.
//!
//! The spread in the fixture is the point. `HYPERION` and `DIVAN_BOOTS` sit near
//! 44-49% sell-through at 24h; `REDSTONE_ORE` sits at 2.5% off a naive median of
//! 0.22h, and `TOTEM`, `PRISMARINE:1` and `BIRD_HOUSE` are the same shape. Those
//! last four are the items a survivorship-biased median calls liquid.

use finder_core::survival::sell_through;
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    item: String,
    events_h: Vec<f64>,
    censored_h: Vec<f64>,
    sell_through_24h: f64,
    sell_through_6h: f64,
}

#[test]
fn kaplan_meier_matches_an_independent_implementation_on_real_data() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../goldens/survival/tts_survival.json"
    ))
    .expect("survival golden");
    let cases: Vec<Case> = serde_json::from_str(&raw).expect("parse survival golden");
    assert_eq!(cases.len(), 8, "fixture size changed");

    for c in &cases {
        for (horizon, want) in [(24.0, c.sell_through_24h), (6.0, c.sell_through_6h)] {
            let got = sell_through(&c.events_h, &c.censored_h, horizon)
                .unwrap_or_else(|| panic!("{} has observations, must not be None", c.item));
            assert!(
                (got - want).abs() < 1e-9,
                "{} at {}h: rust {got} vs python {want}",
                c.item,
                horizon
            );
        }
    }
}

/// The regression this whole change exists to prevent: an item whose naive
/// median-of-sold-listings is fast while almost nothing actually sells.
#[test]
fn the_biased_median_and_the_honest_curve_disagree_on_the_trap_items() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../goldens/survival/tts_survival.json"
    ))
    .expect("survival golden");
    let cases: Vec<Case> = serde_json::from_str(&raw).expect("parse survival golden");

    let trap = cases
        .iter()
        .find(|c| c.item == "REDSTONE_ORE")
        .expect("REDSTONE_ORE in fixture");
    let mut sold = trap.events_h.clone();
    sold.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let naive_median = sold[sold.len() / 2];
    assert!(
        naive_median < 6.0,
        "naive median should look fast, was {naive_median}"
    );

    let honest = sell_through(&trap.events_h, &trap.censored_h, 24.0).unwrap();
    assert!(
        honest < 0.05,
        "the same item must read illiquid once censored listings count: {honest}"
    );
}
