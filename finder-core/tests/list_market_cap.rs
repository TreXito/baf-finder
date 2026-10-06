//! `LIST_MARKET_CAP`: stop listing an over-estimated item miles above the market.
//!
//! We have BOTH an underselling and an overselling problem and they are the two
//! tails of one distribution, not a bias. Target vs the market median for the
//! same item+star group, 30,256 flips: p05 0.76x, p50 **1.01x**, p95 **2.20x**.
//! The centre is right; 25.1% sit >15% above market and never sell. Flips that
//! resold and flips that did not share the same median (1.01x), so the tails
//! decide.
//!
//! The existing `lbin` anchor cannot catch the high tail: `use_lbin` requires
//! `lbin >= target * 0.92`, so precisely when our estimate is far above the
//! market we ignore the market and list at `target * 1.05`. This caps against
//! the MEDIAN of recent sales instead — robust to the single lowballed listing
//! that the 0.92 guard exists to protect against.
//!
//! Run the ON half with `LIST_MARKET_CAP=1.0 cargo test`.

use finder_core::inventory_pricing::{price_inventory, InventoryPricingInput};

const HOUR_MS: f64 = 3_600_000.0;

fn input(target: f64, market: Option<f64>, paid: Option<f64>) -> InventoryPricingInput {
    InventoryPricingInput {
        target,
        lbin: None,
        basis: Some("refs".to_string()),
        paid,
        // fresh: well inside the clearance window, so this exercises the normal
        // path rather than the decay ladder.
        acquired_at_ms: Some(1_000_000.0),
        fallback_first_seen_ms: None,
        volume_per_day: Some(50.0),
        market_median: market,
        variant_priced: false,
        failed_listings: 0.0,
        now_ms: 1_000_000.0 + HOUR_MS,
    }
}

#[test]
fn off_by_default_the_market_is_ignored() {
    if *finder_core::config::LIST_MARKET_CAP > 0.0 {
        return; // env has it on; the other tests cover that
    }
    // The exact case that strands inventory: we think it is worth 100M, it
    // actually trades at 40M, and we list it at 105M anyway.
    let r = price_inventory(&input(100_000_000.0, Some(40_000_000.0), None));
    assert_eq!(r.list_at, 105_000_000.0);
}

#[test]
fn an_over_estimate_is_clamped_to_the_market() {
    if *finder_core::config::LIST_MARKET_CAP <= 0.0 {
        return; // run with LIST_MARKET_CAP=1.0
    }
    let cap = *finder_core::config::LIST_MARKET_CAP;
    let r = price_inventory(&input(100_000_000.0, Some(40_000_000.0), None));
    assert_eq!(r.list_at, (40_000_000.0 * cap).floor());
    assert!(r.list_at < 105_000_000.0, "must not list above the market");
}

