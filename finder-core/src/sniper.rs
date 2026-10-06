//! Port of `baf-flip-finder/src/sniper.ts` — the flip decision engine (snipe /
//! median / model / lbin / dominance lanes). Pinned by `goldens/sniper/dump.json`.
//! Single-box sequencing (no worker); shared `seen`/relist state accumulates in
//! candidate order (order-dependent, per the fixture).

use crate::config::*;
use crate::craft_cost::craft_ceiling;
use crate::modifier_model::ModelEstimate;
use crate::modifier_model::ModifierModel;
use crate::nbt::ItemAttributes;
use crate::price_index::{base_key, KeyStats, PriceIndex};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

/// Rejection funnel for `eval_median_flip` — instrumentation ONLY, the counters
/// never influence a decision. Answers "where are candidates dying?" (esp. the
/// volume floor, the volume→TTS lever). Per-thread: the serve loop drains it at
/// sweep end (all eval for a sweep runs on that one thread); the COMPARE/golden
/// paths simply never read it, so behaviour and parity are unchanged.
#[derive(Default, Clone, Copy)]
pub struct RejectStats {
    pub below_margin: u32,
    pub not_priceable: u32,
    pub below_profit: u32,
    pub below_volume: u32,
    pub below_confidence: u32,
    pub undercuts: u32,
    pub relist_blocked: u32,
    pub falling: u32,
    pub emitted: u32,
}

#[derive(Clone, Copy)]
pub enum Reject {
    Margin,
    NotPriceable,
    Profit,
    Volume,
    Confidence,
    Undercuts,
    Relist,
    Falling,
    Emitted,
}

thread_local! {
    static REJECT: Cell<RejectStats> = const { Cell::new(RejectStats {
        below_margin: 0, not_priceable: 0, below_profit: 0, below_volume: 0,
        below_confidence: 0, undercuts: 0, relist_blocked: 0, falling: 0, emitted: 0,
    }) };
}

fn reject_bump(r: Reject) {
    REJECT.with(|c| {
        let mut s = c.get();
        match r {
            Reject::Margin => s.below_margin += 1,
            Reject::NotPriceable => s.not_priceable += 1,
            Reject::Profit => s.below_profit += 1,
            Reject::Volume => s.below_volume += 1,
            Reject::Confidence => s.below_confidence += 1,
            Reject::Undercuts => s.undercuts += 1,
            Reject::Relist => s.relist_blocked += 1,
            Reject::Falling => s.falling += 1,
            Reject::Emitted => s.emitted += 1,
        }
        c.set(s);
    });
    if !matches!(r, Reject::Emitted) {
        log_high_miss(r);
    }
}

/// The candidate `eval_median_flip` is currently judging, so a reject can name
/// the auction it killed. The per-sweep FUNNEL totals cannot answer "why did
/// THIS 12M flip die"; this can. Logging only — never read by a decision.
#[derive(Default)]
struct CandCtx {
    uuid: String,
    item: String,
    key: String,
    price: f64,
    /// The resale value the CURRENT gate is judging against. Overwritten (not
    /// maxed) as the lanes refine it, so a reject reports the number that gate
    /// actually used. 0 = we never got far enough to value it.
    est: f64,
    /// Where `est` came from. `cheap`/`high` are coarse PRE-FILTERS (1.3x a cheap
    /// median, 1.5x the base key's all-time high) — they are upper bounds, NOT
    /// valuations, so a miss reported against them overstates the loss. Only
    /// `model`/`stats` are real valuations worth acting on.
    basis: &'static str,
}

thread_local! {
    static CAND: RefCell<CandCtx> = RefCell::new(CandCtx::default());
}

#[inline]
fn cand_begin(uuid: &str, item: &str, key: &str, price: f64) {
    if *HIGH_MISS_MIN_PROFIT <= 0.0 {
        return;
    }
    CAND.with(|c| {
        let mut c = c.borrow_mut();
        c.uuid.clear();
        c.uuid.push_str(uuid);
        c.item.clear();
        c.item.push_str(item);
        c.key.clear();
        c.key.push_str(key);
        c.price = price;
        c.est = 0.0;
        c.basis = "none";
    });
}

/// Record the resale value the next gate will judge against. Overwrites, so the
/// reported miss is always against the estimate that gate actually used rather
/// than the most generous one any lane ever produced.
#[inline]
fn cand_est(v: f64, basis: &'static str) {
    if *HIGH_MISS_MIN_PROFIT <= 0.0 || !v.is_finite() {
        return;
    }
    CAND.with(|c| {
        let mut c = c.borrow_mut();
        c.est = v;
        c.basis = basis;
    });
}

fn reject_name(r: Reject) -> &'static str {
    match r {
        Reject::Margin => "margin",
        Reject::NotPriceable => "notpriceable",
        Reject::Profit => "profit",
        Reject::Volume => "volume",
        Reject::Confidence => "confidence",
        Reject::Undercuts => "undercuts",
        Reject::Relist => "relist",
        Reject::Falling => "falling",
        Reject::Emitted => "emitted",
    }
}

/// Last-chance estimate for an item the pricing path could not key.
///
/// Only the VARIANT is stripped, because that is what forks `base_key` and robs
/// the item of both its pool and its fallback. Uses the base MEDIAN rather than
/// the p95 high: if the variant is genuinely valuable, stripping it
/// under-estimates, and under-estimating only costs us flips, never money.
///
/// Requires RESCUE_MIN_MULT, so it fires only when the listing is a small
/// fraction of what the plain item reliably sells for -- an obvious mispricing
/// at any size, not a marginal call.
fn cheap_flip_rescue(
    attrs: &ItemAttributes,
    index: &PriceIndex,
    price: f64,
) -> Option<ModelEstimate> {
    let min_mult = *RESCUE_MIN_MULT;
    if min_mult <= 0.0 || price <= 0.0 || attrs.variant.is_empty() {
        return None;
    }
    let max_buy = *RESCUE_MAX_BUY;
    if max_buy > 0.0 && price > max_buy {
        return None;
    }
    let mut bare = attrs.clone();
    bare.variant = String::new();
    let bk = base_key(&bare);
    let median = index.base_value_for(&bk);
    let samples = index.sold_count_for_base(&bk);
    if median <= 0.0 || samples < *MIN_REFS as i64 {
        return None;
    }
    let mult = median / price;
    if mult < min_mult {
        return None;
    }
    // Report the BARE BASE's liquidity, not 0.0. We are pricing this item off
    // that base, so its volume is the honest answer — and 0.0 asserted "never
    // trades" about an item valued from a liquid pool, which the push filter's
    // `minVolumePerDay` (6 on prod) then rejected. That made the rescue dead
    // twice over: unreachable at RESCUE_MIN_MULT=10, and un-pushable if reached.
    let volume_per_day = index.base_volume_per_day(&bk);
    eprintln!(
        "RESCUE price={price:.0} bare_base={bk} median={median:.0} samples={samples} vol={volume_per_day:.1}/d mult={mult:.1}x variant={:?}",
        attrs.variant
    );
    Some(ModelEstimate {
        target: median,
        samples,
        volume_per_day,
        confidence: 0.5,
        coverage: 0.0,
        volatility: 0.0,
    })
}

/// The resale to fall back to when live listings already undercut our estimate:
/// the cheapest competitor, i.e. the price we would actually have to beat.
///
/// Returns `None` when nothing is cheaper (so the caller keeps its own estimate)
/// or when the flag is off. Callers MUST re-run their margin and profit gates
/// against the result. It only ever moves the estimate DOWN, so it can convert a
/// reject into an emit but never inflates a flip's claimed value.
fn undercut_reprice(competing: &[f64], resale: f64) -> Option<f64> {
    if !*UNDERCUT_REPRICE {
        return None;
    }
    cheapest_competitor_below(competing, resale)
}

/// Pure half of [`undercut_reprice`], split out so it is testable without the
/// env flag (a `LazyLock` other tests would race).
fn cheapest_competitor_below(competing: &[f64], resale: f64) -> Option<f64> {
    let cheapest = competing
        .iter()
        .copied()
        .filter(|p| p.is_finite() && *p > 0.0 && *p < resale)
        .fold(f64::INFINITY, f64::min);
    cheapest.is_finite().then_some(cheapest)
}

fn log_high_miss(r: Reject) {
    if *HIGH_MISS_MIN_PROFIT <= 0.0 {
        return;
    }
    CAND.with(|c| {
        let c = c.borrow();
        if c.est <= 0.0 || c.price <= 0.0 {
            return;
        }
        let profit = c.est - c.price;
        if profit < *HIGH_MISS_MIN_PROFIT {
            return;
        }
        eprintln!(
            "HIGHMISS reason={} basis={} profit={:.0} price={:.0} est={:.0} key={} item={:?} uuid={}",
            reject_name(r),
            c.basis,
            profit,
            c.price,
            c.est,
            c.key,
            c.item,
            c.uuid
        );
    });
}

