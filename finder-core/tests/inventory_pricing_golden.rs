//! Golden-replay parity for inventoryPricing: priceInventory over the input matrix.
//!
//! `cost_floor` AND `list_at` are EXCLUDED from this comparison by design (`mode`
//! and `age_ms` still hold to strict parity). The TS oracle floors a held item at
//! a fixed `paid*1.05` forever — `list_at` is `max(target_or_lbin, cost_floor)`,
//! so that rigid floor could also override a legitimate lower target/lbin,
//! stranding an item at cost indefinitely with no way for its price to ever
//! actually fall to what the market will pay. The Rust side now relaxes the
//! floor once an item has sat in clearance a while, down to a small capped loss
//! (see `MAX_LOSS_PCT`/`FLOOR_RELIEF_PER_DAY` in inventory_pricing.rs), so a
//! genuinely-stuck item can clear instead of sitting at cost forever. That is an
//! intentional, permanent divergence from the TS oracle, not a porting bug — see
//! the dedicated tests below for real coverage of the new floor behavior.

use finder_core::inventory_pricing::{price_inventory, InventoryPricingInput};
use serde_json::Value;

fn num_close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

#[test]
fn inventory_pricing_golden_parity() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../goldens/inventoryPricing/matrix.json"
    );
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let entries = doc["entries"].as_array().unwrap();
    assert!(entries.len() >= 3000);

    let mut mismatches = 0usize;
    let mut examples: Vec<String> = Vec::new();
    for e in entries {
        let input: InventoryPricingInput = serde_json::from_value(e["input"].clone()).unwrap();
        let out = &e["output"];
        let got = price_inventory(&input);
        let ok = num_close(got.age_ms, out["ageMs"].as_f64().unwrap())
            && got.mode == out["mode"].as_str().unwrap();
        if !ok {
            mismatches += 1;
            if examples.len() < 8 {
                examples.push(format!("in={} got={:?} want={}", e["input"], got, out));
            }
        }
    }
    for ex in &examples {
        eprintln!("--- MISMATCH {ex}");
    }
    assert_eq!(
        mismatches,
        0,
        "{mismatches}/{} inventoryPricing entries mismatched",
        entries.len()
    );
}

/// The loss-tolerant floor itself: fresh/short-held items keep the old
/// paid*1.05 guarantee; a genuinely stuck item's floor relaxes toward (and
/// slightly below) break-even, capped at MAX_LOSS_PCT.
#[test]
fn cost_floor_relaxes_for_stuck_items_but_stays_capped() {
    let base = InventoryPricingInput {
        target: 40_000_000.0,
        lbin: None,
        basis: None,
        paid: Some(16_000_000.0),
        acquired_at_ms: Some(0.0),
        fallback_first_seen_ms: None,
        volume_per_day: None,
        market_median: None,
        variant_priced: false,
        failed_listings: 0.0,
        now_ms: 0.0,
    };
    // Fresh (well under the 6h clearance threshold): unchanged +5% floor.
    let fresh = price_inventory(&InventoryPricingInput {
        now_ms: 3.0 * 3_600_000.0,
        ..base.clone()
    });
    assert_eq!(fresh.cost_floor, (16_000_000.0f64 * 1.05).ceil());

    // A week stuck in clearance: floor has relaxed below cost, but not past the cap.
    let week_stuck = price_inventory(&InventoryPricingInput {
        now_ms: 7.0 * 24.0 * 3_600_000.0,
        ..base.clone()
    });
    assert!(
        week_stuck.cost_floor < 16_000_000.0,
        "a week stuck should allow listing below cost"
    );
    assert!(
        week_stuck.cost_floor >= (16_000_000.0f64 * 0.90).ceil(),
        "loss must stay capped at 10%"
    );

    // A month stuck: still capped at the same floor, never keeps dropping.
    let month_stuck = price_inventory(&InventoryPricingInput {
        now_ms: 30.0 * 24.0 * 3_600_000.0,
        ..base
    });
    assert_eq!(month_stuck.cost_floor, (16_000_000.0f64 * 0.90).ceil());
}

/// Before this fix, a crashed market (live lbin at/below cost) still floored
/// the listing at paid*1.05 forever — the bot would ask MORE than the market
/// while calling it "clearance-market" undercut pricing. Given enough stuck
/// time the floor must relax enough to actually match a crashed lbin.
#[test]
fn crashed_market_eventually_undercuts_to_the_real_lbin_not_a_rigid_cost_floor() {
    let input = InventoryPricingInput {
        target: 40_000_000.0,
        lbin: Some(28_000_000.0), // market crashed to exactly what we paid
        basis: None,
        paid: Some(28_000_000.0),
        acquired_at_ms: Some(0.0),
        fallback_first_seen_ms: None,
        volume_per_day: None,
        market_median: None,
        variant_priced: false,
        failed_listings: 0.0,
        now_ms: 30.0 * 24.0 * 3_600_000.0, // a month stuck
    };
    let out = price_inventory(&input);
    assert_eq!(out.mode, "clearance-market");
    assert_eq!(
        out.list_at, 28_000_000.0,
        "should match the real market, not sit above it at a stale cost floor"
    );
}
