//! Clearance must UNDERCUT a live market without CHASING a lone lowball.
//!
//! 2026-08-07, user: "my bots keep underselling shit ... recombobulated talismans
//! for 200k instead of the 7m theyre worth ... they also sell to each other".
//!
//! The clearance branch read:
//!
//! ```ignore
//! .min(lbin.floor().max(if *LIST_FOLLOW_MARKET { target * MIN_FRACTION } else { 0.0 }))
//! ```
//!
//! i.e. the protective floor was removed EXACTLY when market-following was
//! switched off, while the `min(lbin)` that does the following stayed. Prod runs
//! `LIST_FOLLOW_MARKET=0`, and with `follow` off `follow_market` returns early
//! with INFINITY, so the whole expression collapsed to `list_at = lbin` floored at
//! ZERO. After 6h, any held item with a live competitor was parked on the single
//! cheapest listing however lowballed, with only `cost_floor` underneath.
//!
//! That is how a 28M item lists at 13M and still books as "profit" over the 6M we
//! paid, and how our own finder then re-detects our own listing as a 20x flip and
//! another of our bots buys it.
//!
//! These run with LIST_FOLLOW_MARKET unset (the prod posture).

use finder_core::inventory_pricing::{price_inventory, InventoryPricingInput};

const HOUR_MS: f64 = 3_600_000.0;
const DAY_MS: f64 = 24.0 * HOUR_MS;

fn held(target: f64, lbin: Option<f64>, paid: Option<f64>, age_ms: f64) -> InventoryPricingInput {
    InventoryPricingInput {
        target,
        lbin,
        basis: Some("refs".to_string()),
        paid,
        acquired_at_ms: Some(0.0),
        fallback_first_seen_ms: None,
        volume_per_day: Some(50.0),
        market_median: None,
        variant_priced: false,
        failed_listings: 0.0,
        now_ms: age_ms,
    }
}

#[test]
fn a_lone_lowball_cannot_drag_the_clearance_ask_to_the_floor() {
    // The Juju shape: worth 28M, one cheap live listing at 13M, 8h old so we are
    // in clearance. Must NOT list at 13M.
    let out = price_inventory(&held(
        28_000_000.0,
        Some(13_000_000.0),
        Some(6_000_000.0),
        8.0 * HOUR_MS,
    ));
    assert_eq!(out.mode, "clearance-market");
    // LIST_MIN_FRACTION defaults to 0.9.
    assert_eq!(
        out.list_at, 25_200_000.0,
        "must hold at 90% of our own valuation"
    );
    assert!(out.list_at > 13_000_000.0);
}

#[test]
fn a_real_market_just_above_the_floor_is_still_undercut() {
    // Competitor at 26M against a 28M target: that is a real market, not a
    // lowball, so we undercut it rather than sitting above it.
    let out = price_inventory(&held(
        28_000_000.0,
        Some(26_000_000.0),
        Some(6_000_000.0),
        8.0 * HOUR_MS,
    ));
    assert_eq!(out.list_at, 26_000_000.0);
}

#[test]
fn once_the_decay_ladder_bottoms_out_the_market_wins() {
    // After ~12 days the 25% cap is reached: the target has been demonstrably
    // wrong for a long time and an unsold item realises zero, so follow the
    // market down. This is what `crashed_market_eventually_undercuts...` pins.
    let out = price_inventory(&held(
        40_000_000.0,
        Some(28_000_000.0),
        Some(28_000_000.0),
        30.0 * DAY_MS,
    ));
    assert_eq!(out.mode, "clearance-market");
    assert_eq!(out.list_at, 28_000_000.0);
}

#[test]
fn the_cost_floor_still_applies_on_top() {
    // Paid 26M, worth 28M, lowball at 13M. The lowball floor holds it at 25.2M
    // and the cost floor lifts it further. ⚠️ At 8h we are already on clearance
    // DAY 0, so FLOOR_RELIEF has run once: margin = 0.05 - 0.03 = 0.02, i.e.
    // paid*1.02, not paid*1.05.
    let out = price_inventory(&held(
        28_000_000.0,
        Some(13_000_000.0),
        Some(26_000_000.0),
        8.0 * HOUR_MS,
    ));
    assert_eq!(out.list_at, 26_520_000.0);
    assert_eq!(out.cost_floor, 26_520_000.0);
}

#[test]
fn before_clearance_nothing_changes() {
    // 1h old: normal path, ignores a lowball entirely (use_lbin needs
    // lbin >= target*0.92). ⚠️ LIST_OPEN_FACTOR DEFAULTS to 1.05 even though prod
    // sets it to 1.0, so read it rather than hardcoding either — pinning 1.05
    // made this test fail under the prod environment, which is the one that
    // matters.
    let open: f64 = std::env::var("LIST_OPEN_FACTOR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.05);
    let out = price_inventory(&held(
        28_000_000.0,
        Some(13_000_000.0),
        Some(6_000_000.0),
        HOUR_MS,
    ));
    assert_eq!(out.mode, "normal");
    assert_eq!(out.list_at, (28_000_000.0 * open).floor());
}

/// A model-priced item must not pay a flat 10% tax on its opening ask.
///
/// `use_lbin` is unconditionally true for `basis: model`, so the floor below IS
/// the ask whenever a competitor sits under it. Measured 2026-08-07 over 442
/// model resales that cleared inside 6h, the median realised/estimate was
/// EXACTLY 0.90, and so was every item family in it. Meanwhile model items priced
/// by the clearance ladder instead realised 0.99, so the target was fine and the
/// discount was giveaway.
///
/// Default stays 0.90; prod sets `MODEL_LIST_FLOOR=0.97`.
#[test]
fn the_model_opening_floor_is_configurable() {
    let want: f64 = std::env::var("MODEL_LIST_FLOOR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.90);
    let mut i = held(10_000_000.0, Some(1_000_000.0), Some(2_000_000.0), HOUR_MS);
    i.basis = Some("model".to_string());
    let out = price_inventory(&i);
    // A 1M lowball against a 10M model target: the floor decides the ask.
    assert_eq!(
        out.list_at,
        (10_000_000.0 * want).floor(),
        "model ask should be target*MODEL_LIST_FLOOR ({want})"
    );
}

/// A model item whose competitor is ABOVE the floor still undercuts it.
#[test]
fn a_model_item_still_undercuts_a_healthy_market() {
    let mut i = held(10_000_000.0, Some(9_900_000.0), Some(2_000_000.0), HOUR_MS);
    i.basis = Some("model".to_string());
    let out = price_inventory(&i);
    assert_eq!(out.list_at, (9_900_000.0f64 * 0.995).floor());
}