/// Drain and reset the per-thread median funnel (call at sweep end).
pub fn reject_take() -> RejectStats {
    REJECT.with(|c| c.replace(RejectStats::default()))
}

/// Attribute a compound `profit<MIN || vol<MIN` reject to ONE reason (volume
/// first — it is the lever we care about — then profit). Control flow is
/// unchanged; this only decides which counter to bump.
fn bump_profit_gate(profit: f64, vol: f64) {
    if vol < *MIN_VOLUME_PER_DAY {
        reject_bump(Reject::Volume);
    } else if profit < *MIN_PROFIT {
        reject_bump(Reject::Profit);
    }
}

/// A live BIN listing (uuid + minor-adjusted price).
#[derive(Debug, Clone)]
pub struct Bin {
    pub uuid: String,
    pub price: f64,
}

#[derive(Debug, Clone)]
pub struct ActiveAuction {
    pub uuid: String,
    pub starting_bid: f64,
    pub auctioneer: Option<String>,
    pub item_name: String,
}

#[derive(Debug, Clone)]
pub struct DecodedAuction {
    pub a: ActiveAuction,
    pub attrs: ItemAttributes,
    pub key: String,
}

#[derive(Debug, Clone)]
pub struct Flip {
    pub uuid: String,
    pub item_name: String,
    pub finder: String,
    pub price: f64,
    pub reference: f64,
    pub profit: f64,
    pub roi_pct: f64,
    pub confidence: f64,
    pub samples: i64,
    pub key: String,
    pub guard: String,
    pub found_after_refresh_ms: f64,
    pub found_at_ms: f64,
    /// Decoded attributes of the item (for blacklist/own-listing/cost-basis on
    /// the serving side). Not part of the golden decision surface.
    pub attrs: ItemAttributes,
    /// Liquidity stats for median/model/snipe flips (None for lbin/dominance),
    /// consumed by the ws feed filter/grind gate and the discord embed.
    pub median_stats: Option<KeyStats>,
}

// NOTE (2026-07-15): a partial-scroll blade guard used to live here (mirrors the
// TS finder's removal in the same change). Any Hyperion/Astraea/Scylla/Valkyrie
// with SOME but not all of the three ability scrolls was refused outright as
// "scam bait". Prod sold history disproves that premise: partial-scroll blades
// are a liquid, consistently-priced tier (14d: 27 sales of 5*/1-scroll at ~850M
// median, 8 of 7*/1-scroll at ~1.14B). Pricing them is already safe by
// construction: `scroll:*` is a significant key feature (scrolls move a blade
// ~119% of clean base, well past ATTR_MIN_SHARE 0.15), and dominates_ref
// requires ref.scrolls to be a subset of the candidate's. Over-paying on stars
// is the craft ceiling's job. Do NOT reintroduce a blanket block.

// REMOVED 2026-07-30: the "too good to be true" guard (`MAX_PROFIT` +
// `TOO_GOOD_MIN_ROI`). It vetoed a flip purely for being profitable, and every
// time it fired on prod it was wrong. Do NOT reintroduce it.
//
// | item | ask | our est | profit | what happened |
// |---|---|---|---|---|
// | `[Lvl 200] Golden Dragon` | 800M | 1028.8M | 228.8M | competitor bought it, resold within 0.5% of OUR est |
// | `Ancient Crown of Avarice` | 500M | 664.0M | 164.0M | vetoed |
// | `Heroic Hyperion ✪✪✪✪✪` | 500M | 1156.1M | 656.1M | vetoed; 933 comparable sales put the median at 1260M |
//
// The Hyperion is the case that killed the idea. We detected it in the same
// millisecond a competitor did, our estimate was corroborated by 933 sales of
// the same 5-star/3-scroll/2-gem configuration, and we discarded it because the
// profit was 1.31x the ask.
//
// The guard cannot do its stated job. It claims to catch a garbage reference,
// but it only ever sees `price` and `profit`, which say nothing about reference
// quality. Real underpricing is common: across 76 high-value item_ids and 34,958
// sales in 21 days, 2.03% of REAL sales went at >=2x below their group median.
// A threshold low enough to catch a broken reference throws away that entire
// tail, which is where the money is.
//
// Reference quality is already measured, properly, by `refine_confidence` +
// `MIN_CONFIDENCE` (samples, volume, volatility, support) and clamped by
// `thin_key_evidence`, the live-listing floor, the lbin wall cap and
// `craft_ceiling`. Those run on evidence. This ran on a ratio, and it ran FIRST,
// so it got to veto flips those instruments would have passed.
//
// Same error as `CONF_MARGIN_PENALTY`, deleted for the same reason: big margins
// predict profit POSITIVELY.

/// Let an expensive item through on absolute profit when `MIN_MARGIN` alone
/// would reject it.
///
/// `MIN_MARGIN` is a flat percentage, so the coins it demands scale with the ask.
/// A 12% floor on a 960M Golden Dragon means refusing anything under 115M of
/// profit, and 58M gets thrown away. Audited against what those items actually
/// fetched afterwards (`missed_flip_audit`, 207 margin rejects with a real
/// stats/model valuation, realised net of the measured 1.12% AH fee):
///
/// | predicted margin | n | hit% | net | avg/flip | med TTS |
/// |---|---|---|---|---|---|
/// | 0-3%  | 59 | 78% | +0.39B |  6.6M | 1.3h |
/// | 3-6%  | 45 | 87% | +0.56B | 12.5M | 1.0h |
/// | 6-9%  | 26 | 92% | +0.25B |  9.7M | 1.1h |
/// | 9-12% | 20 | 95% | +0.17B |  8.5M | 1.1h |
///
/// Every band is positive and clears in about an hour, and the estimates driving
/// them are accurate (that Golden Dragon was priced within 0.1% of its realised
/// 1030M). CAVEAT, and the reason this ships OFF: the realised side can only be
/// measured on items that SOLD, so a flip we would have bought and then sat on
/// is invisible to it, and 44% of our own inventory has historically never
/// cleared. Treat the table as an upper bound.
///
/// Deliberately narrow, all three must hold:
///   - profit at least `MARGIN_OVERRIDE_MIN_PROFIT` in absolute coins
///   - margin still at least `MARGIN_OVERRIDE_MIN_MARGIN` (never a free pass)
///   - the key is genuinely liquid: `MARGIN_OVERRIDE_MIN_SAMPLES` sales AND
///     `MARGIN_OVERRIDE_MIN_VOLUME` per day, so this cannot fire on a thin pool
///
/// `MARGIN_OVERRIDE_MIN_PROFIT = 0` (the default) disables it entirely.
fn big_ticket_margin_ok(price: f64, resale: f64, samples: i64, volume_per_day: f64) -> bool {
    big_ticket_inner(
        price,
        resale,
        samples,
        volume_per_day,
        *MARGIN_OVERRIDE_MIN_PROFIT,
        *MARGIN_OVERRIDE_MIN_MARGIN,
        *MARGIN_OVERRIDE_MIN_SAMPLES,
        *MARGIN_OVERRIDE_MIN_VOLUME,
    )
}

/// Pure half of [`big_ticket_margin_ok`], tunables injected so it is testable
/// without touching process env.
#[allow(clippy::too_many_arguments)]
fn big_ticket_inner(
    price: f64,
    resale: f64,
    samples: i64,
    volume_per_day: f64,
    min_profit: f64,
    min_margin: f64,
    min_samples: i64,
    min_volume: f64,
) -> bool {
    if min_profit <= 0.0 || resale <= 0.0 || price <= 0.0 {
        return false;
    }
    if samples < min_samples || volume_per_day < min_volume {
        return false;
    }
    let margin = (resale - price) / resale;
    if margin < min_margin {
        return false;
    }
    after_tax(resale) - price >= min_profit
}

/// What a resale at `reference` actually nets, after BOTH Hypixel fees.
///
/// Two separate charges, and this used to model only half of one:
///
/// - **Listing fee**, `rate` below, charged on the ASK at every listing attempt,
///   sale or no sale. BIN is 6.0h and ~55% expire unsold, so the median flip
///   pays it several times ([`LIST_ATTEMPTS_EXPECTED`]).
/// - **Sale tax** ([`AH_SALE_TAX`]), charged once on a completed sale. Measured
///   3.5002% on a 603,900,000 sale, and previously not modelled at all.
///
/// Both defaults keep the old arithmetic byte-identical (one listing fee, no
/// sale tax), so goldens are unaffected until prod turns them on.
fn after_tax(reference: f64) -> f64 {
    after_tax_inner(reference, *LIST_ATTEMPTS_EXPECTED, *AH_SALE_TAX)
}

/// Expected listing attempts relative to the population mean, by POOL confidence.
///
/// Normalised by the derived population mean (1.68) so that multiplying by
/// [`LIST_ATTEMPTS_EXPECTED`] preserves the measured 2.9x SHAPE while leaving the
/// LEVEL exactly where prod set it. See [`LIST_ATTEMPTS_BY_CONF`] for why the
/// shape is trusted and the absolute anchor is not.
const ATTEMPTS_CONF_BANDS: [(f64, f64); 4] = [
    (0.90, 1.48 / 1.68),
    (0.80, 1.87 / 1.68),
    (0.70, 1.95 / 1.68),
    (0.00, 4.25 / 1.68),
];