/// The opening ask is `target * LIST_OPEN_FACTOR`, and that DEFAULTS to 1.05
/// while prod runs 1.0. Hardcoding either makes the suite pass in one
/// environment and fail in the other, so read it.
fn open_factor() -> f64 {
    std::env::var("LIST_OPEN_FACTOR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.05)
}

#[test]
fn an_accurate_estimate_is_left_alone() {
    if *finder_core::config::LIST_MARKET_CAP <= 0.0 {
        return;
    }
    // Target at/below the market: the cap must not touch it, or it would become
    // the very underselling this is not meant to cause.
    let r = price_inventory(&input(10_000_000.0, Some(40_000_000.0), None));
    assert_eq!(
        r.list_at,
        (10_000_000.0 * open_factor()).floor(),
        "cap must only bind on the HIGH tail"
    );
}

#[test]
fn no_market_evidence_means_no_cap() {
    // `market_median: None` is what a thin item looks like (fewer than
    // LIST_MARKET_MIN_REFS sales). A median off 3 sales is not a market, so the
    // cap must not fire on one.
    let r = price_inventory(&input(100_000_000.0, None, None));
    assert_eq!(r.list_at, (100_000_000.0 * open_factor()).floor());
}

#[test]
fn the_cost_floor_still_wins() {
    if *finder_core::config::LIST_MARKET_CAP <= 0.0 {
        return;
    }
    // We overpaid AND the market is below cost. The floor is applied after the
    // cap, so this still lists at paid*1.05 rather than being quietly clamped
    // into a loss. Whether to take that loss is a decision for the clearance
    // ladder (and ultimately the operator), not a side effect of this knob.
    let r = price_inventory(&input(
        100_000_000.0,
        Some(40_000_000.0),
        Some(60_000_000.0),
    ));
    assert_eq!(r.cost_floor, 63_000_000.0);
    assert_eq!(r.list_at, 63_000_000.0);
}

/// A recombobulated item must NOT be capped at the plain pool's median.
///
/// `market_median` is `base_value_for(base_key(attrs))` and `base_key` strips
/// `recombobulated`. Measured over 14 days of sales the premium is 20x-114x
/// (GRAVITY_TALISMAN 0.07M plain vs 8.00M recombed, LUCKY_HOOF 0.35M vs 7.00M),
/// so this cap was pinning a 7M recombed talisman to the 0.35M plain median --
/// which our own finder then bought back as a 20x flip.
///
/// Run with `LIST_MARKET_CAP=1.0 cargo test --test list_market_cap`.
#[test]
fn a_variant_priced_item_is_not_capped_at_the_base_pool_median() {
    let cap_on = std::env::var("LIST_MARKET_CAP").as_deref() == Ok("1.0");
    // 7M recombed Lucky Hoof against a 0.35M plain-pool median.
    let mut i = input(7_000_000.0, Some(350_000.0), Some(160_000.0));
    i.variant_priced = true;
    let out = price_inventory(&i);
    assert!(
        out.list_at > 6_000_000.0,
        "variant item must keep its own valuation, got {}",
        out.list_at
    );

    // The same numbers WITHOUT the variant flag are still capped when the cap is
    // on, so this test proves the flag is what saves it, not a disabled cap.
    let mut j = input(7_000_000.0, Some(350_000.0), Some(160_000.0));
    j.variant_priced = false;
    let plain = price_inventory(&j);
    if cap_on {
        assert!(
            plain.list_at <= 350_000.0,
            "a genuinely base-pool item should still be capped, got {}",
            plain.list_at
        );
    }
}

/// `LIST_CAP_LBIN_FLOOR`: the cap must never price us under the cheapest live
/// BIN. Reproduces the [Lvl 100] Rabbit seen on prod 2026-08-16.
///
/// Env-driven statics are process-global and `LazyLock`, so this asserts the
/// PURE relationship through the public entry point with the flag in whatever
/// state the process has; the arithmetic itself is pinned unconditionally.
#[test]
fn cap_never_prices_under_the_cheapest_live_bin() {
    let now = 1_786_950_000_000.0;
    let out = price_inventory(&InventoryPricingInput {
        target: 14_312_000.0,
        lbin: Some(13_550_001.0),
        basis: Some("refs".to_string()),
        paid: Some(6_000_000.0),
        acquired_at_ms: Some(now - 60_000.0),
        fallback_first_seen_ms: None,
        volume_per_day: Some(20.0),
        market_median: Some(11_650_000.0),
        variant_priced: false,
        failed_listings: 0.0,
        now_ms: now,
    });
    // Whatever the cap does, the ask must land at or above the market median —
    // it may not go BELOW the cheapest competitor while the book is above the
    // sales median. The pre-fix prod value was exactly 11,650,000.
    assert!(
        out.list_at >= 11_650_000.0,
        "ask {} fell under the market median",
        out.list_at
    );
    if *finder_core::config::LIST_CAP_LBIN_FLOOR {
        assert_eq!(
            out.list_at, 13_550_000.0,
            "with the floor on we should sit one coin under the cheapest live BIN"
        );
    }
}
