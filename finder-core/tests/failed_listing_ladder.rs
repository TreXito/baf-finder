//! `LIST_FAIL_DAYS`: a listing that expired unsold advances the clearance ladder.
//!
//! Measured 2026-08-07 over 289 physical items reconstructed from the fleet's own
//! auction history: a BIN runs exactly 6.0h, the gap to the next listing is 2.4h
//! median, and after an expiry we raise the ask 20% of the time and leave it
//! alone another 26%. The ladder itself works (observed -2.3%/day against a
//! designed -2%/day) — it is simply indexed to the wrong clock.
//!
//! These pin that crediting failures cannot reach past any floor that already
//! existed: the ask is still bounded below by `cost_floor` and, while the ladder
//! has room, by `target * LIST_MIN_FRACTION`.

use finder_core::inventory_pricing::{price_inventory, InventoryPricingInput};

const HOUR: f64 = 60.0 * 60.0 * 1000.0;
#[allow(dead_code)]
const DAY: f64 = 24.0 * HOUR;
const NOW: f64 = 1_785_800_000_000.0;

fn fail_days() -> f64 {
    std::env::var("LIST_FAIL_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

/// A held item with no live competitor, so the ask IS the decayed ladder value
/// and the arithmetic is readable. Bought cheap enough that `cost_floor` cannot
/// bind and hide what the ladder did.
fn held(age_h: f64, failures: f64) -> InventoryPricingInput {
    InventoryPricingInput {
        target: 100_000_000.0,
        lbin: None,
        basis: Some("stats".into()),
        paid: Some(10_000_000.0),
        acquired_at_ms: Some(NOW - age_h * HOUR),
        fallback_first_seen_ms: None,
        volume_per_day: Some(5.0),
        market_median: None,
        variant_priced: false,
        failed_listings: failures,
        now_ms: NOW,
    }
}

/// decay = 2% * (days + 1), capped at 25%; ask = target * (1.05 - decay).
fn expected(days: f64) -> f64 {
    let decay = (0.02 * (days + 1.0)).min(0.25);
    (100_000_000.0 * (1.05 - decay)).floor()
}

#[test]
fn failures_advance_the_ladder_by_list_fail_days_each() {
    // 8h old: 2h into clearance, so wall-clock days = 0.
    let none = price_inventory(&held(8.0, 0.0));
    assert_eq!(
        none.list_at,
        expected(0.0),
        "no failures = the old behaviour"
    );

    let three = price_inventory(&held(8.0, 3.0));
    assert_eq!(
        three.list_at,
        expected(3.0 * fail_days()),
        "three failed listings should count as {} extra days",
        3.0 * fail_days()
    );
}

#[test]
fn off_by_default_is_byte_identical() {
    // The whole point of the default: with LIST_FAIL_DAYS unset, an item with a
    // dozen failures prices exactly like one with none.
    if fail_days() != 0.0 {
        return; // prod setting; the assertion above covers the on-path
    }
    for f in [0.0, 1.0, 5.0, 99.0] {
        assert_eq!(
            price_inventory(&held(30.0, f)).list_at,
            price_inventory(&held(30.0, 0.0)).list_at,
            "failures must not move the ask while the knob is off"
        );
    }
}

#[test]
fn an_item_not_yet_in_clearance_is_untouched() {
    // Under 6h the item has not entered clearance, and a failure cannot pull it
    // in early: a BIN takes 6h to expire, so this shape cannot occur in the wild.
    // Pinned anyway, because it is what keeps the fresh-listing path independent.
    let fresh = price_inventory(&held(2.0, 4.0));
    assert_eq!(fresh.list_at, price_inventory(&held(2.0, 0.0)).list_at);
    assert_eq!(fresh.mode, "normal");
}

#[test]
fn the_cost_floor_still_refuses_a_giveaway() {
    // Same item, but bought at 95M against a 100M target. However many listings
    // failed, the ask may not fall below paid * (1 - MAX_LOSS_PCT) = 85.5M.
    let mut i = held(8.0, 50.0);
    i.paid = Some(95_000_000.0);
    let r = price_inventory(&i);
    assert!(
        r.list_at >= (95_000_000.0f64 * 0.90).ceil(),
        "list_at {} fell through the cost floor {}",
        r.list_at,
        r.cost_floor
    );
}

#[test]
fn a_live_lowball_still_cannot_drag_us_below_list_min_fraction() {
    // The failure credit must not open a path around the lowball floor: with the
    // ladder still short of its 25% cap, a 20M competitor on a 100M item leaves
    // the ask at target * LIST_MIN_FRACTION, exactly as without failures.
    let min_fraction: f64 = std::env::var("LIST_MIN_FRACTION")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.9);
    let mut i = held(8.0, 2.0);
    i.lbin = Some(20_000_000.0);
    let r = price_inventory(&i);
    // Only meaningful while the ladder has room; at 25% the floor is 0 by design.
    let days = 0.0 + 2.0 * fail_days();
    if 0.02 * (days + 1.0) < 0.25 {
        assert_eq!(r.list_at, (100_000_000.0 * min_fraction).floor());
        assert_eq!(r.mode, "clearance-market");
    }
}

#[test]
fn the_credit_is_capped_by_list_fail_max() {
    let max: f64 = std::env::var("LIST_FAIL_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(12.0);
    // Beyond the cap, more failures buy nothing.
    let at_cap = price_inventory(&held(8.0, max)).list_at;
    let way_past = price_inventory(&held(8.0, max * 10.0)).list_at;
    assert_eq!(
        at_cap, way_past,
        "failures past LIST_FAIL_MAX must not count"
    );
}

#[test]
fn the_ladder_still_bottoms_out_at_twenty_five_percent() {
    // Whatever the failure count, the decay cap is unchanged: never below 80% of
    // target from the ladder alone (1.05 - 0.25).
    let r = price_inventory(&held(8.0, 1_000.0));
    assert!(
        r.list_at >= (100_000_000.0f64 * 0.80).floor(),
        "list_at {} broke the 25% decay cap",
        r.list_at
    );
}

#[test]
fn wall_clock_and_failures_add_up() {
    // 6h + 3 days old = 3 wall-clock days in clearance, plus two failures.
    let i = held(6.0 + 3.0 * 24.0, 2.0);
    assert_eq!(
        price_inventory(&i).list_at,
        expected(3.0 + 2.0 * fail_days())
    );
}