/// [`after_tax`] keyed on pool confidence. `None`, a non-finite confidence, or
/// the flag being off all fall back to the flat constant, so every unwired lane
/// stays byte-identical.
fn after_tax_conf(reference: f64, confidence: Option<f64>) -> f64 {
    let attempts = match confidence {
        Some(c) if *LIST_ATTEMPTS_BY_CONF && c.is_finite() => {
            let scale = ATTEMPTS_CONF_BANDS
                .iter()
                .find(|(lo, _)| c >= *lo)
                .map(|(_, s)| *s)
                // c < 0.0 is not a real confidence; charge it as the worst band
                // rather than silently as the best.
                .unwrap_or(ATTEMPTS_CONF_BANDS[ATTEMPTS_CONF_BANDS.len() - 1].1);
            *LIST_ATTEMPTS_EXPECTED * scale
        }
        _ => *LIST_ATTEMPTS_EXPECTED,
    };
    after_tax_inner(reference, attempts, *AH_SALE_TAX)
}

/// COFL's `FlipInstance.GetFeeRateForStartingBid`, split into the two charges it
/// bundles, as PERCENT (not fractions), so the arithmetic can be checked against
/// their source line by line.
///
/// ```csharp
/// var reduction = 2f;
/// if (targetPrice > 10_000_000)  reduction = 3;
/// if (targetPrice >= 100_000_000) reduction = 3.5f;
/// if (date >= AuraStart && date < AuraEnd) return reduction + 1;  // early return
/// if (isDerpy && targetPrice >= 1_000_000) reduction += 3;        // 4x claim tax
/// ```
///
/// Their 2 / 3 / 3.5 is `listing tier (1 / 2 / 2.5) + a flat 1% claim tax`, which
/// is why Derpy's "claiming tax x4" is `+3` and why our own measurement of
/// **3.5002% on a 603,900,000 sale** matched their >=100M tier EXACTLY. That
/// measurement was recorded as a sale tax *on top of* the listing fee
/// ([`AH_SALE_TAX`] = 0.035), so the >=100M listing tier was being charged twice:
/// once inside the 3.5% and again as `0.025 * attempts`.
///
/// Returned separately because only the LISTING half is paid per attempt; the
/// claim tax is paid once, on the sale that actually happens.
///
/// ⚠️ COFL's lower boundary is strictly `>` 10M where ours was `>=`. Exactly
/// 10,000,000 is billed 1%, not 2%.
/// ⚠️ Aura `return`s early in COFL, so it does NOT stack with Derpy. Preserved.
fn cofl_fee_pct(reference: f64, now_ms: f64) -> (f64, f64) {
    let listing = if reference >= 100_000_000.0 {
        2.5
    } else if reference > 10_000_000.0 {
        2.0
    } else {
        1.0
    };
    if is_aura(now_ms) {
        // Aura adds 1% and returns before Derpy can also apply.
        return (listing, 2.0);
    }
    if is_derpy(now_ms) && reference >= 1_000_000.0 {
        return (listing, 4.0);
    }
    (listing, 1.0)
}

/// Mayor Derpy, whose term quadruples the AH claiming tax.
///
/// COFL anchors the rotation at `2024-08-26 07:15:00Z` and treats Derpy as
/// active for the first 124 hours of every `124 * 24`-hour cycle. Windows this
/// lands on: 2026-05-08, **2026-09-09**, 2027-01-11.
///
/// ⚠️ This is a CALENDAR GUESS, not an observation. Hypixel's mayor election is
/// not perfectly periodic, so a real term can drift off this schedule. It is
/// COFL's own approximation and is reproduced deliberately, but if a Derpy term
/// is ever seen to disagree, believe the game.
fn is_derpy(now_ms: f64) -> bool {
    if let Some(forced) = *AH_FEE_DERPY {
        return forced;
    }
    const DERPY_START_MS: f64 = 1_724_656_500_000.0; // 2024-08-26T07:15:00Z
    const CYCLE_H: f64 = 124.0 * 24.0;
    let hours = (now_ms - DERPY_START_MS) / 3_600_000.0;
    hours > 0.0 && hours.rem_euclid(CYCLE_H) < 124.0
}

