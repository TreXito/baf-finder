//! Port of `baf-flip-finder/src/inventoryPricing.ts` — stale-market inventory
//! re-list pricing. Pure function; pinned by `goldens/inventoryPricing/matrix.json`.

use crate::config::{
    FLOOR_GIVE_UP_DAYS, LIST_CAP_LBIN_FLOOR, LIST_FAIL_DAYS, LIST_FAIL_MAX, LIST_FOLLOW_MARKET,
    LIST_LOWBALL_GUARD, LIST_MARKET_CAP, LIST_MIN_FRACTION, LIST_OPEN_FACTOR, MODEL_LIST_FLOOR,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryPricingInput {
    pub target: f64,
    #[serde(default)]
    pub lbin: Option<f64>,
    #[serde(default)]
    pub basis: Option<String>,
    #[serde(default)]
    pub paid: Option<f64>,
    #[serde(default)]
    pub acquired_at_ms: Option<f64>,
    #[serde(default)]
    pub fallback_first_seen_ms: Option<f64>,
    #[serde(default)]
    pub volume_per_day: Option<f64>,
    /// What the item ACTUALLY trades at (median of recent sales for its
    /// item+star group, not the narrow final key). The anchor for
    /// `LIST_MARKET_CAP`. `None` = no market evidence, cap does not apply.
    #[serde(default)]
    pub market_median: Option<f64>,
    /// True when the item was priced off a key that is NARROWER than its base key
    /// (`final_key != base_key`), i.e. its value comes from a significant feature
    /// such as `recombobulated`. `market_median` is a BASE-pool number and is
    /// meaningless for these, so `LIST_MARKET_CAP` must not bind on them.
    #[serde(default)]
    pub variant_priced: bool,
    /// How many of our own listings of THIS physical item have expired without
    /// selling. Each one is direct evidence the ask was too high, and is worth
    /// `LIST_FAIL_DAYS` extra days on the clearance ladder. See [`LIST_FAIL_DAYS`]
    /// for why the wall-clock ladder alone cannot keep up.
    #[serde(default)]
    pub failed_listings: f64,
    pub now_ms: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InventoryPricingResult {
    pub list_at: f64,
    pub cost_floor: f64,
    pub age_ms: f64,
    pub mode: String,
}

const HOUR_MS: f64 = 60.0 * 60.0 * 1000.0;
const CLEARANCE_START_MS: f64 = 6.0 * HOUR_MS;
const NICHE_CLEARANCE_START_MS: f64 = 12.0 * HOUR_MS;
// A held item normally never lists below paid*1.05 (a small guaranteed profit).
// But that floor made a bad initial estimate permanent: if the market won't
// actually pay what we thought it would, the item just sits forever at cost,
// immune to the clearance decay above it. Once something has been stuck in
// clearance a while, let the floor itself relax — down to a small, capped loss
// — so it can actually clear instead of sitting indefinitely. Reaches the full
// loss cap faster than the target decay above (days, not the ~12 needed for
// that to hit its own 25% cap): the floor is a last resort, not the main lever.
const MAX_LOSS_PCT: f64 = 0.10;
const FLOOR_RELIEF_PER_DAY: f64 = 0.03;

/// Cap an opening ask at just under the visible market, WITHOUT chasing a single
/// lowballed listing. Pure and config-free so it is directly unit-testable.
///
/// ⚠️ The first version of this followed `lbin` unguarded and cost real money
/// within two hours: a 5-star Spiritual Juju Shortbow bought at 13.00M against a
/// 28.25M estimate was relisted at 13.65M and taken 64 seconds later, while the
/// item's own 3-day median was 35.00M. One cheap live listing dragged our ask to
/// cost. That is exactly what the original `lbin >= target * 0.92` gate was
/// defending against, and the comment on `LIST_MARKET_CAP` in this very file
/// says so — "robust to a single lowball, unlike lbin". I removed the guard and
/// reproduced the bug it existed to prevent.
///
/// So follow the market DOWN, but never below `guard` times the MEDIAN of recent
/// sales. A median cannot be moved by one cheap listing, while a genuinely
/// fallen market drags it down too, so this still tracks a real decline.
fn follow_market(
    list_at: f64,
    lbin: Option<f64>,
    market_median: Option<f64>,
    target: f64,
    follow: bool,
    guard: f64,
) -> f64 {
    let Some(l) = lbin.filter(|l| *l > 0.0) else {
        return list_at;
    };
    if !follow {
        return list_at;
    }
    let mut anchor = lowball_guarded((l * 0.995).floor(), market_median, follow, guard);
    // ⚠️ NEVER drag below what our own keyed valuation supports. The model path
    // already had exactly this protection (`max(target * 0.9)`) and the first
    // version of this function bypassed it, which cost a second real flip: a
    // Jaded Sorrow Leggings valued at 14.41M was pulled from a 12.97M ask down
    // to the BASE pool's 6.97M lbin, then clamped back to 10.50M by the cost
    // floor and taken instantly.
    //
    // `lbin` and `market_median` are both computed over a pool that need not
    // match this item's variant. When the item is genuinely worth more than the
    // pool, following the pool is a giveaway. If instead our TARGET is the thing
    // that is wrong, the fix belongs in pricing (`SHORT_TERM_WIDEN_HOURS`,
    // `THIN_KEY_BASE_TREND`), not in a listing-time override that cannot tell
    // the two cases apart.
    if target > 0.0 {
        anchor = anchor.max((target * *LIST_MIN_FRACTION).floor());
    }
    list_at.min(anchor)
}

/// Raise a market-derived ask back up to `guard` times the sales median, so one
/// cheap live listing cannot drag us to cost. No median evidence = no guard.
fn lowball_guarded(price: f64, market_median: Option<f64>, on: bool, guard: f64) -> f64 {
    match market_median {
        Some(m) if on && m > 0.0 && guard > 0.0 => price.max((m * guard).floor()),
        _ => price,
    }
}

/// How far down the clearance ladder an item has walked, in days.
///
/// Wall-clock age past `clearance_start_ms`, plus [`LIST_FAIL_DAYS`] for each
/// listing of this item that expired unsold. `-1.0` means "not in clearance yet",
/// which is what both the decay ladder and `FLOOR_RELIEF_PER_DAY` test for.
///
/// A failure cannot pull an item into clearance early: a BIN runs 6.0h and
/// `CLEARANCE_START_MS` is 6h, so any item with a failed listing is already past
/// the start by construction. The `age_ms < start` branch is therefore left
/// untouched, which is also what keeps `LIST_FAIL_DAYS=0` byte-identical.
fn clearance_days(age_ms: f64, clearance_start_ms: f64, failed_listings: f64) -> f64 {
    if age_ms < clearance_start_ms {
        return -1.0;
    }
    let wall = ((age_ms - clearance_start_ms) / (24.0 * HOUR_MS)).floor();
    let evidence = *LIST_FAIL_DAYS * failed_listings.max(0.0).min(*LIST_FAIL_MAX);
    wall + evidence
}

pub fn price_inventory(input: &InventoryPricingInput) -> InventoryPricingResult {
    let target = input.target;
    let lbin = input.lbin;
    let started_at = input
        .acquired_at_ms
        .or(input.fallback_first_seen_ms)
        .unwrap_or(input.now_ms);
    let age_ms = (input.now_ms - started_at).max(0.0);
    let clearance_start_ms = match input.volume_per_day {
        Some(v) if v <= 1.0 => NICHE_CLEARANCE_START_MS,
        _ => CLEARANCE_START_MS,
    };
    // ⚠️ This used to be computed here AND again inside the clearance branch
    // below, from the same three values. One definition now, because the two
    // copies are what let `FLOOR_RELIEF_PER_DAY` and the decay ladder drift
    // apart the moment either gains a term — as `LIST_FAIL_DAYS` just did.
    let days_in_clearance = clearance_days(age_ms, clearance_start_ms, input.failed_listings);
    // After long enough, stop defending the purchase price at all.
    //
    // `MAX_LOSS_PCT` caps the floor at `paid * 0.90`, but 266 held items worth
    // 9.96B trade BELOW that (median market/paid 0.65), so the floor sits above
    // any price that would clear them and they are structurally unlistable for
    // ever. 198 of them, 6.97B, are already 7+ days old. An item that never
    // sells realises ZERO and holds a slot permanently, and the fleet is
    // uptime/slot-bound, so at some age the market price beats the hold.
    //
    // ⚠️ This CRYSTALLISES a real loss on the items it touches. It is a capital
    // recycling decision, not a pricing fix. 0 = off.
    let given_up = *FLOOR_GIVE_UP_DAYS > 0.0 && days_in_clearance >= *FLOOR_GIVE_UP_DAYS;
    let cost_floor = match input.paid {
        Some(p) if p != 0.0 && !given_up => {
            let margin = if days_in_clearance >= 0.0 {
                (0.05 - FLOOR_RELIEF_PER_DAY * (days_in_clearance + 1.0)).max(-MAX_LOSS_PCT)
            } else {
                0.05
            };
            (p * (1.0 + margin)).ceil()
        }
        _ => 0.0,
    };

    let model = input.basis.as_deref() == Some("model");
    let fresh_target = (target * if model { 0.97 } else { *LIST_OPEN_FACTOR }).floor();
    let has_lbin = lbin.map(|l| l != 0.0).unwrap_or(false);
    let use_lbin = has_lbin && (model || lbin.unwrap() >= target * 0.92);
    let mut list_at = fresh_target;
    if use_lbin {
        let l = lbin.unwrap();
        list_at = if model {
            // ⚠️ `use_lbin` is unconditionally true for a model-priced item, so
            // this floor IS the model ask whenever a competitor sits below it.
            // At the old hardcoded 0.9 that made the lane pay a flat 10% tax:
            // measured 2026-08-07 over 442 model resales that sold inside 6h
            // (clearance never ran), the median realised/estimate was **exactly
            // 0.90**, and so was every item family in it -- SORROW_BOOTS,
            // MITHRIL_DRILL_2, ROD_OF_THE_SEA, GEMSTONE_GAUNTLET, all 0.90. A
            // constant, not a distribution: we asked 0.9x and got 0.9x.
            //
            // The model's TARGET is not the problem. Model items that reached
            // clearance are priced by the ladder instead of this floor and
            // realised **0.99** in the 6-24h band, so the estimate is close to
            // right and the discount was pure giveaway on ~28B of flow.
            //
            // Default stays 0.90 (byte-identical); prod raises it.
            (l * 0.995)
                .floor()
                .max((target * *MODEL_LIST_FLOOR).floor())
        } else {
            (l * 0.995).floor().min((target * 1.25).floor())
        };
    }
    // Never open ABOVE the visible market.
    //
    // `use_lbin` only consults the market when the market is ALREADY within 8%
    // of our target (`lbin >= target * 0.92`). When the market has fallen well
    // below the target — exactly when following it matters — it is ignored and
    // we open at `target * 1.05`, i.e. 5% above an estimate the market has
    // already left behind. Nothing corrects that until the 6h clearance ladder,
    // so the item sits for 6 hours burning listing fees and a slot.
    //
    // Measured on the held Hegemony Artifacts: est 675M against a 495M market,
    // opening ask ~709M. And the demand curve says at-or-above par is the ONLY
    // cliff that matters (110% of median: 30 min to sell, 50% sell-through;
    // 95%: 15 min, 65%) — so opening above market is the single worst choice
    // available.
    //
    // This only ever lowers the ask, and the cost floor below still refuses to
    // sell into a loss, so it cannot dump an item.
    list_at = follow_market(
        list_at,
        lbin,
        input.market_median,
        target,
        *LIST_FOLLOW_MARKET,
        *LIST_LOWBALL_GUARD,
    );
    let mut mode = "normal".to_string();

    if age_ms >= clearance_start_ms {
        // Both clearance branches walk the ask down the SAME ladder: 105% of
        // target decaying 2%/day to a 25% floor. The only difference is that when
        // a live competitor exists we undercut it, rather than pricing blind.
        let decay = (0.02 * (days_in_clearance + 1.0)).min(0.25);
        let decayed = (target * (1.05 - decay)).floor();
        // How far a live competitor may drag the ask. `LIST_MIN_FRACTION` of our
        // own valuation while the decay ladder still has room, and nothing once it
        // has bottomed out: at that point the target has been demonstrably wrong
        // for ~12 days, so the market is the better evidence and an item that
        // never sells realises zero.
        let ladder_exhausted = decay >= 0.25;
        let lowball_floor = if ladder_exhausted || target <= 0.0 {
            0.0
        } else {
            (target * *LIST_MIN_FRACTION).floor()
        };
        if lbin.map(|l| l > 0.0).unwrap_or(false) {
            // ⚠️ This used to read `.max(if *LIST_FOLLOW_MARKET { target*MIN_FRACTION }
            // else { 0.0 })`, i.e. the floor was removed EXACTLY when market-following
            // was switched off — while the `min(lbin)` that does the following stayed.
            // With `LIST_FOLLOW_MARKET=0` (prod) `follow_market` returns early with
            // INFINITY, so the whole expression collapsed to `list_at = lbin`, floored
            // at ZERO: after 6h any item with a live competitor was parked on the single
            // cheapest listing, however lowballed, with only `cost_floor` beneath it.
            // That sells a 28M item for 13M and still books it as "profit" over what we
            // paid. See finder-lbin-is-one-listing-not-a-market for the Juju that cost.
            //
            // Undercut the market, never chase it below our own decayed valuation.
            list_at = follow_market(
                f64::INFINITY,
                lbin,
                input.market_median,
                target,
                *LIST_FOLLOW_MARKET,
                *LIST_LOWBALL_GUARD,
            )
            .min(lbin.unwrap().floor())
            .max(lowball_floor);
            mode = "clearance-market".to_string();
        } else {
            list_at = decayed;
            mode = "clearance-no-lbin".to_string();
        }
    }

    // ---- market anchor (LIST_MARKET_CAP; 0 = off, byte-identical) ----
    //
    // Our target is well centred (median 1.01x the market median across 30,256
    // flips) but the UPPER TAIL is brutal: p95 = 2.20x, and 25.1% of targets sit
    // >15% above market. Those are the items that never sell.
    //
    // The existing lbin anchor cannot catch them: `use_lbin` requires
    // `lbin >= target * 0.92`, so exactly when our estimate is far above the
    // market we IGNORE the market and list at `target * 1.05`. That guard exists
    // to avoid chasing one lowballed listing down, which is why this caps against
    // the MEDIAN of recent sales instead — robust to a single lowball, unlike lbin.
    //
    // Sub-median pricing costs almost nothing in speed: measured across 3 days of
    // sales, listings at 95% of the item's median sold in 15 min vs 13 min at 55%,
    // while 110% took 30 min. So clamping an over-estimate to ~market is close to
    // free, and it is the difference between selling and stranding.
    // ⚠️ `market_median` is `base_value_for(base_key(attrs))`, and `base_key`
    // STRIPS every significant feature, `recombobulated` above all. Measured over
    // 14 days of sales the recomb premium is 20x to 114x on talismans
    // (GRAVITY 0.07M plain vs 8.00M recombed, LUCKY_HOOF 0.35M vs 7.00M), so this
    // cap was pinning a 7M recombed talisman to the 0.35M plain median. Our own
    // finder then saw that listing as a 20x flip and another of our bots bought
    // it, paying the AH fee to move stock between our own accounts.
    //
    // The cap's PURPOSE is sound (do not ask above a market that has fallen); the
    // base-key anchor is what was wrong. So it now only applies to items that
    // genuinely live in the base pool, i.e. ones with no significant feature.
    // `variant_priced` is set by the caller when `final_key != base_key`.
    if *LIST_MARKET_CAP > 0.0 && !input.variant_priced {
        if let Some(m) = input.market_median {
            if m > 0.0 {
                list_at = list_at.min((m * *LIST_MARKET_CAP).floor());
                // ⚠️ ...but never UNDER the cheapest thing already on the AH.
                //
                // The cap's whole justification is "do not ask above a market
                // that has fallen". When `lbin > market_median` the market has
                // not fallen — the live book is ABOVE the sales median, and the
                // cap then drags us underneath every competing seller for free.
                //
                // Observed 2026-08-16 on prod, `LIST-UNDER`:
                //   [Lvl 100] Rabbit  listAt 11,650,000 (= market_median exactly)
                //                     lbin 13,550,001   target 14,312,000
                // We undercut the cheapest live BIN by 14% on an item our own
                // pool valued at 14.3M off 74 samples at conf 0.96. Selling at
                // `lbin - 1` is just as certain and 1.9M richer, and the same
                // shape is 20.2% of everything we sell inside five minutes.
                //
                // `lbin - 1` and not `lbin`: ties lose to the older listing.
                if *LIST_CAP_LBIN_FLOOR {
                    if let Some(l) = input.lbin.filter(|l| *l > 0.0) {
                        if l > m {
                            list_at = list_at.max((l - 1.0).floor());
                        }
                    }
                }
            }
        }
    }

    InventoryPricingResult {
        list_at: list_at.max(cost_floor),
        cost_floor,
        age_ms,
        mode,
    }
}

#[cfg(test)]
mod follow_market_tests {
    use super::follow_market;

    /// The real held Hegemony Artifact: bought 490M, target still 675M, market
    /// 495M. `use_lbin` refuses to look at the market (495 < 675*0.92 = 621),
    /// so the opening ask is target*1.05.
    const TARGET: f64 = 675_100_000.0;
    const MARKET: f64 = 495_000_000.0;
    const GUARD: f64 = 0.85;

    fn opening_ask() -> f64 {
        (TARGET * 1.05).floor()
    }

    #[test]
    fn off_by_default_keeps_the_old_opening_ask() {
        assert_eq!(
            follow_market(opening_ask(), Some(MARKET), Some(MARKET), 0.0, false, GUARD),
            opening_ask()
        );
    }

    #[test]
    fn follows_a_genuinely_fallen_market_down() {
        // Hegemony: the whole market moved, so lbin AND the median are both low
        // and the guard (495*0.85 = 420M) does not bind.
        let new = follow_market(opening_ask(), Some(MARKET), Some(MARKET), 0.0, true, GUARD);
        assert_eq!(new, (MARKET * 0.995).floor());
        assert!(new < MARKET);
    }

    #[test]
    fn refuses_to_chase_one_lowball_to_cost() {
        // THE regression this guard exists for. 5-star Spiritual Juju Shortbow:
        // bought 13.00M, est 28.25M, 3-day median 35.00M, and ONE live listing
        // at 13.7M. Unguarded we relisted at 13.65M and lost the whole flip.
        let target: f64 = 28_250_000.0;
        let open = (target * 0.95).floor();
        let lowball = 13_700_000.0;
        let median = 35_000_000.0;
        let unguarded = follow_market(open, Some(lowball), Some(median), 0.0, true, 0.0);
        assert!(
            unguarded < 14e6,
            "unguarded should chase the lowball: {unguarded}"
        );
        let guarded = follow_market(open, Some(lowball), Some(median), 0.0, true, GUARD);
        // 35M * 0.85 = 29.75M floor, so the 26.8M opening ask stands untouched.
        assert_eq!(guarded, open);
        assert!(guarded > 26e6, "guarded was {guarded}");
    }

    #[test]
    fn never_drags_below_our_own_valuation() {
        // Jaded Sorrow Leggings: target 14.41M, model path ask 12.97M, but the
        // BASE pool lbin is 6.97M because the pool mixes recombobulated items
        // (median 17.80M) with plain ones (7.00M). Following the pool handed the
        // flip away twice; the 0.9*target floor keeps the ask where our own
        // keyed valuation puts it.
        let target: f64 = 14_410_000.0;
        let model_ask = (target * 0.9).floor();
        let pool_lbin = 7_000_000.0;
        let pool_median = 6_130_000.0;
        let got = follow_market(
            model_ask,
            Some(pool_lbin),
            Some(pool_median),
            target,
            true,
            GUARD,
        );
        assert_eq!(got, model_ask, "dragged to the base pool: {got}");
        assert!(got > 12.9e6);
    }

    #[test]
    fn never_raises_an_ask_that_is_already_below_market() {
        let cheap = 100e6;
        assert_eq!(
            follow_market(cheap, Some(MARKET), Some(MARKET), 0.0, true, GUARD),
            cheap
        );
    }

    #[test]
    fn no_market_evidence_changes_nothing() {
        assert_eq!(
            follow_market(opening_ask(), None, None, 0.0, true, GUARD),
            opening_ask()
        );
        assert_eq!(
            follow_market(opening_ask(), Some(0.0), None, 0.0, true, GUARD),
            opening_ask()
        );
        // lbin but no median: still follows, because there is nothing to guard with.
        let n = follow_market(opening_ask(), Some(MARKET), None, 0.0, true, GUARD);
        assert_eq!(n, (MARKET * 0.995).floor());
    }
}

#[cfg(test)]
mod floor_give_up_tests {
    use super::{price_inventory, InventoryPricingInput};

    const DAY: f64 = 24.0 * 60.0 * 60.0 * 1000.0;
    const NOW: f64 = 1_785_800_000_000.0;

    /// A real shape from the parked stock: a Perfect Chisel bought at 101M that
    /// the market now pays ~64.7M for, 12 days old. `MAX_LOSS_PCT` pins the
    /// floor at 90.9M, which is 40% above anything that would clear it.
    fn chisel(age_days: f64) -> InventoryPricingInput {
        InventoryPricingInput {
            target: 101_000_000.0,
            lbin: Some(64_700_000.0),
            basis: Some("stats".into()),
            paid: Some(101_000_000.0),
            acquired_at_ms: Some(NOW - age_days * DAY),
            fallback_first_seen_ms: None,
            volume_per_day: Some(5.0),
            market_median: None,
            variant_priced: false,
            failed_listings: 0.0,
            now_ms: NOW,
        }
    }

    #[test]
    fn off_by_default_the_floor_still_strands_it() {
        let r = price_inventory(&chisel(12.0));
        // Floor bottoms out at paid*0.90 and never goes lower, so the ask stays
        // far above the 64.7M market and the item cannot clear.
        assert_eq!(r.cost_floor, (101_000_000.0f64 * 0.90).ceil());
        assert!(r.list_at > 90_000_000.0, "list_at {}", r.list_at);
        assert!(r.list_at > 64_700_000.0);
    }

    #[test]
    fn a_fresh_item_is_never_given_up_on() {
        // Whatever the setting, something bought an hour ago keeps its floor.
        let r = price_inventory(&chisel(1.0 / 24.0));
        assert!(r.cost_floor > 0.0);
        assert!(r.list_at >= r.cost_floor);
    }
}