/// Wall clock for the fee model, overridable by `AH_FEE_NOW_MS` so tests and
/// backtests are not at the mercy of which mayor is in office today.
fn now_ms_for_fees() -> f64 {
    let pinned = *AH_FEE_NOW_MS;
    if pinned > 0.0 {
        return pinned;
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// The Aura event window, which added a flat 1% to the AH fee.
/// Already over (2025-11-22 .. 2026-01-19 19:15Z); kept so the model stays
/// faithful if COFL extends it and so backtests over that window are correct.
fn is_aura(now_ms: f64) -> bool {
    const AURA_START_MS: f64 = 1_763_769_600_000.0; // 2025-11-22T00:00:00Z
    const AURA_END_MS: f64 = 1_768_857_300_000.0; // 2026-01-19T19:15:00Z
    (AURA_START_MS..AURA_END_MS).contains(&now_ms)
}

/// COFL's `ProfitAfterFees`, generalised over listing attempts.
///
/// `attempts = 1.0` is byte-identical to COFL: one listing fee, one claim tax,
/// `target * (100 - reduction) / 100`. Above 1.0 it charges the LISTING half per
/// attempt and the claim tax once, which is the honest generalisation for a
/// fleet whose real relist cadence is ~2.4 attempts. The claim tax is never
/// multiplied, because an item is only ever claimed once.
fn cofl_after_tax(reference: f64, attempts: f64, now_ms: f64) -> f64 {
    let (listing_pct, claim_pct) = cofl_fee_pct(reference, now_ms);
    let attempts = if attempts.is_finite() {
        attempts.max(1.0)
    } else {
        1.0
    };
    let pct = listing_pct * attempts + claim_pct;
    (reference * (1.0 - pct / 100.0)).max(reference * 0.05)
}

/// Pure half of [`after_tax`], both knobs injected so either state is testable
/// without touching process env.
///
/// Clamped at `reference * 0.05` so a mis-set `attempts` can never drive the
/// net negative and invert a profit test.
fn after_tax_inner(reference: f64, attempts: f64, sale_tax: f64) -> f64 {
    let rate = if reference >= 100_000_000.0 {
        0.025
    } else if reference >= 10_000_000.0 {
        0.02
    } else {
        0.01
    };
    let attempts = if attempts.is_finite() {
        attempts.max(1.0)
    } else {
        1.0
    };
    if *AH_FEE_COFL {
        return cofl_after_tax(reference, attempts, now_ms_for_fees());
    }
    let sale_tax = if sale_tax.is_finite() {
        sale_tax.max(0.0)
    } else {
        0.0
    };
    (reference * (1.0 - rate * attempts - sale_tax)).max(reference * 0.05)
}

/// Blend a "too good to be true" margin penalty toward 1.0 by market liquidity
/// (sample depth AND daily volume), capped by `waiver` strength. Pure and
/// config-free so it is directly unit-testable; the caller supplies the tunables.
/// `waiver <= 0` returns the penalty UNCHANGED (the default, so goldens/TS parity
/// hold). At full liquidity (samples >= `s_full` and volume >= `v_full`) with
/// `waiver = 1.0` the penalty is fully waived (a big margin on a deep, active
/// market is a real underprice, not a shaky reference). Thin or low-volume items
/// keep the full guard because either factor near 0 drives `liq -> 0`.
fn liquidity_waived(
    margin_factor: f64,
    samples: i64,
    volume_per_day: f64,
    waiver: f64,
    s_full: f64,
    v_full: f64,
) -> f64 {
    if waiver <= 0.0 || s_full <= 0.0 || v_full <= 0.0 {
        return margin_factor;
    }
    let liq = ((samples as f64 / s_full).clamp(0.0, 1.0)
        * (volume_per_day / v_full).clamp(0.0, 1.0)
        * waiver)
        .clamp(0.0, 1.0);
    margin_factor + (1.0 - margin_factor) * liq
}

#[allow(clippy::too_many_arguments)]
fn refine_confidence(
    base: f64,
    signals: &[f64],
    roi: f64,
    market_support: i64,
    samples: i64,
    volume_per_day: f64,
    volatility: f64,
) -> f64 {
    let ref_strength = (samples as f64 / 12.0).min(1.0) * (1.0 - volatility.clamp(0.0, 1.0));
    let mut penalty = 1.0;
    let valid: Vec<f64> = signals.iter().copied().filter(|s| *s > 0.0).collect();
    if valid.len() >= 2 {
        let mx = valid.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mn = valid.iter().copied().fold(f64::INFINITY, f64::min);
        let disp = if mx > 0.0 { (mx - mn) / mx } else { 1.0 };
        let agree = (1.0 - disp / 0.25).max(0.0);
        penalty *= 0.6 + 0.4 * agree;
    } else {
        penalty *= 0.85;
    }
    if roi > 0.6 && *CONF_MARGIN_PENALTY > 0.0 {
        let mut margin_factor = 1.0 / (1.0 + (roi - 0.6) * 0.8 * *CONF_MARGIN_PENALTY);
        if market_support >= 2 {
            margin_factor = (margin_factor + 0.3).min(1.0);
        }
        // A huge margin on a proven-liquid market (e.g. 929 sales, 200+/day) is a
        // genuine underprice, so waive the penalty in proportion to liquidity.
        // OFF by default (CONF_LIQ_WAIVER=0) so goldens/TS parity are untouched.
        margin_factor = liquidity_waived(
            margin_factor,
            samples,
            volume_per_day,
            *CONF_LIQ_WAIVER,
            *CONF_LIQ_SAMPLES,
            *CONF_LIQ_VOLUME,
        );
        penalty *= margin_factor;
    }
    penalty += (1.0 - penalty) * ref_strength;
    (base * penalty).clamp(0.0, 1.0)
}

fn median_guards(s: &KeyStats) -> String {
    let mut g: Vec<&str> = Vec::new();
    if s.manipulated {
        g.push("manipulated");
    }
    if s.volatility > 0.4 {
        g.push("volatile");
    }
    if s.last_sold_ago_h > 48.0 {
        g.push("stale");
    }
    if s.volume_per_day < 3.0 {
        g.push("low_volume");
    }
    if s.trend_pct < -0.05 {
        g.push("falling");
    }
    if g.is_empty() {
        "none".to_string()
    } else {
        g.join(",")
    }
}

// --- relist-spam guard ---
#[derive(Default)]
pub struct RelistTracker {
    map: HashMap<String, (HashSet<String>, f64)>,
}

impl RelistTracker {
    fn seller_ok(seller: &Option<String>) -> Option<&str> {
        match seller {
            Some(s) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        }
    }

    fn relist_blocked(&self, seller: &Option<String>, key: &str, uuid: &str) -> bool {
        let s = match Self::seller_ok(seller) {
            Some(s) => s,
            None => return false,
        };
        match self.map.get(&format!("{s}|{key}")) {
            Some((uuids, _)) => !uuids.contains(uuid) && uuids.len() >= *SELLER_RELIST_LIMIT,
            None => false,
        }
    }

    fn record_relist(&mut self, seller: &Option<String>, key: &str, uuid: &str, now_ms: f64) {
        let s = match Self::seller_ok(seller) {
            Some(s) => s,
            None => return,
        };
        let entry = self.map.entry(format!("{s}|{key}")).or_default();
        entry.0.insert(uuid.to_string());
        entry.1 = now_ms;
        // Bounded memory: drop pairs not seen in 7 days once the map grows large
        // (sniper.ts:98-102). Only reachable once the tracker outlives a single
        // sweep — which it must, or the relist-spam guard never accumulates.
        if self.map.len() > 20_000 {
            let cutoff = now_ms - 7.0 * 86_400_000.0;
            self.map.retain(|_, e| e.1 >= cutoff);
        }
    }
}

fn live_lbin(competing: &[f64]) -> Option<f64> {
    if competing.is_empty() {
        None
    } else {
        Some(competing.iter().copied().fold(f64::INFINITY, f64::min))
    }
}

fn competing_prices(bykey: &HashMap<String, Vec<Bin>>, key: &str, self_uuid: &str) -> Vec<f64> {
    bykey
        .get(key)
        .map(|l| {
            l.iter()
                .filter(|x| x.uuid != self_uuid)
                .map(|x| x.price)
                .collect()
        })
        .unwrap_or_default()
}

/// Clean-item snipe lane.
pub fn eval_clean_snipe(
    d: &DecodedAuction,
    index: &PriceIndex,
    undercut_by_key: &HashMap<String, Vec<Bin>>,
    seen: &mut HashSet<String>,
    relist: &mut RelistTracker,
    dump_last_updated: f64,
    now_ms: f64,
) -> Option<Flip> {
    let (a, attrs, key) = (&d.a, &d.attrs, &d.key);
    let clean = index.clean_snipe(attrs)?;
    if clean.samples < *MIN_REFS as i64 {
        return None;
    }
    let competing = competing_prices(undercut_by_key, key, &a.uuid);
    let live = live_lbin(&competing);
    let mut resale = clean.target;
    if let Some(l) = live {
        if l < resale {
            resale = l;
        }
    }
    let price = a.starting_bid;
    if price > resale * (1.0 - *CLEAN_SNIPE_MARGIN) {
        return None;
    }
    let profit = after_tax(resale) - price;
    if profit < *MIN_PROFIT {
        return None;
    }
    if relist.relist_blocked(&a.auctioneer, key, &a.uuid) {
        return None;
    }
    let support = competing.iter().filter(|p| **p >= resale * 0.9).count() as i64;
    let mut signals = vec![resale];
    if let Some(l) = live {
        signals.push(l);
    }
    let conf = refine_confidence(
        0.9,
        &signals,
        profit / price,
        support,
        clean.samples,
        clean.volume_per_day,
        0.0,
    );
    relist.record_relist(&a.auctioneer, key, &a.uuid, now_ms);
    seen.insert(a.uuid.clone());
    Some(Flip {
        uuid: a.uuid.clone(),
        item_name: a.item_name.clone(),
        finder: "snipe".to_string(),
        attrs: attrs.clone(),
        median_stats: Some(KeyStats {
            target: resale,
            samples: clean.samples,
            volume_per_day: clean.volume_per_day,
            spread_pct: 0.0,
            lowest_ref: resale,
            highest_ref: resale,
            last_sold_ago_h: 0.0,
            confidence: conf,
            volatility: 0.0,
            manipulated: false,
            trend_pct: 0.0,
        }),
        price,
        reference: resale,
        profit,
        roi_pct: (profit / price) * 100.0,
        confidence: conf,
        samples: clean.samples,
        key: key.clone(),
        guard: "clean_snipe".to_string(),
        found_after_refresh_ms: now_ms - dump_last_updated,
        found_at_ms: now_ms,
    })
}

/// Worker-side pre-screen → 'c' | 'l' | 'r'.
pub fn screen_auction(d: &DecodedAuction, index: &PriceIndex, model: &ModifierModel) -> char {
    let (attrs, key) = (&d.attrs, &d.key);
    let price = d.a.starting_bid;
    if let Some(clean) = index.clean_snipe(attrs) {
        if clean.samples >= *MIN_REFS as i64
            && price <= clean.target * (1.0 - *CLEAN_SNIPE_MARGIN)
            && after_tax(clean.target) - price >= *MIN_PROFIT
        {
            return 'c';
        }
    }
    if let Some(qt) = index.cheap_median(key) {
        if price > qt * 1.3 * (1.0 - *MIN_MARGIN) * *PRESCREEN_SLACK {
            return 'r';
        }
    }
    match index.price_for_key(attrs, key) {
        None => {
            let high = index.high_for_base(&base_key(attrs));
            if high > 0.0 && price > high * 1.5 * (1.0 - *MIN_MARGIN) * *PRESCREEN_SLACK {
                return 'r';
            }
            let model_est = model.estimate_for(attrs, index);
            match model_est {
                Some(m) if m.samples >= *MIN_REFS as i64 => {
                    if price > m.target * (1.0 - *MIN_MARGIN) {
                        return 'r';
                    }
                    let profit = after_tax(m.target) - price;
                    if profit < *MIN_PROFIT || m.volume_per_day < *MIN_VOLUME_PER_DAY {
                        return 'r';
                    }
                    'c'
                }
                _ => 'l',
            }
        }
        Some(stats) => {
            if stats.trend_pct <= -0.15 {
                return 'r';
            }
            let minor_credit = (index.minor_feature_value(attrs) * 0.5).min(stats.target * 0.3);
            let trend_scale = 1.0 + stats.trend_pct.min(0.0) * 0.5;
            let resale = (stats.target + minor_credit) * trend_scale;
            if price > resale * (1.0 - *MIN_MARGIN) {
                return 'r';
            }
            let profit = after_tax(resale) - price;
            if profit < *MIN_PROFIT || stats.volume_per_day < *MIN_VOLUME_PER_DAY {
                return 'r';
            }
            'c'
        }
    }
}

/// Median finder for a single auction. Returns (flip, priceable).
#[allow(clippy::too_many_arguments)]
pub fn eval_median_flip(
    d: &DecodedAuction,
    index: &PriceIndex,
    model: &ModifierModel,
    undercut_by_key: &HashMap<String, Vec<Bin>>,
    seen: &mut HashSet<String>,
    relist: &mut RelistTracker,
    dump_last_updated: f64,
    now_ms: f64,
) -> (Option<Flip>, bool) {
    let (a, attrs, key) = (&d.a, &d.attrs, &d.key);
    let price = a.starting_bid;
    cand_begin(&a.uuid, &a.item_name, key, price);
    if let Some(qt) = index.cheap_median(key) {
        cand_est(qt * 1.3, "cheap");
        if price > qt * 1.3 * (1.0 - *MIN_MARGIN) * *PRESCREEN_SLACK {
            reject_bump(Reject::Margin);
            return (None, true);
        }
    }
    let competing = competing_prices(undercut_by_key, key, &a.uuid);
    let live = live_lbin(&competing);

    let mut stats = index.price_for_key(attrs, key);
    // The exact key is fragmented ("notpriceable") for most of the high-value
    // miss surface — 62% of reject events and 91% of missed profit on prod,
    // all `basis=high`. Before falling to the model lane, walk the ladder:
    // drop the key's features by ascending known value until a pool with
    // ≥MIN_REFS prices the item. Under-prices a premium variant at worst; can
    // never inflate what the flip is claimed to be worth. Off unless KEY_LADDER.
    let mut ladder_depth = 0usize;
    if stats.is_none() {
        if let Some((s, depth)) = index.ladder_price(attrs, key) {
            stats = Some(s);
            ladder_depth = depth;
        }
    }
    if stats.is_none() {
        let high = index.high_for_base(&base_key(attrs));
        cand_est(high * 1.5, "high");
        if high > 0.0 && price > high * 1.5 * (1.0 - *MIN_MARGIN) * *PRESCREEN_SLACK {
            reject_bump(Reject::Margin);
            return (None, true);
        }
        let model_est = model.estimate_for(attrs, index);
        let model_est = match model_est {
            Some(m) if m.samples >= *MIN_REFS as i64 => m,
            _ => match cheap_flip_rescue(attrs, index, price) {
                Some(m) => m,
                None => {
                    reject_bump(Reject::NotPriceable);
                    return (None, false);
                }
            },
        };
        let mut model_resale = model_est.target;
        if let Some(l) = live {
            if l < model_resale {
                model_resale = l;
            }
        }
        let thin = index.thin_key_evidence(key);
        if let Some(t) = &thin {
            if t.n >= 2 && t.median * 1.1 < model_resale {
                model_resale = t.median * 1.1;
            }
        }
        let cc = craft_ceiling(attrs, index);
        let craft_capped = cc.map(|c| c < model_resale).unwrap_or(false);
        if craft_capped {
            model_resale = cc.unwrap();
        }
        cand_est(model_resale, "model");
        if price > model_resale * (1.0 - *MIN_MARGIN) {
            reject_bump(Reject::Margin);
            return (None, true);
        }
        let profit = after_tax_conf(model_resale, Some(model_est.confidence)) - price;
        if profit < *MIN_PROFIT || model_est.volume_per_day < *MIN_VOLUME_PER_DAY {
            bump_profit_gate(profit, model_est.volume_per_day);
            return (None, true);
        }
        let support = competing
            .iter()
            .filter(|p| **p >= model_resale * 0.9)
            .count() as i64;
        let mut signals = vec![model_resale];
        if let Some(l) = live {
            signals.push(l);
        }
        if let Some(t) = &thin {
            signals.push(t.median);
        }
        let conf = refine_confidence(
            model_est.confidence,
            &signals,
            profit / price,
            support,
            model_est.samples,
            model_est.volume_per_day,
            model_est.volatility,
        );
        if conf < *MIN_CONFIDENCE {
            reject_bump(Reject::Confidence);
            return (None, true);
        }
        let undercuts = competing.iter().filter(|p| **p < model_resale).count() as i64;
        if undercuts >= *MEDIAN_MAX_UNDERCUTS {
            reject_bump(Reject::Undercuts);
            return (None, true);
        }
        if relist.relist_blocked(&a.auctioneer, key, &a.uuid) {
            reject_bump(Reject::Relist);
            return (None, true);
        }
        reject_bump(Reject::Emitted);
        relist.record_relist(&a.auctioneer, key, &a.uuid, now_ms);
        seen.insert(a.uuid.clone());
        // What the filter's liquidity gate reads. See `ModifierModel::reported_volume`:
        // the base rate flatters a rare variant priced off modifier deltas.
        let (reported_volume, band_applied) =
            model.reported_volume(attrs, price, model_resale, model_est.volume_per_day);
        let mut guards = vec!["model".to_string()];
        if band_applied {
            guards.push("band_vol".to_string());
        }
        if model_est.coverage < 1.0 {
            guards.push("partial_model".to_string());
        }
        if model_est.volatility > 0.4 {
            guards.push("volatile".to_string());
        }
        if craft_capped {
            guards.push("craft_capped".to_string());
        }
        return (
            Some(Flip {
                uuid: a.uuid.clone(),
                item_name: a.item_name.clone(),
                finder: "model".to_string(),
                attrs: attrs.clone(),
                median_stats: Some(KeyStats {
                    target: model_resale,
                    samples: model_est.samples,
                    volume_per_day: reported_volume,
                    spread_pct: model_est.volatility,
                    lowest_ref: model_est.target,
                    highest_ref: model_est.target,
                    last_sold_ago_h: 0.0,
                    confidence: conf,
                    volatility: model_est.volatility,
                    manipulated: false,
                    trend_pct: 0.0,
                }),
                price,
                reference: model_resale,
                profit,
                roi_pct: (profit / price) * 100.0,
                confidence: conf,
                samples: model_est.samples,
                key: key.clone(),
                guard: guards.join(","),
                found_after_refresh_ms: now_ms - dump_last_updated,
                found_at_ms: now_ms,
            }),
            true,
        );
    }

    let mut stats = stats.unwrap();
    // Guard-refresh: the exact key PRICED, but its last sale is old enough that
    // `median_guards` tags `stale` (>48h) and the client's blocked_guards then
    // refuses the push — even when the item's bare FAMILY trades daily at the
    // same level. Measured 2026-08-15: Bouquet of Lies*1 at buy 2.67M vs base
    // LBIN 17M (454% ROI) died on guard 'stale', priced off 13 samples whose
    // last sale was 98h back, while the unstarred pool was fresh and liquid.
    // Walk the ladder for a fresher pool and adopt ITS evidence —
    // freshness/volume/trend from the rung — with the target floored at
    // min(exact, rung) so family evidence can never RAISE the claimed resale;
    // the deviation from the exact reading goes only downward. The conf
    // degrade per rung and the `ladder{N}` guard tag both still apply. Trigger
    // matches the median_guards stale threshold exactly, and a starred key
    // whose family is also stale simply gets no fresher rung (last_sold is the
    // pool's own), so this is self-limiting. Off unless KEY_LADDER.
    if ladder_depth == 0 && stats.last_sold_ago_h > 48.0 {
        if let Some((mut rung, depth)) = index.ladder_price(attrs, key) {
            rung.target = rung.target.min(stats.target);
            stats = rung;
            ladder_depth = depth;
        }
    }
    let ladder_basis: &'static str = if ladder_depth > 0 { "ladder" } else { "stats" };
    cand_est(stats.target, ladder_basis);
    if stats.trend_pct <= -0.15 {
        reject_bump(Reject::Falling);
        return (None, true);
    }
    let minor_credit = (index.minor_feature_value(attrs) * 0.5).min(stats.target * 0.3);
    let trend_scale = 1.0 + stats.trend_pct.min(0.0) * 0.5;
    let mut resale = (stats.target + minor_credit) * trend_scale;
    // The pool may be mostly other variants than this one. Applied HERE, not in
    // `compute_price_for`, because that is memoised by key alone. Lowers only.
    if let Some(ff) = index.feature_floor(attrs, resale) {
        if ff < resale {
            resale = ff;
        }
    }
    if let Some(l) = live {
        if l < resale {
            resale = l;
        }
    }
    cand_est(resale, ladder_basis);
    if price > resale * (1.0 - *MIN_MARGIN)
        && !big_ticket_margin_ok(price, resale, stats.samples, stats.volume_per_day)
    {
        reject_bump(Reject::Margin);
        return (None, true);
    }
    let mut profit = after_tax_conf(resale, Some(stats.confidence)) - price;
    if profit < *MIN_PROFIT || stats.volume_per_day < *MIN_VOLUME_PER_DAY {
        bump_profit_gate(profit, stats.volume_per_day);
        return (None, true);
    }
    let model_est = model.estimate_for(attrs, index);
    let support = competing.iter().filter(|p| **p >= resale * 0.9).count() as i64;
    let mut signals = vec![resale];
    if let Some(m) = &model_est {
        signals.push(m.target);
    }
    if let Some(l) = live {
        signals.push(l);
    }
    let conf = refine_confidence(
        stats.confidence,
        &signals,
        profit / price,
        support,
        stats.samples,
        stats.volume_per_day,
        stats.volatility,
    );
    if conf >= *MIN_CONFIDENCE {
        // Counted against `resale`, the number we actually ask, NOT `stats.target`.
        // Every neighbouring gate (margin, profit, support) and both sibling lanes
        // use their own resale, and `live` has already clamped `resale` DOWN, so
        // measuring here against the higher target counted listings as undercutting
        // us that in fact sit at or above what we would ask.
        let undercuts = competing.iter().filter(|p| **p < resale).count() as i64;
        let mut undercut_repriced = false;
        if undercuts >= *MEDIAN_MAX_UNDERCUTS {
            // Don't discard the flip. We are buying at `price`, so the question is
            // whether it still pays AFTER undercutting them. Re-run every money gate
            // against the lowered resale; `undercut_reprice` only moves it DOWN.
            let repriced = undercut_reprice(&competing, resale).filter(|r| {
                let p = after_tax_conf(*r, Some(stats.confidence)) - price;
                price <= *r * (1.0 - *MIN_MARGIN) && p >= *MIN_PROFIT
            });
            match repriced {
                Some(r) => {
                    resale = r;
                    profit = after_tax_conf(r, Some(stats.confidence)) - price;
                    undercut_repriced = true;
                    cand_est(resale, ladder_basis);
                }
                None => {
                    reject_bump(Reject::Undercuts);
                    return (None, true);
                }
            }
        }
        if relist.relist_blocked(&a.auctioneer, key, &a.uuid) {
            reject_bump(Reject::Relist);
            return (None, true);
        }
        reject_bump(Reject::Emitted);
        relist.record_relist(&a.auctioneer, key, &a.uuid, now_ms);
        seen.insert(a.uuid.clone());
        let mut stats2 = stats.clone();
        stats2.confidence = conf;
        return (
            Some(Flip {
                uuid: a.uuid.clone(),
                item_name: a.item_name.clone(),
                finder: "median".to_string(),
                attrs: attrs.clone(),
                median_stats: Some(stats2.clone()),
                price,
                reference: resale,
                profit,
                roi_pct: (profit / price) * 100.0,
                confidence: conf,
                samples: stats2.samples,
                key: key.clone(),
                guard: {
                    let g = if undercut_repriced {
                        let g = median_guards(&stats2);
                        if g.is_empty() {
                            "undercut_repriced".to_string()
                        } else {
                            format!("{g},undercut_repriced")
                        }
                    } else {
                        median_guards(&stats2)
                    };
                    // Ladder-priced flips are tagged so the ws-config can block
                    // them as a class (`blocked_guards`) rather than tuning
                    // them blind, and so the discord embed shows the depth.
                    if ladder_depth > 0 {
                        format!("{g},ladder{ladder_depth}")
                    } else {
                        g
                    }
                },
                found_after_refresh_ms: now_ms - dump_last_updated,
                found_at_ms: now_ms,
            }),
            true,
        );
    }
    reject_bump(Reject::Confidence);
    (None, true)
}

/// Dominance finder over the unpriceable tail.
#[allow(clippy::too_many_arguments)]
pub fn eval_dominance_flips(
    candidates: &[DecodedAuction],
    bykey: &HashMap<String, Vec<Bin>>,
    seen: &mut HashSet<String>,
    relist: &mut RelistTracker,
    dump_last_updated: f64,
    index: &PriceIndex,
    now_ms: f64,
) -> Vec<Flip> {
    let mut flips = Vec::new();
    for d in candidates {
        let (a, attrs, key) = (&d.a, &d.attrs, &d.key);
        if seen.contains(&a.uuid) {
            continue;
        }
        let dom = match index.dominance_floor(attrs) {
            Some(dm) if dm.samples >= *MIN_REFS as i64 => dm,
            _ => continue,
        };
        let competing = competing_prices(bykey, key, &a.uuid);
        let live = live_lbin(&competing);
        let mut resale = dom.target;
        if let Some(l) = live {
            if l < resale {
                resale = l;
            }
        }
        let cc = craft_ceiling(attrs, index);
        let craft_capped = cc.map(|c| c < resale).unwrap_or(false);
        if craft_capped {
            resale = cc.unwrap();
        }
        let price = a.starting_bid;
        if price > resale * (1.0 - *MIN_MARGIN) {
            continue;
        }
        let profit = after_tax(resale) - price;
        if profit < *MIN_PROFIT || dom.volume_per_day < *MIN_VOLUME_PER_DAY {
            continue;
        }
        let support = competing.iter().filter(|p| **p >= resale * 0.9).count() as i64;
        let mut signals = vec![resale];
        if let Some(l) = live {
            signals.push(l);
        }
        let mut conf = refine_confidence(
            0.7,
            &signals,
            profit / price,
            support,
            dom.samples,
            dom.volume_per_day,
            0.0,
        );
        if dom.manipulated {
            conf *= 0.5;
        }
        if conf < *MIN_CONFIDENCE {
            continue;
        }
        let undercuts = competing.iter().filter(|p| **p < resale).count() as i64;
        if undercuts >= *MEDIAN_MAX_UNDERCUTS {
            continue;
        }
        if relist.relist_blocked(&a.auctioneer, key, &a.uuid) {
            continue;
        }
        relist.record_relist(&a.auctioneer, key, &a.uuid, now_ms);
        seen.insert(a.uuid.clone());
        let mut g: Vec<&str> = vec!["dominance"];
        if dom.manipulated {
            g.push("manipulated");
        }
        if craft_capped {
            g.push("craft_capped");
        }
        flips.push(Flip {
            uuid: a.uuid.clone(),
            item_name: a.item_name.clone(),
            finder: "dominance".to_string(),
            attrs: attrs.clone(),
            median_stats: None,
            price,
            reference: resale,
            profit,
            roi_pct: (profit / price) * 100.0,
            confidence: conf,
            samples: dom.samples,
            key: key.clone(),
            guard: g.join(","),
            found_after_refresh_ms: now_ms - dump_last_updated,
            found_at_ms: now_ms,
        });
    }
    flips
}

/// Lbin finder over the full current dump.
#[allow(clippy::too_many_arguments)]
pub fn eval_lbin_flips(
    candidates: &[DecodedAuction],
    bykey: &HashMap<String, Vec<Bin>>,
    seen: &mut HashSet<String>,
    relist: &mut RelistTracker,
    dump_last_updated: f64,
    index: &PriceIndex,
    now_ms: f64,
) -> Vec<Flip> {
    let mut flips = Vec::new();
    for d in candidates {
        let (a, attrs, key) = (&d.a, &d.attrs, &d.key);
        if seen.contains(&a.uuid) {
            continue;
        }
        let price = a.starting_bid;
        if index.sold_count_for_base(&base_key(attrs)) < *LBIN_MIN_SOLD {
            continue;
        }
        let list = match bykey.get(key) {
            Some(l) => l,
            None => continue,
        };
        if list.len() < *LBIN_MIN_LISTINGS {
            continue;
        }
        if list[0].uuid != a.uuid {
            continue;
        }
        let mut reference = list[1].price;
        let cc = craft_ceiling(attrs, index);
        let craft_capped = cc.map(|c| c < reference).unwrap_or(false);
        if craft_capped {
            reference = cc.unwrap();
        }
        // The reference above is the 2nd-cheapest LIVE listing, which can be a
        // wall priced far above what the item actually sells for. Cap it at the
        // sold-history median (+ tolerance) so a lone overpriced listing cannot
        // manufacture a phantom flip against a fantasy exit price.
        let mut wall_capped = false;
        if let Some(sold_med) = index.cheap_median(key) {
            let cap = sold_med * *LBIN_REF_SOLD_MULT;
            if cap < reference {
                reference = cap;
                wall_capped = true;
            }
        }
        let profit = after_tax(reference) - price;
        if price <= reference * (1.0 - *MIN_MARGIN) && profit >= *MIN_PROFIT {
            let depth = (list.len() as f64 / 6.0).min(1.0);
            let upper: Vec<f64> = list.iter().skip(1).map(|x| x.price).collect();
            let spread = if upper.len() > 1 {
                (upper[upper.len() - 1] - upper[0]) / reference
            } else {
                0.0
            };
            let confidence = 0.6 * depth + 0.4 * (1.0 - spread).max(0.0);
            if confidence < *MIN_CONFIDENCE {
                continue;
            }
            let mut guards: Vec<&str> = Vec::new();
            if list.len() < 6 {
                guards.push("thin_market");
            }
            if craft_capped {
                guards.push("craft_capped");
            }
            if wall_capped {
                guards.push("wall_capped");
            }
            if relist.relist_blocked(&a.auctioneer, key, &a.uuid) {
                continue;
            }
            relist.record_relist(&a.auctioneer, key, &a.uuid, now_ms);
            seen.insert(a.uuid.clone());
            flips.push(Flip {
                uuid: a.uuid.clone(),
                item_name: a.item_name.clone(),
                finder: "lbin".to_string(),
                attrs: attrs.clone(),
                median_stats: None,
                price,
                reference,
                profit,
                roi_pct: (profit / price) * 100.0,
                confidence,
                samples: list.len() as i64,
                key: key.clone(),
                guard: if guards.is_empty() {
                    "none".to_string()
                } else {
                    guards.join(",")
                },
                found_after_refresh_ms: now_ms - dump_last_updated,
                found_at_ms: now_ms,
            });
        }
    }
    flips
}

#[cfg(test)]
mod big_ticket_tests {
    use super::big_ticket_inner;

    // The suggested first setting from the config doc.
    const P: f64 = 25_000_000.0;
    const M: f64 = 0.06;
    const S: i64 = 8;
    const V: f64 = 2.0;

    // Disabled by default: MIN_MARGIN must behave exactly as it does today,
    // which is what the sniper goldens pin.
    #[test]
    fn off_by_default() {
        assert!(!big_ticket_inner(960e6, 1028.9e6, 40, 10.0, 0.0, M, S, V));
    }

    // The real prod reject: 960M ask, 1028.9M estimate, realised 1030M. 6.7%
    // margin, so MIN_MARGIN=0.12 killed 58M of profit on a deep pet pool.
    #[test]
    fn the_golden_dragon_gets_through() {
        assert!(big_ticket_inner(960e6, 1028.9e6, 40, 10.0, P, M, S, V));
    }

    // A thin or quiet pool must never qualify, however big the coins look:
    // absolute profit is exactly the signal that tempts us onto illiquid items.
    #[test]
    fn thin_or_quiet_pools_never_qualify() {
        assert!(
            !big_ticket_inner(960e6, 1028.9e6, 3, 10.0, P, M, S, V),
            "too few samples"
        );
        assert!(
            !big_ticket_inner(960e6, 1028.9e6, 40, 0.5, P, M, S, V),
            "too little volume"
        );
    }

    // Never a free pass: the floor margin still binds, and small coins still lose.
    #[test]
    fn the_floors_still_bind() {
        // 2% margin, under MARGIN_OVERRIDE_MIN_MARGIN
        assert!(!big_ticket_inner(1000e6, 1020e6, 40, 10.0, P, M, S, V));
        // 10% margin but only 4M of profit, under MARGIN_OVERRIDE_MIN_PROFIT
        assert!(!big_ticket_inner(45e6, 50e6, 40, 10.0, P, M, S, V));
    }

    // after_tax is applied before the profit test, so a flip that only clears the
    // floor gross must not sneak through net.
    #[test]
    fn profit_is_measured_after_tax() {
        // resale 400M, ask 374M: gross 26M clears 25M, but 2.5% tax leaves 16M.
        assert!(!big_ticket_inner(374e6, 400e6, 40, 10.0, P, M, S, V));
    }

    #[test]
    fn degenerate_inputs_are_refused() {
        assert!(!big_ticket_inner(0.0, 1000e6, 40, 10.0, P, M, S, V));
        assert!(!big_ticket_inner(100e6, 0.0, 40, 10.0, P, M, S, V));
    }
}

#[cfg(test)]
mod liquidity_waiver_tests {
    use super::liquidity_waived;

    // roi 11.27 -> raw margin_factor ~0.10 (the Mithril Drill case). Waiver OFF must
    // leave it untouched (this is what keeps goldens/TS parity green by default).
    #[test]
    fn off_by_default_is_a_noop() {
        assert_eq!(liquidity_waived(0.1, 929, 207.0, 0.0, 30.0, 20.0), 0.1);
    }

    // Deep + active market at full waiver strength -> penalty essentially gone.
    #[test]
    fn deep_liquid_market_waives_the_penalty() {
        let w = liquidity_waived(0.1, 929, 207.0, 1.0, 30.0, 20.0);
        assert!(
            w > 0.99,
            "expected near-full waiver for a 929-sample/207-vol item, got {w}"
        );
    }

    // The guard must still bite when EITHER liquidity factor is weak, so a big
    // margin on a thin or quiet market stays distrusted even with the waiver on.
    #[test]
    fn thin_or_low_volume_keeps_the_guard() {
        let thin = liquidity_waived(0.1, 3, 207.0, 1.0, 30.0, 20.0);
        assert!(thin < 0.2, "few samples must keep the guard, got {thin}");
        let quiet = liquidity_waived(0.1, 929, 1.0, 1.0, 30.0, 20.0);
        assert!(
            quiet < 0.2,
            "low daily volume must keep the guard, got {quiet}"
        );
    }

    // Waiver only ever RELAXES the penalty (0..1 blend toward 1.0), never tightens.
    #[test]
    fn never_reduces_confidence() {
        for &(s, v) in &[(0i64, 0.0f64), (10, 5.0), (929, 207.0), (50, 40.0)] {
            let w = liquidity_waived(0.1, s, v, 1.0, 30.0, 20.0);
            assert!(
                w >= 0.1 - 1e-12,
                "waiver must not lower margin_factor (s={s} v={v} -> {w})"
            );
        }
    }
}

#[cfg(test)]
mod fee_model_tests {
    //! See [`crate::config::LIST_ATTEMPTS_EXPECTED`] and [`crate::config::AH_SALE_TAX`].
    use super::{after_tax_inner, ATTEMPTS_CONF_BANDS};

    /// Mossy Helianthus Boots, 2026-08-14: bought 50.00M against a 58.52M resale
    /// at 93% confidence, reported 3.66M profit / 7% ROI while COFL called the
    /// same trade 6.69M. User: *"we calculate taxes too high or sum"*.
    ///
    /// Our ESTIMATE was the higher of the two (58.52M vs their 57.85M); the whole
    /// gap was the fee. This pins both the diagnosis and the fix.
    #[test]
    fn a_liquid_flip_is_not_charged_the_junk_relist_rate() {
        let (buy, resale) = (50.00e6, 58.52e6);

        // What prod charged: the flat population mean on a 93%-confidence item.
        let flat = after_tax_inner(resale, 2.41, 0.035) - buy;
        assert!(
            (flat - 3.65e6).abs() < 0.05e6,
            "should reproduce the 3.66M on the card, got {:.0}",
            flat
        );

        // Same trade with the confidence-scaled attempts. 2.41 * (1.48/1.68).
        let scale = ATTEMPTS_CONF_BANDS
            .iter()
            .find(|(lo, _)| 0.93 >= *lo)
            .unwrap()
            .1;
        let scaled = after_tax_inner(resale, 2.41 * scale, 0.035) - buy;
        assert!(
            scaled > flat,
            "a liquid pool must be charged less, not more"
        );
        assert!(
            (scaled - 3.99e6).abs() < 0.05e6,
            "expected ~3.99M profit, got {:.0}",
            scaled
        );

        // ⛔ The point is REALLOCATION, not a blanket discount. Junk must get
        // charged MORE than the flat constant, or this is just a loosened gate.
        let junk = ATTEMPTS_CONF_BANDS.last().unwrap().1;
        assert!(
            junk > 1.0,
            "conf<0.70 must scale ABOVE the population mean, got {junk}"
        );
        let junk_profit = after_tax_inner(resale, 2.41 * junk, 0.035) - buy;
        assert!(
            junk_profit < flat,
            "a low-confidence pool must be charged more than the flat constant"
        );
    }

    /// The bands must be ordered and must bracket the population mean, or the
    /// lookup silently picks the wrong row.
    #[test]
    fn conf_bands_are_descending_and_straddle_the_mean() {
        let mut prev_lo = f64::INFINITY;
        let mut prev_scale = 0.0;
        for (lo, scale) in ATTEMPTS_CONF_BANDS {
            assert!(lo < prev_lo, "thresholds must descend");
            assert!(
                scale > prev_scale,
                "lower confidence must cost MORE attempts"
            );
            prev_lo = lo;
            prev_scale = scale;
        }
        // Straddling the mean is what makes this a reallocation.
        assert!(ATTEMPTS_CONF_BANDS[0].1 < 1.0);
        assert!(ATTEMPTS_CONF_BANDS[3].1 > 1.0);
    }

    #[test]
    fn defaults_are_the_old_single_listing_fee() {
        // One attempt, no sale tax => exactly the pre-2026-08-09 arithmetic.
        assert_eq!(after_tax_inner(200e6, 1.0, 0.0), 200e6 * 0.975);
        assert_eq!(after_tax_inner(50e6, 1.0, 0.0), 50e6 * 0.98);
        assert_eq!(after_tax_inner(5e6, 1.0, 0.0), 5e6 * 0.99);
    }

    #[test]
    fn charges_the_listing_fee_once_per_attempt_plus_the_sale_tax() {
        // The crown: 4 attempts at the >=100M tier plus 3.5% on the sale is
        // 13.5% of the ask, against the 2.5% the old model assumed.
        let net = after_tax_inner(1_000e6, 4.0, 0.035);
        assert!((net - 1_000e6 * (1.0 - 0.10 - 0.035)).abs() < 1.0);
        assert!(
            net < after_tax_inner(1_000e6, 1.0, 0.0),
            "modelling the real fees must LOWER the net, never raise it"
        );
    }

    #[test]
    fn a_relisted_flip_stops_clearing_a_margin_it_never_had() {
        // Buy 610M against a 676M reference: +40M under the old model, a loss
        // once four listing attempts and the sale tax are paid. This is the
        // actual Ancient Crown of Avarice that lost 172,180,532.
        let (price, reference) = (610e6, 676e6);
        assert!(after_tax_inner(reference, 1.0, 0.0) - price > 0.0);
        assert!(
            after_tax_inner(reference, 4.0, 0.035) - price < 0.0,
            "four listing attempts eat the whole margin"
        );
    }

    #[test]
    fn a_mis_set_attempts_count_cannot_invert_the_net() {
        // Nonsense config degrades to a small positive net, never a negative
        // one that would make an overpay look profitable by sign flip.
        assert!(after_tax_inner(100e6, 1e9, 0.9) > 0.0);
        assert_eq!(after_tax_inner(100e6, f64::NAN, f64::NAN), 100e6 * 0.975);
        assert_eq!(
            after_tax_inner(100e6, 0.2, 0.0),
            100e6 * 0.975,
            "attempts below 1 still pays one listing fee"
        );
    }
}

#[cfg(test)]
mod undercut_reprice_tests {
    //! The undercut gate used to veto. See [`crate::config::UNDERCUT_REPRICE`].
    use super::*;

    #[test]
    fn picks_the_price_we_would_have_to_beat() {
        // Only listings BELOW our resale count, and we must beat the cheapest.
        assert_eq!(
            cheapest_competitor_below(&[12e6, 18e6, 40e6], 29.3e6),
            Some(12e6)
        );
    }

    #[test]
    fn nothing_cheaper_means_keep_our_own_estimate() {
        assert_eq!(cheapest_competitor_below(&[40e6, 55e6], 29.3e6), None);
        assert_eq!(cheapest_competitor_below(&[], 29.3e6), None);
    }

    #[test]
    fn junk_listings_cannot_drag_the_estimate_to_zero() {
        // A 0-coin or NaN listing must not become our resale: the caller would
        // then compute a nonsense margin. They are filtered, not clamped.
        assert_eq!(
            cheapest_competitor_below(&[0.0, -5.0, f64::NAN, 12e6], 29.3e6),
            Some(12e6)
        );
        assert_eq!(cheapest_competitor_below(&[0.0, f64::NAN], 29.3e6), None);
    }

    #[test]
    fn the_glacial_scythe_still_pays_after_undercutting() {
        // The real miss: ask 2.98M, resale 29,326,963, killed by reason=undercuts.
        // Even undercutting to the cheapest competitor it is a multiple of the ask,
        // which is exactly what the veto threw away.
        let price = 2_980_000.0;
        let resale = 29_326_963.0;
        let competing = [12e6, 13.5e6, 14e6, 25e6];
        let r = cheapest_competitor_below(&competing, resale).expect("someone is cheaper");
        assert_eq!(r, 12e6);
        assert!(
            price <= r * (1.0 - *MIN_MARGIN),
            "still clears the margin gate"
        );
        assert!(
            after_tax(r) - price > 8e6,
            "still worth multiples of the ask"
        );
    }
}

/// COFL parity for the fee model. These pin `cofl_after_tax`/`cofl_fee_pct`
/// against `FlipInstance.GetFeeRateForStartingBid` + `ProfitAfterFees` as
/// published, so a future edit that drifts from their arithmetic fails here
/// rather than silently on prod.
///
/// Deliberately calls the pure functions with an injected clock instead of
/// touching env: `AH_FEE_COFL` stays off for every other test and every golden.
#[cfg(test)]
mod cofl_fee_tests {
    use super::{cofl_after_tax, cofl_fee_pct, is_aura, is_derpy};

    /// A quiet day: no Aura, no Derpy. 2026-08-16T07:00:00Z.
    const QUIET_MS: f64 = 1_786_950_000_000.0;

    fn rate(reference: f64, now_ms: f64) -> f64 {
        let (l, c) = cofl_fee_pct(reference, now_ms);
        l + c
    }

    #[test]
    fn quiet_day_is_neither_aura_nor_derpy() {
        assert!(!is_aura(QUIET_MS));
        assert!(!is_derpy(QUIET_MS));
    }

    /// COFL: 2 / 3 / 3.5, with `> 10M` and `>= 100M`.
    #[test]
    fn tiers_match_cofl_exactly() {
        assert_eq!(rate(1.0, QUIET_MS), 2.0);
        assert_eq!(
            rate(10_000_000.0, QUIET_MS),
            2.0,
            "10M exactly is the LOW tier (COFL uses >)"
        );
        assert_eq!(rate(10_000_001.0, QUIET_MS), 3.0);
        assert_eq!(rate(99_999_999.0, QUIET_MS), 3.0);
        assert_eq!(
            rate(100_000_000.0, QUIET_MS),
            3.5,
            "100M exactly is the TOP tier (COFL uses >=)"
        );
        assert_eq!(rate(1_000_000_000.0, QUIET_MS), 3.5);
    }

    /// `ProfitAfterFees(target, cost) = target * (100 - reduction) / 100 - cost`
    /// at one attempt. Same three worked numbers as the legacy test above.
    #[test]
    fn one_attempt_is_cofl_profit_after_fees() {
        assert_eq!(cofl_after_tax(200e6, 1.0, QUIET_MS), 200e6 * 0.965);
        assert_eq!(cofl_after_tax(50e6, 1.0, QUIET_MS), 50e6 * 0.970);
        assert_eq!(cofl_after_tax(5e6, 1.0, QUIET_MS), 5e6 * 0.980);
    }

    /// The claim tax is paid ONCE however many times we relist. Only the listing
    /// half scales, which is the whole point of keeping `attempts` around.
    #[test]
    fn attempts_scale_the_listing_half_only() {
        // >=100M: listing 2.5, claim 1.0. Three attempts = 7.5 + 1.0 = 8.5%.
        let net = cofl_after_tax(1_000e6, 3.0, QUIET_MS);
        assert!((net - 1_000e6 * 0.915).abs() < 1.0, "got {net:.0}");
        // Attempts below 1 cannot buy a discount.
        assert_eq!(
            cofl_after_tax(1_000e6, 0.1, QUIET_MS),
            cofl_after_tax(1_000e6, 1.0, QUIET_MS)
        );
        assert_eq!(
            cofl_after_tax(1_000e6, f64::NAN, QUIET_MS),
            cofl_after_tax(1_000e6, 1.0, QUIET_MS)
        );
    }

    /// The measured 603,900,000 sale billed 21,137,700 = 3.5002%. COFL's >=100M
    /// reduction reproduces it to within rounding, which is the evidence that
    /// `AH_SALE_TAX=0.035` was the WHOLE fee and not a second charge.
    #[test]
    fn reproduces_the_measured_603m_sale() {
        let billed = 603_900_000.0 - cofl_after_tax(603_900_000.0, 1.0, QUIET_MS);
        assert!(
            (billed - 21_137_700.0).abs() < 25_000.0,
            "expected ~21,137,700 billed, got {billed:.0}"
        );
    }

    /// Derpy quadruples the claiming tax (1% -> 4%, i.e. COFL's `+3`), and only
    /// at or above 1M. 2026-09-09T12:00:00Z is inside COFL's Derpy window.
    #[test]
    fn derpy_quadruples_the_claim_tax_above_1m() {
        const DERPY_MS: f64 = 1_789_041_600_000.0;
        assert!(
            is_derpy(DERPY_MS),
            "2026-09-09 should be inside a Derpy term"
        );
        assert_eq!(rate(500_000.0, DERPY_MS), 2.0, "under 1M is exempt");
        assert_eq!(rate(1_000_000.0, DERPY_MS), 5.0, "1% claim -> 4%");
        assert_eq!(rate(200_000_000.0, DERPY_MS), 6.5, "2.5 listing + 4 claim");
    }

    /// COFL `return`s inside the Aura branch, so Aura never stacks with Derpy.
    #[test]
    fn aura_adds_one_percent_and_does_not_stack() {
        const AURA_MS: f64 = 1_765_000_000_000.0; // 2025-12-06, inside the window
        assert!(is_aura(AURA_MS));
        assert_eq!(rate(5e6, AURA_MS), 3.0);
        assert_eq!(rate(200e6, AURA_MS), 4.5);
    }

    /// The number this whole change is about: what the deployed gate charges vs
    /// what Hypixel actually takes, at the top tier.
    #[test]
    fn old_model_overcharged_the_top_tier_by_about_6_points() {
        let old = super::after_tax_inner(1_000e6, 2.41, 0.035); // tier*2.41 + 3.5%
        let cofl = cofl_after_tax(1_000e6, 1.0, QUIET_MS); // 3.5%
        let gap = (cofl - old) / 1_000e6;
        assert!(
            (gap - 0.06025).abs() < 1e-6,
            "expected a 6.025 point gap at >=100M, got {:.4}",
            gap * 100.0
        );
    }
}
