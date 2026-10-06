//! Bazaar finder: decides WHICH product to make a market in, at WHAT price, for
//! HOW MANY units, and when to give up on an order that isn't filling.
//!
//! Reads the collector's history ([`crate::bazaar_collect`]) plus the live
//! snapshot; emits COFL-shaped `bazaarFlip` recommendations that the mod's
//! existing `BazaarFlipHandler` already knows how to execute. Deliberately
//! isolated from the auction finder in exactly the way the collector is: own
//! thread, own db handle, own side-channel HTTP client, no shared state with the
//! flip path, so it cannot perturb detection latency or golden parity.
//!
//! # What the model is built on
//!
//! Three measurements over 25 days of stored order books (71.2M snapshots) and
//! 15,778 real bazaar chat events from our own bots decided the shape of this:
//!
//! 1. **Spread is not opportunity.** SPIKED_BAIT showed a 128% net spread that
//!    persisted, unchanged, for the full 25 days. A spread nobody closes for
//!    25 days is not a spread anybody CAN close; ranking on margin alone puts
//!    that class of book at the top of the list. MELON prices "3.6 ask / 0.2
//!    bid" for the same reason: two lowball buy orders are the whole bid side.
//!    Hence [`Tuning::max_ask_bid_ratio`] and the stability gate below.
//!
//! 2. **Fills are the binding constraint, and they are slow.** Our own
//!    top-of-book orders: buy orders filled 55% of the time, median 12.9 min,
//!    p90 25.3 HOURS. Sell offers filled 32%, median 106.9 min, p90 40.7 hours.
//!    So a round trip is ~2h at the median and the tail is measured in days,
//!    which is why everything here is ranked per SLOT-HOUR rather than per unit
//!    or per trade: slots, not coins, are what we run out of (the bots hit
//!    "you reached your maximum of N Bazaar orders" 286 times).
//!
//! 3. **The queue ahead of us is invisible and dominates.** Predicting fill
//!    time as `order_size / traded_flow` has a Spearman correlation of +0.09
//!    with what actually happened (n=510). Orders that model says fill in under
//!    a minute really took 14 min at the median and 25h at p90. So this module
//!    does NOT pretend to compute a fill time from flow. It uses the measured
//!    population priors as the default, prefers products whose top of book is
//!    quiet enough that we keep our place in the queue, and learns the real
//!    per-product fill time from our own outcomes as they arrive.
//!
//! # Enabling
//!
//! Off unless `BZ_FINDER=1`. `BZ_FINDER_DRY=1` runs the whole loop and logs
//! every decision without emitting anything to the bots, which is how this
//! should be watched for a day before it is allowed to trade.

use crate::bazaar_ledger::{BazaarLedger, Source};
use crate::hypixel::{self, BazaarProduct};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Per-order unit cap enforced by the bazaar GUI.
const MAX_UNITS_PER_ORDER: f64 = 71_680.0;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn env_f64(k: &str, d: f64) -> f64 {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

fn env_i64(k: &str, d: i64) -> i64 {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

fn env_on(k: &str) -> bool {
    std::env::var(k).as_deref() == Ok("1")
}

/// Explicitly disabled. Absent means the default stays in force, so a safety
/// gate has to be switched OFF on purpose rather than by forgetting to set it.
fn env_off(k: &str) -> bool {
    std::env::var(k).as_deref() == Ok("0")
}

/// Read the do-not-trade list from `BZ_FILTER_FILE`.
///
/// Deliberately reads the *other* flipper's file shape verbatim
/// (`{"blacklist":[...], "whitelist":{...}}`) so their curated list drops in
/// unedited and can be re-copied whenever they update it. Only `blacklist` is
/// consumed; the rest of the file is ignored rather than rejected, so a file
/// carrying their unrelated keys still loads. A missing or unreadable file is
/// not an error: it just means no blacklist, which is the pre-existing
/// behaviour.
fn load_blacklist() -> std::collections::HashSet<String> {
    let Ok(path) = std::env::var("BZ_FILTER_FILE") else {
        return Default::default();
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("bazaar-finder: blacklist {path} unreadable: {e}");
            return Default::default();
        }
    };
    #[derive(Deserialize)]
    struct F {
        #[serde(default)]
        blacklist: Vec<String>,
    }
    match serde_json::from_str::<F>(&raw) {
        Ok(f) => {
            let set: std::collections::HashSet<String> = f.blacklist.into_iter().collect();
            eprintln!(
                "bazaar-finder: blacklist {} products from {path}",
                set.len()
            );
            set
        }
        Err(e) => {
            eprintln!("bazaar-finder: blacklist {path} is not valid JSON: {e}");
            Default::default()
        }
    }
}

/// Next price strictly above `p`. Hypixel quotes the bazaar in 0.1 steps and
/// rejects an order that does not beat the current best ("your price isn't
/// competitive enough" — 939 of those in our own logs), so beating it by
/// exactly one step is both necessary and the cheapest way to do it.
fn tick_up(p: f64) -> f64 {
    ((p * 10.0).floor() + 1.0) / 10.0
}

/// Next price strictly below `p`, same 0.1 step.
fn tick_down(p: f64) -> f64 {
    ((p * 10.0).ceil() - 1.0) / 10.0
}

/// Does one unit of this product occupy one whole inventory slot?
///
/// ⚠️ Mirrors `is_unstackable_item` in the mod (`src/utils/string.rs`) EXACTLY,
/// because the mod is what actually places the order: for an unstackable buy it
/// caps the amount to `empty_slots - 2` and skips the order entirely when that
/// is zero (`main.rs`). Sizing an order the mod will silently cut to a fraction
/// inflates its coins-per-slot-hour by exactly that fraction, and this covers
/// **773 of 2124 bazaar products** — every `ENCHANTMENT_*` book — so getting it
/// wrong systematically over-ranks a third of the market.
///
/// ⛔ Hypixel's own `unstackable` field is NOT usable here: it flags only 68
/// bazaar products and **none** of the enchantment books, and 1106 products
/// carry no material or flag at all. Keep this in step with the mod instead.
fn is_unstackable(tag: &str, name: &str) -> bool {
    tag.starts_with("ENCHANTMENT_") || name.to_lowercase().contains("enchanted book")
}

/// Walk a side of the resting book until `need` units have accumulated, and
/// return the price of the level that got us there.
///
/// This is the live answer to "is the top of book a real price, or one thin
/// order?". Hot Potato Book once showed an ask of 85,379 against a 24h median
/// implying ~38,500: the cheap sell offers had been bought out and the top was
/// a gap, so a sell order of ours priced off it would still be sitting there
/// when the level refilled underneath. The old gate inferred that by comparing
/// the live margin to a 24h median, i.e. it used history to judge the present.
/// The book itself says it directly and immediately: accumulate the resting
/// units and see where the supply actually is.
///
/// `levels` must be ordered outward from the top of book (asks ascending, bids
/// descending), which is how Hypixel sends them. Returns `None` when the book
/// is empty, or when it is too shallow to cover `need` — a book that cannot
/// absorb our own order size has no reliable price for it, and the caller
/// treats that as a refusal rather than guessing.
fn price_at_depth(levels: &[(f64, i64)], need: f64) -> Option<f64> {
    if levels.is_empty() || need <= 0.0 {
        return None;
    }
    let mut cum = 0.0;
    for &(price, amount) in levels {
        cum += amount.max(0) as f64;
        if cum >= need {
            return Some(price);
        }
    }
    None
}

/// Units resting at prices STRICTLY BETTER than `ours` on our own side of the
/// book, i.e. the queue that must clear before our order is reached.
///
/// `levels` is `bid_levels` for a buy order (buy orders, highest first). The
/// scan stops at the first level that is not better than ours, which is correct
/// for a sorted book and cheap.
fn units_ahead_of(levels: &[(f64, i64)], ours: f64) -> f64 {
    let mut n = 0.0;
    for &(price, amount) in levels {
        if price <= ours {
            break;
        }
        n += amount.max(0) as f64;
    }
    n
}

/// Should an outbid buy order be KEPT rather than cancelled?
///
/// Pure and config-free so the decision is directly testable. `keep_hours <= 0`
/// returns false, i.e. the old cancel-on-any-outbid behaviour, byte-identical.
///
/// The test is whether the queue ahead of us is small RELATIVE to how fast this
/// book trades. `hourly_flow` comes from `sell_moving_week / (7 * 24)`.
///
/// ⚠️ **This is a screen, not a fill-time prediction.** Measured over 510 real
/// orders, `order_size / traded_flow` correlates with realised fill time at
/// Spearman **+0.09** ([[finder-bazaar-fill-truth]]) — the queue's TIME priority
/// is invisible and dominates. What this can honestly say is the one-sided
/// version: a wall many hours of flow deep is a book we are not going to win,
/// and that much is robust even though the converse is not. Keep the threshold
/// conservative and never read the number as "we will fill in N hours".
fn keep_when_outbid(
    units_ahead: f64,
    hourly_flow: f64,
    our_price: f64,
    best_price: f64,
    keep_hours: f64,
    stale_gap: f64,
) -> bool {
    if keep_hours <= 0.0 {
        return false;
    }
    if our_price <= 0.0 || best_price <= our_price {
        return false;
    }
    // The market moved, not a queue-jump: our price is stale, re-price instead.
    if (best_price - our_price) / our_price > stale_gap {
        return false;
    }
    // No flow means the queue never drains, whatever its size.
    if hourly_flow <= 0.0 {
        return false;
    }
    units_ahead / hourly_flow <= keep_hours
}

// ─────────────────────────────────────────────────────────────────────────────
// Tuning
// ─────────────────────────────────────────────────────────────────────────────

/// Every gate and prior, all env-overridable so a bad default can be corrected
/// on the running process without a rebuild. Defaults are the measured values
/// documented in the module header, not guesses.
#[derive(Clone, Debug)]
pub struct Tuning {
    /// Bazaar sale tax. 1.25% base; a bot with the Bazaar Flipper perk pays
    /// less, so this is deliberately the pessimistic end.
    pub tax: f64,
    /// Minimum net margin after tax, percent of the buy price.
    pub min_margin_pct: f64,
    /// Fraction of recent snapshots that must show a positive net spread.
    /// Measured separator: healthy books sit at 97-100%, the over-farmed races
    /// (BOOSTER_COOKIE 81.9%, RECOMBOBULATOR_3000 85.9%) fall below 90%.
    pub min_stability: f64,
    /// Units/day that must move on BOTH sides. A one-sided book cannot be
    /// round-tripped no matter how wide it looks.
    pub min_flow_per_day: f64,
    /// Reject books whose top of book is re-priced more often than this. The
    /// measured range is 5-44 improvements/hour; the top of that range is
    /// BOOSTER_COOKIE and RECOMBOBULATOR_3000, where the margin is 0.2-0.4% and
    /// we would spend every slot-hour being outbid.
    pub max_undercut_per_hour: f64,
    /// Reject books wider than this ratio (ask/bid). A 2.3x book that has been
    /// 2.3x for 25 days is a book with no traffic at the top, not free money.
    pub max_ask_bid_ratio: f64,
    /// Floor on how much resting supply must back the price we plan to sell
    /// into, in units, when our own order is smaller than that.
    ///
    /// Guards the degenerate case: a 1-unit order would otherwise be "backed"
    /// by a 1-unit ask, which is precisely the thin top that cannot be relied
    /// on. Applied as `max(our_units, this)`.
    pub min_depth_units: f64,
    /// Inventory slots left free for AH and sell operations when sizing a buy.
    /// ⚠️ Must match the mod's reserve (currently 2) or we size orders it will
    /// cut.
    pub unstackable_reserve: i64,
    /// Units of a STACKABLE product per inventory slot. Everything on the
    /// bazaar that is not a book stacks to 64.
    pub units_per_slot: f64,
    /// Below this unit price the 0.1 tick is a large fraction of the margin.
    pub min_unit_price: f64,
    /// Coins we are willing to have resting in one order.
    pub max_order_coins: f64,
    /// Never order more than this share of one day's flow.
    pub max_flow_share: f64,
    /// Bazaar order slots per account (14 stock; the mod's "maximum orders"
    /// chat message is the real backstop).
    pub slots_per_bot: i64,
    /// Fraction of a bot's purse the bazaar is allowed to tie up. The auction
    /// finder is the primary earner and must not be starved of buying power.
    pub purse_share: f64,
    /// Cancel and re-place a buy order that has been outbid and has sat this
    /// long without filling.
    pub reprice_after_min: f64,
    /// Give up on a position entirely after this long (log it, stop repricing).
    pub abandon_after_min: f64,
    /// Only trade on bots we have heard chat from. ⛔ Turning this off means
    /// placing orders we cannot confirm, which is how the 2026-08-14 runaway
    /// happened. Off only makes sense if every bot is pointed at the finder.
    pub require_chat: bool,
    /// Drop a position Hypixel has not confirmed within this many minutes.
    ///
    /// A placement that works is acknowledged in **600 ms** end to end -- the
    /// mod runs `/bz <name>` -> click item -> Create Buy Order -> amount sign ->
    /// price sign -> confirm -> `Buy Order Setup!` inside one second (measured
    /// 2026-08-15). One that fails dies at the mod's own 5-second GUI watchdog
    /// (`Window N open for >5 s in state Bazaar -- auto-closing`) and is then
    /// marked "Completed" with nothing ordered. So the outcome is known within
    /// ~6 seconds and everything past that is a slot held against nothing.
    pub confirm_timeout_min: f64,
    /// How long to bar a product after a PHANTOM before trying it again.
    ///
    /// ⛔ Deliberately NOT `rebuy_cooldown_min`. A phantom means no order, no
    /// escrow and no coins moved, so re-sending cannot duplicate anything --
    /// whereas a 30-minute ban on 74% of attempts is precisely why the finder
    /// placed 27 orders in a day instead of a hundred.
    pub phantom_retry_min: f64,
    /// Consecutive phantoms on one product before it falls back to the full
    /// `rebuy_cooldown_min`. Retrying is right when placement is flaky; it is
    /// wrong when the item can never be placed (a name `/bz` cannot find, or a
    /// price the server always refuses), and this is what tells them apart.
    pub phantom_max_strikes: i64,
    /// Sell bazaar goods a bot is holding that no position accounts for.
    ///
    /// 🔥 Without this the finder cannot recover from its own past mistakes, and
    /// on 2026-08-15 that was fatal rather than untidy. Every orphaned buy sat
    /// in the inventory forever, and once a bot drops to ≤4 free slots the mod
    /// silently skips EVERY buy order at `is_inventory_near_full()` -- a
    /// `debug!` that is invisible at INFO. Four of four trading bots were at
    /// 0-4 free slots holding Tasty Cheese x1072, Hunk of Ice x695, Enchanted
    /// Bone x960 and the rest, so 100% of orders were being dropped before they
    /// were ever sent to the game.
    pub orphan_sweep: bool,
    /// Ignore orphans worth less than this. Cheap dust is not worth an order
    /// slot, and a sell offer per stray item would be its own kind of spam.
    pub orphan_min_coins: f64,
    /// Ignore orphan stacks smaller than this. Bot equipment and consumables
    /// arrive as x1, bazaar stock arrives in hundreds, and this is the cheapest
    /// line between them.
    pub orphan_min_units: f64,
    /// Tags never to sweep, whatever the inventory says. Bazaar products the
    /// bot itself consumes: selling a bot's cookie is a real loss.
    pub orphan_exclude: std::collections::HashSet<String>,
    /// Ignore a bot for this long after chat shows it in transit.
    ///
    /// A bot that is warping, mid-startup or sitting in a lobby accepts the
    /// order, opens the bazaar, clicks the item -- and the next window never
    /// arrives because the server swap invalidated it. The click is silently
    /// eaten and the order is lost. Observed on `Gloomgourd x569`, 2026-08-15.
    pub transit_quiet_sec: f64,
    /// Refuse to re-buy a product for this long after closing a position in it.
    /// Bounds the blast radius of a bad close: without it, one wrong close
    /// re-buys immediately and the loop compounds into duplicate orders.
    pub rebuy_cooldown_min: f64,
    /// How long a sell offer must rest before it is treated as filled.
    ///
    /// ⚠️ This is an ASSUMPTION, not an observation, and it is here only because
    /// the finder has no fill feed. Our own measured sell fills are p50 107min
    /// and p90 40h, so the default deliberately sits past the median: closing
    /// early frees a bazaar slot the resting offer is still occupying, which is
    /// the more expensive mistake. Replace this the moment the mod uploads its
    /// open bazaar orders.
    pub sell_settle_min: f64,
    /// Minutes a sell offer may rest BEHIND the book before it is cancelled and
    /// re-quoted at the front. 0 = off.
    ///
    /// Measured on prod 2026-08-16: **25 of 25** resting sell offers were behind
    /// the current best ask, several by 40-70% (ICE_HUNK asking 4,384 into a book
    /// whose best ask was 2,588). We quote once at `tick_down(top_insta_buy)` and
    /// then never touch it again — `reprice_after_min` only governs BUY orders
    /// (`stale_buys` returns early on `sell_issued_at_ms.is_some()`). So the book
    /// moves, we are left stranded above it, and the offer cannot fill at any
    /// point in the next `sell_settle_min` (180) minutes. That is why 23 of 34
    /// open positions were bought-and-unsold with `units_sold = 0`, and it is the
    /// binding constraint on bazaar throughput.
    /// Sell a position from whichever bot PHYSICALLY holds the units, rather
    /// than from the bot we happened to ask to buy them. Off by default.
    pub sell_from_holder: bool,
    /// Hours of flow that may rest AHEAD of our outbid buy order before we give
    /// up on it. 0 = off, i.e. cancel on any outbid, the old behaviour.
    ///
    /// Being outbid is not one situation. Someone jumping us by 0.1 with a
    /// 20-unit order on a book that trades 40,000 units an hour is noise: the
    /// queue in front drains in seconds and our order still fills. Someone
    /// parking a 70,000-unit wall above us on a thin book means we will never
    /// fill and the slot is dead. The old rule — `top_insta_sell > buy_price`
    /// after `reprice_after_min` — cannot tell those apart and cancels both.
    pub outbid_keep_hours: f64,
    /// Relative price gap above which an outbid is treated as the market moving
    /// rather than a queue-jump, and the order is cancelled regardless of depth.
    /// Our price is simply stale at that point.
    pub outbid_stale_gap: f64,
    pub sell_requote_min: f64,
    /// How far above the front of book an offer must sit to be worth the
    /// cancel/replace round trip, as a fraction. Guards against churning on a
    /// one-tick difference.
    pub sell_requote_gap: f64,
    /// Cap on cancel/replace cycles per position, so a book we are permanently
    /// losing cannot be chased forever.
    pub sell_max_requotes: i64,
    /// Prior fill times, hours, from our own measured outcomes.
    pub prior_buy_fill_h: f64,
    pub prior_sell_fill_h: f64,
    /// Prior fill probabilities at top of book, also measured.
    pub prior_buy_fill_p: f64,
    pub prior_sell_fill_p: f64,
    /// History window for the per-product statistics.
    pub stats_hours: i64,
    /// How often to recompute those statistics.
    pub stats_refresh_min: i64,
    /// Loop cadence.
    pub interval_ms: u64,
    /// Log decisions, emit nothing.
    pub dry_run: bool,
    /// Manipulation guard: refuse a product whose live bid or ask has moved more
    /// than this many percent away from its own median over the last
    /// `manip_window_min` minutes, and keep refusing it for
    /// `manip_cooldown_min` afterwards.
    ///
    /// The cooldown is the load-bearing half and the reason this is not just
    /// another stateless gate. Every other gate here is recomputed from scratch
    /// each pass, so during a pump the product flickers in and out of range and
    /// we would eventually place an order on one of the passes where it looks
    /// fine. Quarantining the product for a fixed period after the FIRST
    /// anomalous reading means one bad print takes it off the table until the
    /// event is demonstrably over. Defaults (10%, 15min) are taken from a
    /// working flipper's config; the window is ours.
    ///
    /// This covers what `max_margin_over_median` cannot: that gate compares the
    /// SPREAD to its median, so a pump that carries both sides up together (the
    /// shape that actually traps a buyer) passes it untouched.
    pub manip_trigger_pct: f64,
    pub manip_window_min: i64,
    pub manip_cooldown_min: i64,
    /// Product tags we refuse to trade at any price.
    ///
    /// This is not a taste list, it is a fill-rate list. Loaded from a working
    /// bazaar flipper's own curated blacklist (`BZ_FILTER_FILE`, their JSON
    /// shape: `{"blacklist": ["TAG", ...]}`), then checked against 15,778 of our
    /// OWN bazaar chat events: orders on their blacklist filled 45% (buy) and
    /// 32% (sell); everything else filled 68% and 49%. Individual entries are
    /// worse than the average suggests -- `FLAWED_ONYX_GEM` filled 0 of 138
    /// placements, `FEL_PEARL` 1 of 65. Those orders each held a slot and
    /// returned nothing, which is precisely the cost coins-per-slot-hour exists
    /// to price and which none of the book-shape gates above can see.
    pub blacklist: std::collections::HashSet<String>,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            tax: env_f64("BZ_TAX", 0.0125),
            min_margin_pct: env_f64("BZ_MIN_MARGIN_PCT", 3.0),
            min_stability: env_f64("BZ_MIN_STABILITY", 0.90),
            min_flow_per_day: env_f64("BZ_MIN_FLOW_PER_DAY", 2000.0),
            max_undercut_per_hour: env_f64("BZ_MAX_UNDERCUT_PER_HOUR", 30.0),
            max_ask_bid_ratio: env_f64("BZ_MAX_ASK_BID_RATIO", 1.6),
            min_depth_units: env_f64("BZ_MIN_DEPTH_UNITS", 64.0),
            unstackable_reserve: env_i64("BZ_UNSTACKABLE_RESERVE", 2),
            units_per_slot: env_f64("BZ_UNITS_PER_SLOT", 64.0),
            min_unit_price: env_f64("BZ_MIN_UNIT_PRICE", 50.0),
            max_order_coins: env_f64("BZ_MAX_ORDER_COINS", 20_000_000.0),
            max_flow_share: env_f64("BZ_MAX_FLOW_SHARE", 0.02),
            slots_per_bot: env_i64("BZ_SLOTS_PER_BOT", 14),
            purse_share: env_f64("BZ_PURSE_SHARE", 0.25),
            reprice_after_min: env_f64("BZ_REPRICE_AFTER_MIN", 20.0),
            abandon_after_min: env_f64("BZ_ABANDON_AFTER_MIN", 1440.0),
            require_chat: !env_off("BZ_REQUIRE_CHAT"),
            confirm_timeout_min: env_f64("BZ_CONFIRM_TIMEOUT_MIN", 1.0),
            orphan_sweep: !env_off("BZ_ORPHAN_SWEEP"),
            orphan_min_coins: env_f64("BZ_ORPHAN_MIN_COINS", 25_000.0),
            orphan_min_units: env_f64("BZ_ORPHAN_MIN_UNITS", 4.0),
            orphan_exclude: std::env::var("BZ_ORPHAN_EXCLUDE")
                .unwrap_or_else(|_| "BOOSTER_COOKIE,GOD_POTION,GOD_POTION_2".to_string())
                .split(',')
                .map(|s| s.trim().to_uppercase())
                .filter(|s| !s.is_empty())
                .collect(),
            phantom_retry_min: env_f64("BZ_PHANTOM_RETRY_MIN", 2.0),
            phantom_max_strikes: env_f64("BZ_PHANTOM_MAX_STRIKES", 3.0) as i64,
            transit_quiet_sec: env_f64("BZ_TRANSIT_QUIET_SEC", 90.0),
            rebuy_cooldown_min: env_f64("BZ_REBUY_COOLDOWN_MIN", 30.0),
            sell_settle_min: env_f64("BZ_SELL_SETTLE_MIN", 180.0),
            sell_from_holder: env_on("BZ_SELL_FROM_HOLDER"),
            outbid_keep_hours: env_f64("BZ_OUTBID_KEEP_HOURS", 0.0),
            outbid_stale_gap: env_f64("BZ_OUTBID_STALE_GAP", 0.02),
            sell_requote_min: env_f64("BZ_SELL_REQUOTE_MIN", 0.0),
            sell_requote_gap: env_f64("BZ_SELL_REQUOTE_GAP", 0.01),
            sell_max_requotes: env_i64("BZ_SELL_MAX_REQUOTES", 4),
            prior_buy_fill_h: env_f64("BZ_PRIOR_BUY_FILL_H", 0.215),
            prior_sell_fill_h: env_f64("BZ_PRIOR_SELL_FILL_H", 1.78),
            prior_buy_fill_p: env_f64("BZ_PRIOR_BUY_FILL_P", 0.55),
            prior_sell_fill_p: env_f64("BZ_PRIOR_SELL_FILL_P", 0.32),
            stats_hours: env_i64("BZ_STATS_HOURS", 24),
            stats_refresh_min: env_i64("BZ_STATS_REFRESH_MIN", 30),
            interval_ms: env_i64("BZ_INTERVAL_MS", 60_000).max(5_000) as u64,
            dry_run: env_on("BZ_FINDER_DRY"),
            manip_trigger_pct: env_f64("BZ_MANIP_TRIGGER_PCT", 10.0),
            manip_window_min: env_i64("BZ_MANIP_WINDOW_MIN", 30),
            manip_cooldown_min: env_i64("BZ_MANIP_COOLDOWN_MIN", 15),
            blacklist: load_blacklist(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-product statistics from stored history
// ─────────────────────────────────────────────────────────────────────────────

/// What the last [`Tuning::stats_hours`] of stored books say about one product.
#[derive(Clone, Debug, Default)]
pub struct ProductStats {
    pub samples: i64,
    /// Share of samples whose net spread (after tax) was positive.
    pub stability: f64,
    /// Median net margin over the window, percent of bid.
    pub median_margin_pct: f64,
    /// Median ask/bid ratio: the trap detector.
    pub median_ask_bid_ratio: f64,
    /// Improvements to the best buy order per hour = how fast we get outbid.
    pub undercut_per_hour: f64,
    /// Median absolute 1h move of the mid, percent. Drift risk while we hold.
    pub vol_1h_pct: f64,
    /// Median bid/ask over a SHORT recent window (`manip_window_min`), the
    /// anchor the manipulation guard measures the live book against. NaN when
    /// the window held no usable sample, which the guard treats as "no opinion"
    /// rather than as a trigger.
    pub recent_bid_median: f64,
    pub recent_ask_median: f64,
}

fn median(v: &mut Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

/// Stream the window ordered by (product, ts) — which is the index order, so
/// this is a range scan per product rather than a sort — and fold each product's
/// series down to a [`ProductStats`] before moving on to the next one.
/// One product at a time, driven off the `(product, ts)` index so each query is
/// a range scan of just that product's window.
///
/// The obvious `WHERE ts >= ? ORDER BY product, ts` over the whole table instead
/// takes **260 seconds** against the live 5.7GB collection, because `ts` is not
/// the index prefix and sqlite ends up walking everything. That much disk churn
/// every refresh, on the box that runs the auction finder, is not a cost this
/// module is allowed to impose.
pub fn compute_stats(
    conn: &Connection,
    hours: i64,
    tax: f64,
    manip_window_min: i64,
) -> rusqlite::Result<HashMap<String, ProductStats>> {
    let cutoff = now_ms() - hours * 3_600_000;
    let recent_cutoff = now_ms() - manip_window_min.max(1) * 60_000;
    let products: Vec<(i64, String)> = {
        let mut st = conn.prepare("SELECT id, tag FROM bz_product")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        rows.flatten().collect()
    };
    let mut st = conn.prepare(
        "SELECT ts, top_insta_buy, top_insta_sell FROM bz_snapshot
         WHERE product = ?1 AND ts >= ?2 ORDER BY ts",
    )?;

    let mut out: HashMap<String, ProductStats> = HashMap::new();
    for (pid, tag) in products {
        let mut margins: Vec<f64> = Vec::new();
        let mut ratios: Vec<f64> = Vec::new();
        let mut mids: Vec<f64> = Vec::new();
        let mut recent_bids: Vec<f64> = Vec::new();
        let mut recent_asks: Vec<f64> = Vec::new();
        let (mut pos, mut n, mut ups) = (0i64, 0i64, 0i64);
        let mut prev_bid = f64::NAN;
        let (mut t_first, mut t_last) = (0i64, 0i64);

        let mut rows = st.query(rusqlite::params![pid, cutoff])?;
        while let Some(r) = rows.next()? {
            let ts: i64 = r.get(0)?;
            let ask: f64 = r.get(1)?;
            let bid: f64 = r.get(2)?;
            if t_first == 0 {
                t_first = ts;
            }
            t_last = ts;
            if ask <= 0.0 || bid <= 0.0 {
                continue;
            }
            n += 1;
            if ask * (1.0 - tax) - bid > 0.0 {
                pos += 1;
            }
            margins.push((ask * (1.0 - tax) - bid) / bid * 100.0);
            ratios.push(ask / bid);
            mids.push((ask + bid) / 2.0);
            if ts >= recent_cutoff {
                recent_bids.push(bid);
                recent_asks.push(ask);
            }
            if prev_bid.is_finite() && bid > prev_bid {
                ups += 1;
            }
            prev_bid = bid;
        }
        if n == 0 {
            continue;
        }
        // 60 samples ~ 1h at the collector's 60s cadence.
        let mut vol: Vec<f64> = Vec::new();
        let mut i = 60usize;
        while i < mids.len() {
            if mids[i - 60] > 0.0 {
                vol.push(((mids[i] / mids[i - 60]) - 1.0).abs() * 100.0);
            }
            i += 10;
        }
        let span_h = ((t_last - t_first) as f64 / 3_600_000.0).max(1.0 / 60.0);
        out.insert(
            tag,
            ProductStats {
                samples: n,
                stability: pos as f64 / n as f64,
                median_margin_pct: median(&mut margins),
                median_ask_bid_ratio: median(&mut ratios),
                undercut_per_hour: ups as f64 / span_h,
                vol_1h_pct: if vol.is_empty() {
                    0.0
                } else {
                    median(&mut vol)
                },
                recent_bid_median: median(&mut recent_bids),
                recent_ask_median: median(&mut recent_asks),
            },
        );
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────────────
// Plans
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

/// One order we want a bot to place.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub tag: String,
    pub name: String,
    pub side: Side,
    pub unit_price: f64,
    pub units: f64,
    /// Net coins per unit if the round trip completes, after tax.
    pub net_unit: f64,
    pub margin_pct: f64,
    /// Expected coins per slot-hour: the ranking quantity.
    pub score: f64,
}

impl Plan {
    /// The exact envelope the mod already parses. Its websocket client matches
    /// `bazaarFlip` before falling through to the finder-native `{type:"flip"}`
    /// shape, and `WebSocketMessage.data` is a STRING, so the payload is
    /// serialised twice on purpose. `itemName` is the display name because the
    /// bot types it into the bazaar search box.
    /// ⛔ `itemTag` is deliberately OMITTED, and sending it breaks the order.
    ///
    /// The mod picks its bazaar search term as "itemTag when available, else the
    /// title-cased itemName" (`bot/client.rs:3808`), on the comment that a tag
    /// "skips search results page". It does not. Hypixel's bazaar search does
    /// not match raw tags, so `/bz BLESSED_BAIT` lands on an EMPTY results grid,
    /// `find_slot_by_name` misses, and the bot logs "not found in search
    /// results; going idle" and silently drops the order. Measured 2026-08-14:
    /// with the tag, 17 of 17 orders died there and not one reached the book.
    /// COFL's own bazaar recommendations carry `itemTag: null` and run
    /// `/bz Scarf Fragment`, which finds the item every time.
    ///
    /// So this payload is deliberately shaped like COFL's, and `itemName` must
    /// stay the exact display name for both the search and the slot match.
    pub fn to_wire(&self) -> String {
        let inner = serde_json::json!({
            "itemName": self.name,
            "amount": self.units.round() as i64,
            "pricePerUnit": (self.unit_price * 10.0).round() / 10.0,
            "totalPrice": (self.unit_price * self.units * 10.0).round() / 10.0,
            "isBuyOrder": self.side == Side::Buy,
            "isSell": self.side == Side::Sell,
        });
        serde_json::json!({ "type": "bazaarFlip", "data": inner.to_string() }).to_string()
    }

    /// Cancel the same order. Same payload shape; the mod routes `cancelOrder`
    /// to its targeted-cancel path.
    pub fn to_cancel_wire(&self) -> String {
        let inner = serde_json::json!({
            "itemName": self.name,
            "itemTag": self.tag,
            "amount": self.units.round() as i64,
            "pricePerUnit": (self.unit_price * 10.0).round() / 10.0,
            "isBuyOrder": self.side == Side::Buy,
            "isSell": self.side == Side::Sell,
        });
        serde_json::json!({ "type": "cancelOrder", "data": inner.to_string() }).to_string()
    }
}

/// A buy order we have issued and not yet closed out.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Position {
    pub tag: String,
    pub name: String,
    pub units: f64,
    pub buy_price: f64,
    pub placed_at_ms: i64,
    /// Set once we have asked a bot to sell it.
    #[serde(default)]
    pub sell_issued_at_ms: Option<i64>,
    #[serde(default)]
    pub sell_price: Option<f64>,
    /// Which bot was asked to do this.
    #[serde(default)]
    pub bot: String,
    #[serde(default)]
    pub repriced: i64,
    /// Units of this tag the bot ALREADY held when the buy order went in.
    ///
    /// The inventory upload reports a total per tag and cannot say where the
    /// units came from, so without this a single stray unit from AH flipping
    /// reads as our fill. Measured 2026-08-14: one incidental Hunk of Ice made
    /// the finder sell x1 and close a 1,083-unit position as fully sold, while
    /// the real 1,999,110-coin buy order was still resting on the book.
    #[serde(default)]
    pub baseline_units: f64,
    /// Units currently sitting in a sell offer, and the running total we have
    /// actually managed to offer. A buy order fills in pieces, so the position
    /// is only finished once the pieces add up.
    #[serde(default)]
    pub units_in_offer: f64,
    #[serde(default)]
    pub units_sold: f64,
    /// When Hypixel's own chat confirmed the buy order reached the book. Until
    /// this is set the position is only a hope: the mod may have dropped it, or
    /// the server may have refused it as not competitive.
    #[serde(default)]
    pub confirmed_at_ms: Option<i64>,
    /// Units Hypixel has told us our BUY order actually filled.
    ///
    /// ⚠️ This is what we sell against, NOT the inventory count. The bots
    /// already hold bazaar stock from other activity, so "the bot has 200 Jelly"
    /// says nothing about whether our order filled. Inventory is still consulted
    /// as an upper bound, because unclaimed units cannot be offered.
    #[serde(default)]
    pub units_filled: f64,
    /// Adopted stock with no real cost basis, not something we chose to buy.
    ///
    /// `adopt_orphan_sell` sets `buy_price` to the ask so the position cannot
    /// look infinitely profitable, which means its realised "profit" is
    /// manufactured (net of the bazaar tax it books a small loss). Without this
    /// flag the ledger would file that fiction next to real trades and drag the
    /// headline ROI down by exactly the tax on every orphan liquidated.
    ///
    /// `#[serde(default)]` = false, so positions written before this field
    /// existed reload as ordinary trades, which is what they were.
    #[serde(default)]
    pub orphan: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// The finder
// ─────────────────────────────────────────────────────────────────────────────

pub struct BazaarFinder {
    pub tuning: Tuning,
    db_path: String,
    state_path: String,
    names: HashMap<String, String>,
    stats: HashMap<String, ProductStats>,
    /// tag → the open position, one per product so we never stack two orders on
    /// the same book and bid against ourselves.
    open: Mutex<HashMap<String, Position>>,
    /// tag → unix ms until which the manipulation guard refuses this product.
    /// Deliberately NOT persisted: a cooldown is a statement about the last few
    /// minutes of a live book, and reloading a stale one after a restart would
    /// block a product for a reason that has long since expired.
    quarantine: Mutex<HashMap<String, i64>>,
    /// Tag -> earliest ms at which we will buy it again after closing a
    /// position. A hard backstop against the one failure that actually costs
    /// money: a position that closes wrongly is instantly re-bought, and the
    /// loop compounds. Observed live 2026-08-14 as FOUR duplicate 2.0M Locust
    /// Larva buy orders, 7.9M committed on one thin product, all placed within
    /// a minute of each other.
    rebuy_cooldown: Mutex<HashMap<String, i64>>,
    /// Tag -> how many times in a row an order in it vanished without ever
    /// reaching the book. Reset by the first confirmation. Distinguishes a bot
    /// that happened to be warping (retry, it will work) from a product that
    /// cannot be ordered at all (stop, it never will).
    phantom_strikes: Mutex<HashMap<String, i64>>,
    /// Realised P&L. Until this existed the module banked nothing: the only
    /// place a trade's result is knowable threw the sell price away one line
    /// after counting the fill. See [`crate::bazaar_ledger`].
    pub ledger: BazaarLedger,
}

/// Why a product was not tradeable this pass. The FIRST failing gate is the one
/// reported, so the order below is also the order of the explanation.
type Reject = &'static str;

impl BazaarFinder {
    pub fn new(db_path: &str) -> Self {
        let tuning = Tuning::default();
        let state_path = std::env::var("BZ_STATE_PATH").unwrap_or_else(|_| {
            std::path::Path::new(db_path)
                .parent()
                .map(|d| {
                    d.join("bazaar-positions.json")
                        .to_string_lossy()
                        .into_owned()
                })
                .unwrap_or_else(|| "./bazaar-positions.json".to_string())
        });
        let open = std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<String, Position>>(&s).ok())
            .unwrap_or_default();
        if !open.is_empty() {
            eprintln!(
                "bazaar-finder: restored {} open position(s) from {}",
                open.len(),
                state_path
            );
        }
        // Built before the struct literal: `state_path` is moved into it.
        let ledger = BazaarLedger::load(&state_path);
        BazaarFinder {
            tuning,
            db_path: db_path.to_string(),
            state_path,
            names: HashMap::new(),
            stats: HashMap::new(),
            open: Mutex::new(open),
            quarantine: Mutex::new(HashMap::new()),
            rebuy_cooldown: Mutex::new(HashMap::new()),
            phantom_strikes: Mutex::new(HashMap::new()),
            ledger,
        }
    }

    pub fn refresh_names(&mut self) {
        let n = hypixel::fetch_item_names();
        if !n.is_empty() {
            eprintln!("bazaar-finder: {} item display names", n.len());
            self.names = n;
        }
    }

    pub fn refresh_stats(&mut self) {
        let t = Instant::now();
        let conn = match Connection::open(&self.db_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("bazaar-finder: open {} failed: {e}", self.db_path);
                return;
            }
        };
        match compute_stats(
            &conn,
            self.tuning.stats_hours,
            self.tuning.tax,
            self.tuning.manip_window_min,
        ) {
            Ok(s) => {
                eprintln!(
                    "bazaar-finder: stats for {} products over {}h in {:?}",
                    s.len(),
                    self.tuning.stats_hours,
                    t.elapsed()
                );
                self.stats = s;
            }
            Err(e) => eprintln!("bazaar-finder: stats failed: {e}"),
        }
    }

    fn display_name(&self, tag: &str) -> Option<String> {
        self.names.get(tag).cloned()
    }

    /// Reverse of `display_name`. Chat only ever names an item the way the GUI
    /// does, so adopting an order Hypixel reports needs this direction.
    fn tag_for_name(&self, name: &str) -> Option<String> {
        self.names
            .iter()
            .find(|(_, n)| n.eq_ignore_ascii_case(name))
            .map(|(t, _)| t.clone())
    }

    /// True while `tag` is serving a manipulation cooldown. Expired entries are
    /// dropped as they are found, so the map self-cleans without a sweep.
    fn is_quarantined(&self, tag: &str) -> bool {
        let mut q = self.quarantine.lock().unwrap();
        match q.get(tag) {
            Some(&until) if until > now_ms() => true,
            Some(_) => {
                q.remove(tag);
                false
            }
            None => false,
        }
    }

    /// Compare the live book against this product's own median over the last
    /// `manip_window_min`. A move beyond `manip_trigger_pct` on EITHER side
    /// quarantines the product and returns true.
    ///
    /// Both sides are checked because the two manipulations look different: a
    /// pumped ask fakes the profit we would book, while a pumped bid makes us
    /// pay up for inventory whose real value is lower. Both are measured
    /// against their own side's median rather than against each other, so a
    /// genuine wholesale move that carries the whole book still trips it, which
    /// is intended: we cannot tell that apart from a pump in the moment, and
    /// declining to trade for 15 minutes costs a slot-hour, not a position.
    fn trips_manipulation_guard(&self, tag: &str, ask: f64, bid: f64, st: &ProductStats) -> bool {
        let t = &self.tuning;
        if t.manip_trigger_pct <= 0.0 {
            return false;
        }
        let dev = |live: f64, anchor: f64| -> f64 {
            if !anchor.is_finite() || anchor <= 0.0 || !live.is_finite() {
                return 0.0; // no anchor in the window ⇒ no opinion, never a trigger
            }
            (live - anchor).abs() / anchor * 100.0
        };
        let bid_dev = dev(bid, st.recent_bid_median);
        let ask_dev = dev(ask, st.recent_ask_median);
        let worst = bid_dev.max(ask_dev);
        if worst <= t.manip_trigger_pct {
            return false;
        }
        let until = now_ms() + t.manip_cooldown_min.max(0) * 60_000;
        self.quarantine
            .lock()
            .unwrap()
            .insert(tag.to_string(), until);
        eprintln!(
            "bazaar-finder: {tag} quarantined {}min — live bid {:.1}/ask {:.1} vs {}min medians \
             {:.1}/{:.1} (moved {:.1}%, trigger {:.1}%)",
            t.manip_cooldown_min,
            bid,
            ask,
            t.manip_window_min,
            st.recent_bid_median,
            st.recent_ask_median,
            worst,
            t.manip_trigger_pct
        );
        true
    }

    /// Evaluate one product for a BUY order. `budget` is the coins this bot may
    /// commit to a single order.
    pub fn evaluate_buy(
        &self,
        tag: &str,
        p: &BazaarProduct,
        budget: f64,
        free_slots: i64,
    ) -> Result<Plan, Reject> {
        let t = &self.tuning;
        // First, and ahead of every book-shape gate: these are items measured to
        // not fill for us, and nothing in the book's shape reveals that. Running
        // it first also means a blacklisted product reports "blacklisted" rather
        // than whichever incidental gate it happens to trip.
        if t.blacklist.contains(tag) {
            return Err("blacklisted (does not fill)");
        }
        // Still serving a cooldown from an earlier anomalous print. Checked
        // before the book is even read, because the whole point is to ignore
        // what the book currently says.
        if self.is_quarantined(tag) {
            return Err("quarantined (recent price manipulation)");
        }
        let (ask, bid) = (p.top_insta_buy, p.top_insta_sell);
        if ask <= 0.0 || bid <= 0.0 {
            return Err("one-sided book");
        }
        if bid < t.min_unit_price {
            return Err("unit price below floor");
        }
        let st = self.stats.get(tag).ok_or("no history")?;
        if st.samples < 60 {
            return Err("history too thin");
        }
        // Detection runs once we have both the live book and a recent anchor.
        // It sets the cooldown, so the NEXT pass short-circuits above without
        // re-reading the book at all.
        if self.trips_manipulation_guard(tag, ask, bid, st) {
            return Err("price moved too far, too fast");
        }
        // Trap gate, LIVE: a book this wide right now is a spread nobody is
        // crossing right now. ENCHANTED_SPRUCE_LOG sat at ask 1,754 / bid 644 —
        // 2.7x — with 5,313 units resting at the ask and 71,077 at the bid, so
        // it is not a thin top and depth cannot tell it apart from an edge. Only
        // the width does.
        //
        // ⚠️ This must be checked live as well as on the median. The median
        // version below went blind on exactly this product because the book had
        // been ~1.25x for most of the 24h window, and a mid-window regime change
        // left the historical gate looking at a market that no longer existed.
        // Checking the live ratio also bounds the margin structurally: at 1.6 the
        // most that can pass is ~58% after tax, which is why no history-relative
        // margin cap is needed to keep fantasy spreads off the top of the list.
        if ask / bid > t.max_ask_bid_ratio {
            return Err("book too wide right now");
        }
        // And the historical twin: a book that has been wide for 25 days is a
        // book whose top does not trade, even if it happens to look tight in
        // this instant.
        if st.median_ask_bid_ratio > t.max_ask_bid_ratio {
            return Err("book too wide to be real");
        }
        if st.stability < t.min_stability {
            return Err("spread not persistent");
        }
        if st.undercut_per_hour > t.max_undercut_per_hour {
            return Err("outbid too often");
        }
        let flow = (p.buy_moving_week.min(p.sell_moving_week) as f64) / 7.0;
        if flow < t.min_flow_per_day {
            return Err("flow too thin on one side");
        }

        // We must quote against what is actually on the book, so the BUY price
        // is the live one. What that buy is worth, though, is decided further
        // down against the book's real depth rather than against history.
        let buy_at = tick_up(bid);

        // ── Size first, because how much we are moving decides which price we
        //    can actually get out at. ──────────────────────────────────────────
        let name = self.display_name(tag).ok_or("no display name")?;
        let mut units = (budget / buy_at)
            .min(t.max_order_coins / buy_at)
            .min(flow * t.max_flow_share)
            .min(MAX_UNITS_PER_ORDER);
        // Filled units have to be claimed into the bot's inventory before they
        // can be offered back, so the inventory is a hard cap on order size for
        // EVERY product, not just books. A book costs a whole slot; anything
        // else costs a slot per stack.
        //
        // The mod only caps the unstackable case (`main.rs:3239`) and passes any
        // stackable amount straight through, so this side has to be right. It
        // bites hardest when the coin cap is small: a 2M order of a 715-coin
        // material is 2,797 units, which is 44 slots of a 36-slot inventory. The
        // overflow is not lost, it just sits unclaimed while the position closes
        // on the part that fit.
        let per_slot = if is_unstackable(tag, &name) {
            1.0
        } else {
            t.units_per_slot
        };
        let by_slots = (free_slots - t.unstackable_reserve).max(0) as f64 * per_slot;
        if by_slots < 1.0 {
            return Err("no inventory slot for this buy");
        }
        units = units.min(by_slots);
        let units = units.floor();
        if units < 1.0 {
            return Err("cannot afford one unit");
        }

        // ── The exit price, read off the live book. ──────────────────────────
        // The best ask is only worth what stands behind it. Where the top is one
        // thin order and the real supply sits lower, selling into that top is a
        // fiction: it is gone, or undercut, long before our buy fills. So take
        // the price at which the resting asks actually cover our own size, and
        // never assume better than the live top.
        //
        // This replaces a gate that compared the live margin to a 24h median and
        // refused anything above 3x it. That inferred depth from history; this
        // reads it. It is also strictly more informative: it prices a merely
        // THIN top down instead of refusing it, and it catches a gap whose
        // margin happens to sit under the old multiple.
        let depth_need = units.max(t.min_depth_units);
        let backed_ask = price_at_depth(&p.ask_levels, depth_need);
        let exit_at = match backed_ask {
            Some(px) => tick_down(ask).min(px),
            // Book too shallow to cover our own size at any price. Do not guess.
            None if !p.ask_levels.is_empty() => return Err("sell side too thin for this size"),
            // No book detail at all (a poll that predates level capture).
            None => tick_down(ask),
        };
        let net_unit = exit_at * (1.0 - t.tax) - buy_at;
        if net_unit <= 0.0 {
            return Err("no margin after tax");
        }
        let margin_pct = net_unit / buy_at * 100.0;
        if margin_pct < t.min_margin_pct {
            return Err("margin below floor");
        }
        // Drift while we hold has to be small next to what we are collecting.
        // Measured 1h volatility is under 1% for most books, so this rarely
        // binds — it is here to catch a book that is moving, not to trim.
        if st.vol_1h_pct > margin_pct / 2.0 {
            return Err("too volatile for the margin");
        }

        // Coins per slot-hour, the only ranking that respects what we actually
        // run out of. Both fill probabilities are applied because an order that
        // never fills still held the slot.
        let hours = (t.prior_buy_fill_h + t.prior_sell_fill_h).max(0.01);
        let p_round = t.prior_buy_fill_p * t.prior_sell_fill_p;
        let score = net_unit * units * p_round / hours;

        Ok(Plan {
            tag: tag.to_string(),
            name,
            side: Side::Buy,
            unit_price: buy_at,
            units,
            net_unit,
            margin_pct,
            score,
        })
    }

    /// The sell offer that closes a position. Never below the break-even price,
    /// which is what turns a slow flip into a small loss instead of a big one.
    pub fn evaluate_sell(&self, pos: &Position, p: &BazaarProduct) -> Result<Plan, Reject> {
        let t = &self.tuning;
        if p.top_insta_buy <= 0.0 {
            return Err("no ask side");
        }
        let ask = tick_down(p.top_insta_buy);
        let break_even = pos.buy_price / (1.0 - t.tax);
        if ask < break_even {
            return Err("ask below break-even");
        }
        let net_unit = ask * (1.0 - t.tax) - pos.buy_price;
        Ok(Plan {
            tag: pos.tag.clone(),
            name: pos.name.clone(),
            side: Side::Sell,
            unit_price: ask,
            units: pos.units,
            net_unit,
            margin_pct: net_unit / pos.buy_price * 100.0,
            score: net_unit * pos.units / t.prior_sell_fill_h.max(0.01),
        })
    }

    /// Rank every product for a buy, best coins-per-slot-hour first. Returns the
    /// reject tally alongside so a quiet pass can say WHY it was quiet.
    pub fn rank_buys(
        &self,
        products: &HashMap<String, BazaarProduct>,
        budget: f64,
        free_slots: i64,
    ) -> (Vec<Plan>, HashMap<Reject, i64>) {
        let mut plans = Vec::new();
        let mut rejects: HashMap<Reject, i64> = HashMap::new();
        let open = self.open.lock().unwrap();
        for (tag, p) in products {
            if open.contains_key(tag) {
                *rejects.entry("already holding").or_insert(0) += 1;
                continue;
            }
            if self.in_rebuy_cooldown(tag) {
                *rejects.entry("closed too recently").or_insert(0) += 1;
                continue;
            }
            match self.evaluate_buy(tag, p, budget, free_slots) {
                Ok(plan) => plans.push(plan),
                Err(r) => *rejects.entry(r).or_insert(0) += 1,
            }
        }
        plans.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        (plans, rejects)
    }

    // ── position bookkeeping ────────────────────────────────────────────────

    pub fn record_buy(&self, plan: &Plan, bot: &str) {
        let mut open = self.open.lock().unwrap();
        open.insert(
            plan.tag.clone(),
            Position {
                tag: plan.tag.clone(),
                name: plan.name.clone(),
                units: plan.units,
                buy_price: plan.unit_price,
                placed_at_ms: now_ms(),
                sell_issued_at_ms: None,
                sell_price: None,
                bot: bot.to_string(),
                repriced: 0,
                // Whatever the bot is holding right now is not ours.
                baseline_units: confirmed_units(bot, &plan.tag).unwrap_or(0.0),
                units_in_offer: 0.0,
                units_sold: 0.0,
                orphan: false,
                confirmed_at_ms: None,
                units_filled: 0.0,
            },
        );
        drop(open);
        self.persist();
    }

    /// Apply what Hypixel said. Positions are matched by DISPLAY NAME, which is
    /// what the chat carries and what we already store.
    pub fn apply_chat(&self, bot: &str, ev: &BazaarChat) {
        let mut open = self.open.lock().unwrap();
        let find = |open: &mut HashMap<String, Position>, name: &str| -> Option<String> {
            open.values()
                .find(|p| p.bot == bot && p.name.eq_ignore_ascii_case(name))
                .map(|p| p.tag.clone())
        };
        match ev {
            BazaarChat::BuyPlaced { name, units, coins } => {
                if find(&mut open, name).is_none() {
                    // 🔥 ADOPT it. This is a real buy order, on the book, with our
                    // coins in escrow -- and until 2026-08-15 the finder answered
                    // it with a log line and nothing else, so the units arrived
                    // and no sell offer was ever made for them. That is the
                    // "collects stuff but never sells it" symptom exactly.
                    //
                    // It happens whenever a placement lands after we gave up on
                    // it: the mod queues our order behind whatever the bot was
                    // already doing, so a slow pass confirms late. Dropping the
                    // position is safe; forgetting the ORDER is not.
                    // ⚠️ The derived fallback must be loud. `our_units` looks the
                    // holdings up BY TAG, so a tag the inventory feed does not
                    // use reads as "the bot holds none of this" and the sell leg
                    // never fires -- the same silent hold we are fixing here.
                    let tag = self.tag_for_name(name).unwrap_or_else(|| {
                        let guess = name.to_uppercase().replace(' ', "_");
                        eprintln!(
                            "bazaar-finder: ⚠️ no tag known for {name:?}, guessing {guess} -- \
                             if the sell never fires, this is why"
                        );
                        guess
                    });
                    // ⛔ Adopt only with a real cost basis. Every sell decision
                    // divides by `buy_price` (`break_even`, `margin_pct`), so a
                    // zero basis makes any ask look infinitely profitable and
                    // would dump the stock at the floor. Both Setup lines carry
                    // the coin total, so this is a guard, not a normal path.
                    match coins.filter(|_| *units > 0.0).map(|c| c / units) {
                        Some(unit_price) => {
                            eprintln!(
                                "bazaar-finder: ADOPTED {name} x{units:.0} @ {unit_price:.1}/u on \
                                 {bot} -- on the book but untracked"
                            );
                            open.insert(
                                tag.clone(),
                                Position {
                                    name: name.clone(),
                                    units: *units,
                                    buy_price: unit_price,
                                    placed_at_ms: now_ms(),
                                    sell_issued_at_ms: None,
                                    sell_price: None,
                                    bot: bot.to_string(),
                                    repriced: 0,
                                    baseline_units: confirmed_units(bot, &tag).unwrap_or(0.0),
                                    units_in_offer: 0.0,
                                    units_sold: 0.0,
                                    confirmed_at_ms: Some(now_ms()),
                                    units_filled: 0.0,
                                    orphan: false,
                                    tag,
                                },
                            );
                        }
                        // ⚠️ Loud on purpose. If the bot name on the chat socket
                        // ever stops matching the one on the position, every
                        // match here fails and nothing ever sells, silently.
                        None => eprintln!(
                            "bazaar-finder: ⚠️ {bot} placed {name} x{units:.0} with no price in \
                             chat and we track no such position -- NOT adopted"
                        ),
                    }
                }
                if let Some(tag) = find(&mut open, name) {
                    self.clear_phantom_strikes(&tag);
                    let p = open.get_mut(&tag).unwrap();
                    p.confirmed_at_ms = Some(now_ms());
                    // Trust the server's count over ours: the mod caps
                    // unstackable buys on its own side, so what reached the book
                    // can legitimately be smaller than what we asked for.
                    p.units = *units;
                }
            }
            BazaarChat::SellFilled { name, units } => {
                if let Some(tag) = find(&mut open, name) {
                    let done = {
                        let p = open.get_mut(&tag).unwrap();
                        // ⚠️ Bank the result BEFORE `sell_price` is cleared two
                        // lines down. This is the ONLY moment both legs of the
                        // trade are known, and it used to be thrown away.
                        if let Some(ask) = p.sell_price {
                            self.ledger.record(
                                &p.tag,
                                &p.name,
                                &p.bot,
                                *units,
                                p.buy_price,
                                ask,
                                self.tuning.tax,
                                p.placed_at_ms,
                                now_ms(),
                                if p.orphan {
                                    Source::Orphan
                                } else {
                                    Source::Chat
                                },
                            );
                        }
                        p.units_sold += units;
                        p.units_in_offer = (p.units_in_offer - units).max(0.0);
                        p.sell_issued_at_ms = None;
                        p.sell_price = None;
                        eprintln!(
                            "bazaar-finder: FILLED sell {} x{units:.0} ({:.0}/{:.0} done)",
                            p.name, p.units_sold, p.units
                        );
                        p.units_sold >= p.units - 1.0
                    };
                    if done {
                        eprintln!("bazaar-finder: CLOSED {name} (sell confirmed by chat)");
                        open.remove(&tag);
                    }
                }
            }
            BazaarChat::NotCompetitive => {
                // Hypixel refused an order outright. The only candidate is a
                // position on this bot that chat has never confirmed.
                let stale: Vec<String> = open
                    .values()
                    .filter(|p| p.bot == bot && p.confirmed_at_ms.is_none())
                    .map(|p| p.tag.clone())
                    .collect();
                for tag in stale {
                    if let Some(p) = open.remove(&tag) {
                        eprintln!(
                            "bazaar-finder: DROPPED {} x{:.0} (Hypixel: price not competitive)",
                            p.name, p.units
                        );
                    }
                }
            }
            BazaarChat::BuyFilled { name, units } => {
                if let Some(tag) = find(&mut open, name) {
                    let p = open.get_mut(&tag).unwrap();
                    p.units_filled += units;
                    eprintln!(
                        "bazaar-finder: FILLED buy {} x{units:.0} ({:.0}/{:.0})",
                        p.name, p.units_filled, p.units
                    );
                }
            }
            BazaarChat::SellPlaced { .. } | BazaarChat::MaxOrders { .. } => {}
        }
        drop(open);
        self.persist();
    }

    pub fn record_sell_issued(&self, tag: &str, price: f64, units: f64) {
        let mut open = self.open.lock().unwrap();
        if let Some(p) = open.get_mut(tag) {
            p.sell_issued_at_ms = Some(now_ms());
            p.sell_price = Some(price);
            p.units_in_offer = units;
        }
        drop(open);
        self.persist();
    }

    /// A sell offer has been resting long enough to call it done. Bank the units
    /// and let the position take another piece of the buy order if one filled
    /// while we were waiting.
    pub fn record_sell_settled(&self, tag: &str) {
        let mut open = self.open.lock().unwrap();
        if let Some(p) = open.get_mut(tag) {
            // ⚠️ Nobody confirmed this. The offer rested past
            // `BZ_ABANDON_AFTER_MIN` and we are banking it on the assumption it
            // sold; it may still be sitting on the book. Recorded as `Settled`
            // so it stays OUT of the headline P&L.
            if let Some(ask) = p.sell_price {
                self.ledger.record(
                    &p.tag,
                    &p.name,
                    &p.bot,
                    p.units_in_offer,
                    p.buy_price,
                    ask,
                    self.tuning.tax,
                    p.placed_at_ms,
                    now_ms(),
                    if p.orphan {
                        Source::Orphan
                    } else {
                        Source::Settled
                    },
                );
            }
            p.units_sold += p.units_in_offer;
            p.units_in_offer = 0.0;
            p.sell_issued_at_ms = None;
            p.sell_price = None;
        }
        drop(open);
        self.persist();
    }

    /// Pull a resting sell offer back without banking it as sold.
    ///
    /// ⛔ NOT [`BazaarFinder::record_sell_settled`], which adds `units_in_offer`
    /// to `units_sold`. Cancelling means the units come BACK; counting them as
    /// sold would invent revenue and, via the ledger, invent profit.
    /// `sellable` is recomputed from `units_filled - units_sold - units_in_offer`
    /// on the next pass, so clearing these three fields is all it takes for the
    /// ordinary sell path to re-offer at the live price.
    pub fn record_sell_cancelled(&self, tag: &str) {
        let mut open = self.open.lock().unwrap();
        if let Some(p) = open.get_mut(tag) {
            p.sell_issued_at_ms = None;
            p.sell_price = None;
            p.units_in_offer = 0.0;
            p.repriced += 1;
        }
        drop(open);
        self.persist();
    }

    pub fn close(&self, tag: &str) {
        self.open.lock().unwrap().remove(tag);
        self.rebuy_cooldown.lock().unwrap().insert(
            tag.to_string(),
            now_ms() + (self.tuning.rebuy_cooldown_min * 60_000.0) as i64,
        );
        self.persist();
    }

    /// The most valuable orphan a bot is holding, or `None` if nothing qualifies.
    ///
    /// An orphan is bazaar stock in the inventory that no position accounts for.
    /// The guards exist because the inventory also holds things the bot needs:
    /// equipment and consumables arrive as x1 while bazaar stock arrives in
    /// hundreds, and anything the bot uses itself is excluded outright.
    pub fn best_orphan(
        &self,
        products: &HashMap<String, BazaarProduct>,
        tracked: &std::collections::HashSet<String>,
        held: &[(String, f64)],
    ) -> Option<(Plan, f64)> {
        let t = &self.tuning;
        let mut best: Option<(Plan, f64)> = None;
        for (tag, units) in held {
            if *units < t.orphan_min_units
                || tracked.contains(tag)
                || t.orphan_exclude.contains(tag.as_str())
            {
                continue;
            }
            let Some(p) = products.get(tag) else { continue };
            if p.top_insta_buy <= 0.0 {
                continue;
            }
            // ⛔ No cost basis exists for an orphan, so there is no margin to
            // check and none is checked. The unit is already bought; the only
            // question left is whether to keep paying for the slot it occupies.
            // Price it at the top of the sell book like any other offer -- NOT
            // at the bid, which would be a dump.
            let ask = tick_down(p.top_insta_buy);
            let value = ask * units;
            if value < t.orphan_min_coins {
                continue;
            }
            let Some(name) = self.display_name(tag) else {
                continue;
            };
            let plan = Plan {
                tag: tag.clone(),
                name,
                side: Side::Sell,
                unit_price: ask,
                units: *units,
                net_unit: 0.0,
                margin_pct: 0.0,
                score: value,
            };
            if best.as_ref().is_none_or(|(_, v)| value > *v) {
                best = Some((plan, value));
            }
        }
        best
    }

    /// Track an orphan we have just offered for sale, so the next pass does not
    /// offer it again and the settle timer can close it.
    ///
    /// `buy_price` is set to the ask rather than left at zero: we genuinely do
    /// not know what these units cost, and recording a zero basis would book the
    /// entire proceeds as profit and quietly corrupt the ledger. A flat basis
    /// books it as neither gain nor loss, which is the honest answer.
    pub fn adopt_orphan_sell(&self, plan: &Plan, bot: &str) {
        let mut open = self.open.lock().unwrap();
        open.insert(
            plan.tag.clone(),
            Position {
                tag: plan.tag.clone(),
                name: plan.name.clone(),
                units: plan.units,
                buy_price: plan.unit_price,
                placed_at_ms: now_ms(),
                sell_issued_at_ms: Some(now_ms()),
                sell_price: Some(plan.unit_price),
                bot: bot.to_string(),
                repriced: 0,
                baseline_units: 0.0,
                units_in_offer: plan.units,
                units_sold: 0.0,
                confirmed_at_ms: Some(now_ms()),
                units_filled: plan.units,
                orphan: true,
            },
        );
        drop(open);
        self.persist();
    }

    /// Drop an order that never reached the book, and say whether the product is
    /// worth another try. Returns `(strikes, retrying)`.
    ///
    /// ⛔ This is NOT `close()`. `close()` arms the 30-minute rebuy cooldown
    /// because it means a real position ended and re-buying instantly is how the
    /// 2026-08-14 runaway compounded. A phantom is the opposite: Hypixel never
    /// acknowledged anything, so no escrow exists and no coins moved. Barring
    /// the product for half an hour just forfeits it -- on 2026-08-15 that
    /// combination turned 103 buy decisions into 27 live orders.
    pub fn close_phantom(&self, tag: &str) -> (i64, bool) {
        self.open.lock().unwrap().remove(tag);
        let strikes = {
            let mut s = self.phantom_strikes.lock().unwrap();
            let n = s.entry(tag.to_string()).or_insert(0);
            *n += 1;
            *n
        };
        let retrying = strikes < self.tuning.phantom_max_strikes;
        let bar_min = if retrying {
            self.tuning.phantom_retry_min
        } else {
            self.tuning.rebuy_cooldown_min
        };
        self.rebuy_cooldown
            .lock()
            .unwrap()
            .insert(tag.to_string(), now_ms() + (bar_min * 60_000.0) as i64);
        self.persist();
        (strikes, retrying)
    }

    /// The product placed fine, so whatever went wrong before was the bot, not
    /// the item. Without this a product accumulates strikes across a whole day
    /// of healthy trading and eventually bans itself.
    fn clear_phantom_strikes(&self, tag: &str) {
        self.phantom_strikes.lock().unwrap().remove(tag);
    }

    /// Closed too recently to buy again. Expired entries are dropped as found.
    fn in_rebuy_cooldown(&self, tag: &str) -> bool {
        let mut c = self.rebuy_cooldown.lock().unwrap();
        match c.get(tag) {
            Some(&until) if now_ms() < until => true,
            Some(_) => {
                c.remove(tag);
                false
            }
            None => false,
        }
    }

    pub fn positions(&self) -> Vec<Position> {
        self.open.lock().unwrap().values().cloned().collect()
    }

    /// Realised P&L plus what is still at risk, as one log line.
    ///
    /// Exposure is the cost of units we are CONFIRMED to hold and have not sold
    /// (`units_filled - units_sold`), not the notional size of the buy order:
    /// an order resting on the book has not spent anything yet, and counting it
    /// as capital at risk would overstate exposure by the ~76% of orders that
    /// never fill ([[finder-bazaar-orders-are-not-placed]]).
    pub fn report_pnl(&self) {
        let open = self.open.lock().unwrap();
        let mut cost = 0.0;
        let mut units = 0.0;
        for p in open.values() {
            let held = (p.units_filled - p.units_sold).max(0.0);
            units += held;
            cost += held * p.buy_price;
        }
        let n = open.len();
        drop(open);
        self.ledger.report(now_ms(), cost, units, n);
    }

    fn persist(&self) {
        let open = self.open.lock().unwrap();
        if let Ok(s) = serde_json::to_string(&*open) {
            let tmp = format!("{}.tmp", self.state_path);
            if std::fs::write(&tmp, s).is_ok() {
                let _ = std::fs::rename(&tmp, &self.state_path);
            }
        }
    }

    /// Sell offers that are resting BEHIND the book and should be re-quoted.
    ///
    /// The counterpart to [`BazaarFinder::stale_buys`], which deliberately skips
    /// any position with a sell offer out (`sell_issued_at_ms.is_some()`) — so
    /// until now nothing ever revisited a sell price. Measured 2026-08-16:
    /// **25 of 25** resting offers sat above the current best ask, which is the
    /// whole reason `units_sold` was 0 on 23 of 34 open positions.
    ///
    /// Returns `(position, front_of_book_ask)`. The caller cancels; the ordinary
    /// sell pass then re-offers at the live price on the next tick, so the price
    /// is decided in exactly one place ([`BazaarFinder::evaluate_sell`]) and this
    /// cannot invent an ask of its own.
    ///
    /// ⚠️ Deliberately does NOT ladder below the front of book. `evaluate_sell`
    /// already quotes `tick_down(top_insta_buy)`, i.e. one tick under the
    /// cheapest live offer, so front-of-book IS the aggressive passive price.
    /// Anything lower is crossing the spread, which is a different decision with
    /// different risk and is not what this does.
    pub fn stale_sells(&self, products: &HashMap<String, BazaarProduct>) -> Vec<(Position, f64)> {
        let t = &self.tuning;
        if t.sell_requote_min <= 0.0 {
            return Vec::new();
        }
        let now = now_ms();
        let mut out = Vec::new();
        for pos in self.open.lock().unwrap().values() {
            let (Some(at), Some(ask)) = (pos.sell_issued_at_ms, pos.sell_price) else {
                continue; // no offer resting
            };
            if pos.repriced >= t.sell_max_requotes {
                continue;
            }
            if (now - at) as f64 / 60_000.0 < t.sell_requote_min {
                continue;
            }
            let Some(p) = products.get(&pos.tag) else {
                continue;
            };
            let front = tick_down(p.top_insta_buy);
            if front <= 0.0 || ask <= 0.0 {
                continue;
            }
            // Only worth a cancel/replace if we are materially behind. A one-tick
            // difference is noise and the round trip costs a GUI cycle.
            if front >= ask * (1.0 - t.sell_requote_gap) {
                continue;
            }
            // ⚠️ Re-quoting is pointless if `evaluate_sell` will refuse the new
            // price anyway. For a REAL position that means the book has fallen
            // under our cost and holding is correct. For an ORPHAN it is an
            // artefact: `adopt_orphan_sell` sets `buy_price` to the ask, so its
            // break-even is the ask times the tax and is ALWAYS above its own
            // offer — which is why the same orphans cycle offer -> 180min timer
            // -> re-adopt forever. Orphan stock has no real basis to defend, so
            // it is exempt.
            if !pos.orphan && front < pos.buy_price / (1.0 - t.tax) {
                continue;
            }
            out.push((pos.clone(), front));
        }
        out
    }

    /// Buy orders that have been outbid and have sat long enough to be worth
    /// cancelling and re-placing. Measured motivation: 45% of our top-of-book
    /// buy orders never filled at all, and being outbid is how that happens.
    pub fn stale_buys(
        &self,
        products: &HashMap<String, BazaarProduct>,
    ) -> Vec<(Position, &'static str)> {
        let t = &self.tuning;
        let now = now_ms();
        let mut out = Vec::new();
        for pos in self.open.lock().unwrap().values() {
            if pos.sell_issued_at_ms.is_some() {
                continue; // the buy side is done with
            }
            let age_min = (now - pos.placed_at_ms) as f64 / 60_000.0;
            if age_min > t.abandon_after_min {
                out.push((pos.clone(), "abandoned"));
                continue;
            }
            if age_min < t.reprice_after_min {
                continue;
            }
            if let Some(p) = products.get(&pos.tag) {
                if p.top_insta_sell > pos.buy_price {
                    // Being outbid by a thin order on a fast book is not a
                    // reason to give up the queue position we already hold.
                    // `sell_moving_week` is the flow that trades INTO buy orders
                    // like ours (Hypixel's names are taker-relative and read
                    // backwards here; see `BazaarProduct::bid_levels`).
                    let hourly_flow = p.sell_moving_week.max(0) as f64 / (7.0 * 24.0);
                    let ahead = units_ahead_of(&p.bid_levels, pos.buy_price);
                    if keep_when_outbid(
                        ahead,
                        hourly_flow,
                        pos.buy_price,
                        p.top_insta_sell,
                        t.outbid_keep_hours,
                        t.outbid_stale_gap,
                    ) {
                        continue;
                    }
                    out.push((pos.clone(), "outbid"));
                }
            }
        }
        out
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Fill confirmation
// ─────────────────────────────────────────────────────────────────────────────

/// bot → (item tag → units currently in that bot's inventory).
///
/// A bazaar buy order is not a purchase: 45% of ours never filled at all. So the
/// finder must never offer to sell something on the strength of having ASKED a
/// bot to buy it — it waits until the units are visibly in hand. The mod already
/// uploads its inventory to this socket for auction listing recommendations, and
/// every slot carries `tag` and `count`, so the confirmation costs nothing and
/// needs no change on the mod side.
static HOLDINGS: std::sync::OnceLock<Mutex<HashMap<String, HashMap<String, f64>>>> =
    std::sync::OnceLock::new();

fn holdings() -> &'static Mutex<HashMap<String, HashMap<String, f64>>> {
    HOLDINGS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// True when the bazaar finder is enabled. Cached: this is read on the
/// websocket inventory path, which the auction listing flow also runs on.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| env_on("BZ_FINDER"))
}

/// Called from the websocket inventory handler with the raw slot array.
/// Chat arrives on the websocket thread; the finder owns its positions on its
/// own thread. Queue here, drain at the top of the next pass.
fn chat_queue() -> &'static Mutex<Vec<(String, BazaarChat)>> {
    static Q: std::sync::OnceLock<Mutex<Vec<(String, BazaarChat)>>> = std::sync::OnceLock::new();
    Q.get_or_init(|| Mutex::new(Vec::new()))
}

/// Running tally of what the chat feed has understood, by event kind.
///
/// This is what makes a DRY run worth anything now. Dry mode sends no orders,
/// so no confirmation for OUR orders can ever arrive -- but the bots are using
/// the bazaar for COFL flips anyway, so their chat still exercises the parser
/// end to end. A dry run that reports zero events here means the feed is dead
/// and arming would put us straight back to guessing.
fn chat_seen() -> &'static Mutex<HashMap<&'static str, i64>> {
    static C: std::sync::OnceLock<Mutex<HashMap<&'static str, i64>>> = std::sync::OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Bots we have actually heard chat from.
///
/// 🔥 The mod's websocket points at COFL **or** at the finder, never both
/// (`ws_client = COFL OR finder`). A bot on COFL's socket will happily place our
/// orders and we will never hear whether they existed, which is exactly the
/// blindness that let 17 phantom orders and a 29.2M runaway through on
/// 2026-08-14. Hearing chat from a bot is self-proving evidence that its
/// confirmations will reach us, so that is the bar to trade on it.
/// ⚠️ PERSISTED, and it has to be. The set rebuilds only as chat arrives, so on
/// a fresh process every bot is inaudible and the finder trades with nobody
/// until each one happens to speak. Observed: 5 minutes after a restart, 3 chat
/// lines had arrived and `1 of 1 bot(s) skipped` -- armed, correct, and idle.
/// Having heard a bot once is permanent evidence about which socket it talks on.
fn chat_bots() -> &'static Mutex<std::collections::HashSet<String>> {
    static B: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    B.get_or_init(|| {
        let loaded = std::fs::read_to_string(audible_path())
            .ok()
            .and_then(|s| serde_json::from_str::<std::collections::HashSet<String>>(&s).ok())
            .unwrap_or_default();
        if !loaded.is_empty() {
            eprintln!(
                "bazaar-finder: {} bot(s) known audible from a previous run",
                loaded.len()
            );
        }
        Mutex::new(loaded)
    })
}

fn audible_path() -> String {
    std::env::var("BZ_AUDIBLE_FILE").unwrap_or_else(|_| {
        std::path::Path::new(&std::env::var("BAZAAR_DB_PATH").unwrap_or_default())
            .parent()
            .map(|d| d.join("bazaar-audible.json").to_string_lossy().into_owned())
            .unwrap_or_else(|| "./bazaar-audible.json".to_string())
    })
}

/// Have we ever heard this bot speak? Fail-closed: a bot we have not heard from
/// is not tradeable, because we could not tell a filled order from a dropped one.
pub fn bot_is_audible(bot: &str) -> bool {
    chat_bots().lock().unwrap().contains(bot)
}

/// bot → unix ms before which it is not worth sending an order to.
fn bot_busy_until() -> &'static Mutex<HashMap<String, i64>> {
    static B: std::sync::OnceLock<Mutex<HashMap<String, i64>>> = std::sync::OnceLock::new();
    B.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Chat that means the bot is between servers and cannot drive a GUI.
///
/// 🔥 This is the whole reason orders vanish. The mod accepts the order, runs
/// `/bz <name>`, finds the item, clicks it -- and the server swap invalidates
/// the window, so the next screen never opens. Five seconds later its own
/// watchdog logs `Window N open for >5 s in state Bazaar -- auto-closing` and
/// then marks the command **Completed**, having ordered nothing. Nothing in
/// that sequence is an error, which is why it never showed up in any log we
/// were reading. Traced on `Gloomgourd x569`, 2026-08-15 08:18:42.
fn is_transit_line(l: &str) -> bool {
    const MARKERS: [&str; 6] = [
        "Warping",
        "Sending to server",
        "joined the lobby",
        "You are already playing SkyBlock",
        "Welcome to Hypixel SkyBlock",
        "Evacuating to another server",
    ];
    MARKERS.iter().any(|m| l.contains(m))
}

/// Is this bot settled enough to be handed an order?
pub fn bot_is_ready(bot: &str) -> bool {
    let mut b = bot_busy_until().lock().unwrap();
    match b.get(bot) {
        Some(&until) if now_ms() < until => false,
        Some(_) => {
            b.remove(bot);
            true
        }
        None => true,
    }
}

fn kind_of(ev: &BazaarChat) -> &'static str {
    match ev {
        BazaarChat::BuyPlaced { .. } => "buy-placed",
        BazaarChat::SellPlaced { .. } => "sell-placed",
        BazaarChat::BuyFilled { .. } => "buy-filled",
        BazaarChat::SellFilled { .. } => "sell-filled",
        BazaarChat::NotCompetitive => "refused",
        BazaarChat::MaxOrders { .. } => "at-order-cap",
    }
}

/// Feed the bot's chat to the finder. `lines` is whatever the mod batched.
pub fn note_chat(bot: &str, lines: &[String]) {
    if bot.is_empty() || !enabled() {
        return;
    }
    // Count EVERY line, not just the bazaar ones. Otherwise "we understood
    // nothing" cannot be told apart from "the bots did no bazaar business since
    // we disarmed", and the liveness gate is worthless exactly when it matters.
    *chat_seen().lock().unwrap().entry("lines").or_insert(0) += lines.len() as i64;
    if !lines.is_empty() {
        let mut b = chat_bots().lock().unwrap();
        if b.insert(bot.to_string()) {
            eprintln!(
                "bazaar-finder: {bot} is audible ({} bot(s) now tradeable)",
                b.len()
            );
            if let Ok(j) = serde_json::to_string(&*b) {
                let tmp = format!("{}.tmp", audible_path());
                if std::fs::write(&tmp, j).is_ok() {
                    let _ = std::fs::rename(&tmp, audible_path());
                }
            }
        }
    }
    if let Some(l) = lines.iter().find(|l| is_transit_line(l)) {
        let until = now_ms() + (Tuning::default().transit_quiet_sec * 1000.0) as i64;
        bot_busy_until()
            .lock()
            .unwrap()
            .insert(bot.to_string(), until);
        if throttled("transit") {
            eprintln!(
                "bazaar-finder: {bot} is in transit ({}), holding orders",
                l.trim()
            );
        }
    }
    let mut q = chat_queue().lock().unwrap();
    for l in lines {
        if let Some(ev) = parse_bazaar_chat(l) {
            if let BazaarChat::MaxOrders { limit } = ev {
                // The server's own ceiling beats our assumption. Worth saying
                // out loud, because BZ_SLOTS_PER_BOT is a guess next to it.
                if throttled("max-orders") {
                    eprintln!("bazaar-finder: {bot} is at Hypixel's cap of {limit} bazaar orders");
                }
            }
            *chat_seen().lock().unwrap().entry(kind_of(&ev)).or_insert(0) += 1;
            q.push((bot.to_string(), ev));
        }
    }
    // A bot that is disconnected while the finder is idle must not grow this
    // without bound.
    if q.len() > 10_000 {
        let excess = q.len() - 10_000;
        q.drain(..excess);
    }
}

pub fn note_inventory(bot: &str, items: &[serde_json::Value]) {
    if bot.is_empty() || !enabled() {
        return;
    }
    let mut counts: HashMap<String, f64> = HashMap::new();
    for it in items {
        let Some(tag) = it.get("tag").and_then(|t| t.as_str()) else {
            continue;
        };
        let n = it.get("count").and_then(|c| c.as_f64()).unwrap_or(1.0);
        *counts.entry(tag.to_string()).or_insert(0.0) += n;
    }
    holdings().lock().unwrap().insert(bot.to_string(), counts);
}

/// Units of `tag` confirmed in `bot`'s inventory. `None` when that bot has never
/// uploaded one, which is different from "uploaded, and holds none".
pub fn confirmed_units(bot: &str, tag: &str) -> Option<f64> {
    let h = holdings().lock().unwrap();
    h.get(bot).map(|c| c.get(tag).copied().unwrap_or(0.0))
}

/// How much of what the bot is holding is actually ours: the total minus what
/// it already had when the buy order went in. `None` while the bot has not
/// uploaded an inventory at all, which is not the same as holding nothing.
fn our_units(bot: &str, tag: &str, baseline: f64) -> Option<f64> {
    confirmed_units(bot, tag).map(|held| (held - baseline).max(0.0))
}

/// Everything a bot is holding, tag and count. `None` until it has uploaded an
/// inventory at all.
fn held_tags(bot: &str) -> Option<Vec<(String, f64)>> {
    let h = holdings().lock().unwrap();
    h.get(bot)
        .map(|c| c.iter().map(|(t, n)| (t.clone(), *n)).collect())
}

/// Any bot that PHYSICALLY holds at least `need` units of `tag`.
///
/// 🔥 The position records which bot we ASKED to buy, which is not the same as
/// which bot is holding the goods. One mod instance runs two accounts and the
/// name on the socket is whichever is currently primary, so a position booked
/// under `argamer1014` can end up as stock in `zShadowReaper_`'s inventory.
/// Measured on prod 2026-08-16: of the bazaar tags in one bot's inventory, 2 of
/// 6 had positions attributed to a DIFFERENT bot, and both of those bots were
/// offline (`ROSTER ... HOLDING POSITIONS BUT NOT CONNECTED`).
///
/// Such a position is unsellable by either route: the sell path calls
/// `our_units(pos.bot, ..)`, gets `None` because that name never uploads an
/// inventory, and waits forever; and the orphan sweep skips the tag because a
/// position exists, so it counts as `tracked`. The units sit in a bot's hands
/// with nothing able to offer them.
///
/// Prefers the bot holding the MOST, so a split across accounts sells the
/// largest piece first rather than dribbling.
fn bot_holding(tag: &str, need: f64) -> Option<String> {
    let h = holdings().lock().unwrap();
    h.iter()
        .filter_map(|(bot, counts)| {
            let n = counts.get(tag).copied().unwrap_or(0.0);
            (n >= need.max(1.0)).then(|| (bot.clone(), n))
        })
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(bot, _)| bot)
}

// ─────────────────────────────────────────────────────────────────────────────
// Ground truth: Hypixel's own bazaar chat
// ─────────────────────────────────────────────────────────────────────────────

/// What Hypixel said actually happened to one of our orders.
///
/// 🔥 This is the only truthful signal the finder has, and it was already
/// arriving unused. `fleet.send()` returning true means a websocket write
/// succeeded, NOT that an order exists: the mod can still drop the order at its
/// `is_inventory_near_full()` guard (behind a `debug!`, so it is invisible at
/// INFO), and Hypixel itself rejects orders as "not competitive" -- 939 times in
/// our own logs. Both produce a position the finder believes in and the game has
/// never heard of.
///
/// The mod already ships every chat line to this socket as a `chatBatch`; it is
/// labelled `[COFL ->]` in its logs because the finder IS its COFL socket. So
/// none of this needs a mod change, only a handler.
#[derive(Clone, Debug, PartialEq)]
pub enum BazaarChat {
    /// `Buy Order Setup! 1,083x Hunk of Ice for 1,999,110 coins.`
    /// `coins` is the order total, and it is what gives an ADOPTED order a real
    /// cost basis instead of a guess.
    BuyPlaced {
        units: f64,
        name: String,
        coins: Option<f64>,
    },
    /// `Sell Offer Setup! 15x Jelly for 1,000 coins.`
    SellPlaced {
        units: f64,
        name: String,
        coins: Option<f64>,
    },
    /// `Your Buy Order for 64x Coal was filled!`
    BuyFilled { units: f64, name: String },
    /// `Your Sell Offer for 64x Coal was filled!` -- the true end of a round trip.
    SellFilled { units: f64, name: String },
    /// `Your price isn't competitive enough with the best order/offer's price.`
    /// The order was refused outright; nothing reached the book.
    NotCompetitive,
    /// `You reached your maximum of 14 Bazaar orders!` The real slot ceiling,
    /// stated by the server rather than assumed by `BZ_SLOTS_PER_BOT`.
    MaxOrders { limit: i64 },
}

/// Split `1,083x Hunk of Ice` into its count and its display name.
fn split_count_and_name(s: &str) -> Option<(f64, String)> {
    let (count, name) = s.split_once("x ")?;
    let n: f64 = count.trim().replace(',', "").parse().ok()?;
    // Some names arrive with a doubled space (`Nx  Flawed Onyx Gemstone`).
    Some((n, name.split_whitespace().collect::<Vec<_>>().join(" ")))
}

pub fn parse_bazaar_chat(line: &str) -> Option<BazaarChat> {
    let l = line.trim();
    // The mod prefixes its own log line but the payload it forwards does not.
    let l = l.strip_prefix("[Bazaar] ").unwrap_or(l);
    if l.starts_with("Your price isn't competitive") {
        return Some(BazaarChat::NotCompetitive);
    }
    if let Some(rest) = l.strip_prefix("You reached your maximum of ") {
        let n = rest
            .split_whitespace()
            .next()?
            .replace(',', "")
            .parse()
            .ok()?;
        return Some(BazaarChat::MaxOrders { limit: n });
    }
    for (prefix, cut, mk) in [
        ("Buy Order Setup! ", " for ", 0u8),
        ("Sell Offer Setup! ", " for ", 1),
        ("Your Buy Order for ", " was filled", 2),
        ("Your Sell Offer for ", " was filled", 3),
    ] {
        if let Some(rest) = l.strip_prefix(prefix) {
            let body = rest.split(cut).next()?;
            let (units, name) = split_count_and_name(body)?;
            // `... for 1,999,110 coins.` -- present on the two Setup lines only.
            let coins = rest
                .rsplit_once(" for ")
                .and_then(|(_, tail)| tail.split_whitespace().next())
                .and_then(|n| n.replace(',', "").parse::<f64>().ok());
            return Some(match mk {
                0 => BazaarChat::BuyPlaced { units, name, coins },
                1 => BazaarChat::SellPlaced { units, name, coins },
                2 => BazaarChat::BuyFilled { units, name },
                _ => BazaarChat::SellFilled { units, name },
            });
        }
    }
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// Report mode
// ─────────────────────────────────────────────────────────────────────────────

/// `BZ_REPORT=1`: print what the finder would do right now and exit. The way to
/// look at a tuning change before any bot acts on it.
pub fn report(db_path: &str) {
    let mut f = BazaarFinder::new(db_path);
    f.refresh_names();
    f.refresh_stats();
    let Some(full) = hypixel::fetch_bazaar_full() else {
        eprintln!("bazaar-finder: no live snapshot, nothing to report");
        return;
    };
    let budget = env_f64("BZ_REPORT_BUDGET", 10_000_000.0);
    let free_slots = env_i64("BZ_REPORT_FREE_SLOTS", 34);
    let (plans, rejects) = f.rank_buys(&full.products, budget, free_slots);
    println!(
        "bazaar-finder report: {} products, budget {:.0} coins/order",
        full.products.len(),
        budget
    );
    println!("tuning: {:?}\n", f.tuning);
    println!(
        "{:<30} {:>11} {:>11} {:>9} {:>6} {:>7} {:>8} {:>12} {:>6} {:>8}",
        "product",
        "buy@",
        "sell@",
        "net/unit",
        "net%",
        "net%24h",
        "units",
        "coins/slot-h",
        "stab",
        "outbid/h"
    );
    for plan in plans.iter().take(env_i64("BZ_REPORT_TOP", 40) as usize) {
        let st = f.stats.get(&plan.tag).cloned().unwrap_or_default();
        let sell_at = tick_down(full.products[&plan.tag].top_insta_buy);
        println!(
            "{:<30} {:>11.1} {:>11.1} {:>9.1} {:>6.2} {:>7.2} {:>8.0} {:>12.0} {:>6.2} {:>8.1}",
            plan.name.chars().take(30).collect::<String>(),
            plan.unit_price,
            sell_at,
            plan.net_unit,
            plan.margin_pct,
            st.median_margin_pct,
            plan.units,
            plan.score,
            st.stability,
            st.undercut_per_hour
        );
    }
    let mut rj: Vec<_> = rejects.into_iter().collect();
    rj.sort_by_key(|(_, n)| -*n);
    println!(
        "\nrejected: {}",
        rj.iter()
            .map(|(r, n)| format!("{r} {n}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("tradeable: {}", plans.len());
}

// ─────────────────────────────────────────────────────────────────────────────
// Loop
// ─────────────────────────────────────────────────────────────────────────────

/// What the loop needs from the outside world, so this module never has to know
/// what a websocket is.
pub trait Fleet: Send + Sync {
    /// Bots that can take a bazaar order right now: (name, purse coins).
    /// Bots that can take an order: `(name, purse, free inventory slots)`.
    /// Free slots matter because an unstackable unit costs one of them, and the
    /// mod drops the order outright when there are none.
    fn targets(&self) -> Vec<(String, f64, i64)>;
    /// Every connected bot regardless of purse. Selling must not require coins.
    fn holders(&self) -> Vec<(String, f64, i64)>;
    /// Deliver a payload to one bot. `false` when it could not be delivered.
    fn send(&self, bot: &str, payload: String) -> bool;
}

/// True at most once every `BZ_LOG_EVERY_MIN` per key. The loop runs every 60s,
/// so anything that describes a standing condition rather than an event has to
/// go through here or it buries the decisions it is meant to explain.
fn throttled(key: &'static str) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static LAST: OnceLock<Mutex<HashMap<&'static str, i64>>> = OnceLock::new();
    let every = env_i64("BZ_LOG_EVERY_MIN", 15).max(1) * 60_000;
    let now = now_ms();
    let mut m = LAST
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    match m.get(key) {
        Some(&t) if now - t < every => false,
        _ => {
            m.insert(key, now);
            true
        }
    }
}

pub fn run(db_path: &str, fleet: std::sync::Arc<dyn Fleet>) {
    let mut f = BazaarFinder::new(db_path);
    f.refresh_names();
    f.refresh_stats();
    let t = f.tuning.clone();
    eprintln!(
        "bazaar-finder: RUNNING{} (min margin {:.1}%, stability {:.2}, max outbid/h {:.0}, slots/bot {})",
        if t.dry_run { " [DRY]" } else { "" },
        t.min_margin_pct,
        t.min_stability,
        t.max_undercut_per_hour,
        t.slots_per_bot
    );
    let mut last_stats = Instant::now();
    let mut last_names = Instant::now();
    loop {
        let started = Instant::now();
        if last_stats.elapsed() >= Duration::from_secs((t.stats_refresh_min * 60) as u64) {
            f.refresh_stats();
            last_stats = Instant::now();
        }
        if last_names.elapsed() >= Duration::from_secs(6 * 3600) {
            f.refresh_names();
            last_names = Instant::now();
        }
        if let Some(full) = hypixel::fetch_bazaar_full() {
            tick(&f, &*fleet, &full.products);
        }
        if let Some(rem) = Duration::from_millis(t.interval_ms).checked_sub(started.elapsed()) {
            std::thread::sleep(rem);
        }
    }
}

/// One pass: close what can be closed, clear what is stuck, then fill free slots.
fn tick(f: &BazaarFinder, fleet: &dyn Fleet, products: &HashMap<String, BazaarProduct>) {
    let t = &f.tuning;

    // 0. Apply what Hypixel actually said before deciding anything. This is the
    //    only place a position becomes confirmed, filled or refuted by fact
    //    rather than by inference off inventory counts.
    for (bot, ev) in chat_queue().lock().unwrap().drain(..) {
        f.apply_chat(&bot, &ev);
    }
    if throttled("chat-feed") {
        let seen = chat_seen().lock().unwrap();
        let lines = seen.get("lines").copied().unwrap_or(0);
        let mut kinds: Vec<_> = seen
            .iter()
            .filter(|(k, _)| **k != "lines")
            .map(|(k, n)| format!("{k} {n}"))
            .collect();
        kinds.sort();
        if lines == 0 {
            eprintln!(
                "bazaar-finder: ⚠️ NO chat has reached the finder at all -- order \
                 confirmation is blind, do NOT arm on this"
            );
        } else if kinds.is_empty() {
            // Not a fault: the bots simply have not touched the bazaar yet. The
            // socket is proven, the parser is merely unexercised.
            eprintln!(
                "bazaar-finder: chat feed connected ({lines} lines) but no bazaar \
                 events yet -- socket proven, parser unexercised"
            );
        } else {
            eprintln!(
                "bazaar-finder: chat feed alive ({lines} lines: {})",
                kinds.join(", ")
            );
        }
    }
    // Realised P&L on the same throttle as the chat heartbeat: the two answer
    // "is the plumbing working" and "is it making money", and reading either
    // without the other is how a module that places 238 orders and confirms 56
    // gets called profitable.
    if throttled("pnl") {
        f.report_pnl();
    }
    let mut targets = fleet.targets();
    if t.require_chat {
        let before = targets.len();
        targets.retain(|(bot, _, _)| bot_is_audible(bot));
        if targets.len() < before && throttled("inaudible") {
            eprintln!(
                "bazaar-finder: {} of {before} bot(s) skipped -- no chat heard from them, so a \
                 filled order could not be told from a dropped one",
                before - targets.len()
            );
        }
    }
    // A bot that is warping cannot drive the bazaar GUI, and an order handed to
    // one is not refused -- it is silently eaten. Skipping it costs one pass.
    let settled = targets.len();
    targets.retain(|(bot, _, _)| bot_is_ready(bot));
    if targets.len() < settled && throttled("in-transit") {
        eprintln!(
            "bazaar-finder: {} of {settled} bot(s) skipped -- in transit between servers",
            settled - targets.len()
        );
    }
    // ⛔ Which bots are eligible, and why the rest are not. Without this the
    // roster is invisible: a bot can be silently absent from EVERY pass and the
    // only symptom is its inventory filling up while nothing sells.
    //
    // Observed 2026-08-16: 4 mod instances all reporting healthy purses
    // (337M-2.2B) and nearly full inventories (invUsed 24-33, invFree 3-12), yet
    // only 2 of the 7 bot names holding positions received any bazaar action at
    // all. The orphan sweep — the ONLY path that sells stock no position accounts
    // for — iterates `targets`, so a bot missing from it never has its inventory
    // drained, and its purse never comes back.
    if throttled("roster") {
        let eligible: Vec<String> = targets.iter().map(|(b, _, _)| b.clone()).collect();
        let mut excluded: Vec<String> = Vec::new();
        for (bot, purse, _) in fleet.targets() {
            if eligible.iter().any(|e| *e == bot) {
                continue;
            }
            let why = if t.require_chat && !bot_is_audible(&bot) {
                "no chat heard"
            } else if !bot_is_ready(&bot) {
                "in transit"
            } else {
                "unknown"
            };
            excluded.push(format!("{bot}({why}, purse {purse:.0})"));
        }
        // ⚠️ A bot can also be absent from `fleet.targets()` ENTIRELY, which the
        // loop above cannot see: `bazaar_targets()` drops any client that is
        // disconnected, unnamed, or reporting `purse <= 0`. That is the quiet
        // case — the finder still holds POSITIONS in its name, still tries to
        // sell them, and `our_units` returns None forever because no inventory
        // upload ever arrives under that name.
        let known: Vec<String> = fleet.targets().into_iter().map(|(b, _, _)| b).collect();
        let mut absent: Vec<String> = Vec::new();
        for pos in f.positions() {
            if pos.bot.is_empty()
                || known.iter().any(|k| *k == pos.bot)
                || absent.iter().any(|a| *a == pos.bot)
            {
                continue;
            }
            absent.push(pos.bot.clone());
        }
        eprintln!(
            "bazaar-finder: ROSTER eligible {} [{}]{}{}",
            eligible.len(),
            eligible.join(", "),
            if excluded.is_empty() {
                String::new()
            } else {
                format!(" | excluded {} [{}]", excluded.len(), excluded.join(", "))
            },
            if absent.is_empty() {
                String::new()
            } else {
                format!(
                    " | ⚠️ HOLDING POSITIONS BUT NOT CONNECTED/NO PURSE {} [{}]",
                    absent.len(),
                    absent.join(", ")
                )
            }
        );
    }
    if targets.is_empty() {
        // Do not go quiet here. A fleet that has not reported a purse yet and a
        // market with nothing worth buying look identical in the log otherwise,
        // and the first is a blind finder while the second is a working one.
        if throttled("no-targets") {
            eprintln!("bazaar-finder: idle, no bot is settled and reporting a purse");
        }
        return;
    }

    // 0b. A position Hypixel never acknowledged does not exist. The mod drops
    //     buys at its inventory-near-full guard behind a `debug!`, and the
    //     server refuses others outright, so without this a phantom sits in the
    //     book forever holding a slot against nothing.
    for pos in f.positions() {
        if pos.confirmed_at_ms.is_some() {
            continue;
        }
        let age_min = (now_ms() - pos.placed_at_ms) as f64 / 60_000.0;
        if age_min >= t.confirm_timeout_min {
            let (strikes, retrying) = f.close_phantom(&pos.tag);
            let next = if retrying {
                format!("retrying in {:.0}min", t.phantom_retry_min)
            } else {
                format!(
                    "{} strikes, benched for {:.0}min",
                    strikes, t.rebuy_cooldown_min
                )
            };
            eprintln!(
                "bazaar-finder: PHANTOM {} x{:.0} never reached the book ({age_min:.1}min unconfirmed; {next})",
                pos.name, pos.units
            );
        }
    }

    // 1. Sell side first: a position that can be closed frees a slot, and the
    //    sell is the slower half (measured p50 107 min), so it must not wait.
    let mut unconfirmed = 0;
    for mut pos in f.positions() {
        let mut bot = if pos.bot.is_empty() {
            targets[0].0.clone()
        } else {
            pos.bot.clone()
        };
        // The inventory upload gives a TOTAL per tag and cannot say where the
        // units came from, so only the excess over what the bot held when we
        // ordered can be ours.
        let mut ours = our_units(&bot, &pos.tag, pos.baseline_units);
        if let Some(at) = pos.sell_issued_at_ms {
            // ⛔ Do NOT close on an empty inventory. Placing a sell offer moves
            // the units out of the inventory immediately, so "inventory is now
            // zero" is evidence the offer went UP, not that it filled -- it
            // fires within a tick of the sell every time. That closed a
            // 1,083-unit position as sold while its buy order was still resting.
            //
            // The mod's chat carries the real fill message but the finder is not
            // on that feed (it gets listed/purse/ping/estimate/inventory only),
            // so until the mod uploads its open bazaar orders the only honest
            // settle is a timer. It is an assumption, and it says so.
            let resting_min = (now_ms() - at) as f64 / 60_000.0;
            if resting_min >= t.sell_settle_min {
                f.record_sell_settled(&pos.tag);
                let sold = pos.units_sold + pos.units_in_offer;
                if sold >= pos.units - 1.0 {
                    eprintln!(
                        "bazaar-finder: CLOSED {} x{:.0} (offer rested {resting_min:.0}min)",
                        pos.name, pos.units
                    );
                    f.close(&pos.tag);
                } else {
                    eprintln!(
                        "bazaar-finder: PARTIAL {} {:.0}/{:.0} sold, buy order still open",
                        pos.name, sold, pos.units
                    );
                }
            }
            continue;
        }
        // ⛔ Inventory is NOT the fill signal. The bots already hold bazaar stock
        // from other activity, so "the bot has 200 Jelly" says nothing about
        // whether OUR order filled -- reading it that way is what sold 1 unit
        // and closed a 1,083-unit position. Hypixel's own "Your Buy Order for
        // Nx X was filled!" is the signal; inventory only bounds it, because
        // units that have not been claimed yet cannot be offered.
        let sellable = (pos.units_filled - pos.units_sold - pos.units_in_offer).max(0.0);
        if sellable < 1.0 {
            continue; // buy order still resting, or everything filled is already offered
        }
        // 🔥 `None` means that name has never uploaded an inventory, which on a
        // fleet where one instance runs two accounts usually means it is simply
        // not the connected identity any more. The goods are still SOMEWHERE.
        // Sell them from whoever is actually holding them instead of waiting on
        // a bot that will never report. `baseline_units` is dropped on this path
        // on purpose: it describes the ORIGINAL bot's pre-existing stock and
        // says nothing about the holder's.
        //
        // ⚠️ Placed HERE, after the `sell_issued_at_ms` and `sellable` returns,
        // and not before them. The first cut ran it at the top of the loop, so a
        // position that already had an offer resting re-logged `REHOME` on every
        // 60s tick and then immediately `continue`d without doing anything: 15
        // REHOME lines covering 3 items, none of which led to a sell. Noise that
        // reads exactly like the fix working.
        if t.sell_from_holder && ours.is_none() {
            if let Some(holder) = bot_holding(&pos.tag, sellable) {
                eprintln!(
                    "bazaar-finder: REHOME {} x{:.0} {} -> {holder} (original bot not reporting)",
                    pos.name, sellable, bot
                );
                ours = confirmed_units(&holder, &pos.tag);
                bot = holder;
            }
        }
        match ours {
            None => {
                unconfirmed += 1;
                continue;
            }
            Some(units) if units < 1.0 => continue, // filled but not claimed yet
            Some(units) => pos.units = units.min(sellable),
        }
        if pos.units < 1.0 {
            continue;
        }
        let Some(p) = products.get(&pos.tag) else {
            continue;
        };
        match f.evaluate_sell(&pos, p) {
            Ok(plan) => {
                eprintln!(
                    "bazaar-finder: SELL {} x{:.0} @ {:.1} (net {:.0}/u, {:.2}%) -> {bot}",
                    plan.name, plan.units, plan.unit_price, plan.net_unit, plan.margin_pct
                );
                if !t.dry_run && fleet.send(&bot, plan.to_wire()) {
                    f.record_sell_issued(&pos.tag, plan.unit_price, plan.units);
                }
            }
            Err(r) => {
                // Holding below break-even is the expected state right after a
                // buy fills; it only matters if it lasts.
                let age_h = (now_ms() - pos.placed_at_ms) as f64 / 3_600_000.0;
                if age_h > 6.0 {
                    eprintln!(
                        "bazaar-finder: HOLD {} x{:.0} ({r}, {age_h:.1}h)",
                        pos.name, pos.units
                    );
                }
            }
        }
    }
    if unconfirmed > 0 {
        // Fail-closed, but say so: without an inventory upload the sell half of
        // this finder cannot run at all, and silence would look like "no sells
        // were worth making".
        eprintln!("bazaar-finder: {unconfirmed} position(s) waiting on an inventory upload to confirm the fill");
    }

    // 1b. Orphaned stock. Bazaar goods a bot holds that no position accounts
    //     for: buys that filled while the finder had already given up on them,
    //     back when a dropped position meant a forgotten order.
    //
    //     🔥 This is not tidying, it is the unblock. The mod refuses EVERY buy
    //     order once a bot is at ≤4 free inventory slots, silently, at `debug!`.
    //     Orphans accumulate, the inventory fills, and the whole finder goes
    //     quiet with nothing in any log to say why. Selling them is what turns
    //     the money back into coins AND reopens the buy side.
    if t.orphan_sweep {
        let tracked: std::collections::HashSet<String> =
            f.positions().into_iter().map(|p| p.tag).collect();
        // ⛔ `holders`, not `targets`: purse is a BUY constraint and the sweep is
        // the SELL side. A bot that has spent everything is exactly the one whose
        // inventory needs draining, and gating it on purse deadlocks it shut.
        let mut sweep_over = fleet.holders();
        if t.require_chat {
            sweep_over.retain(|(b, _, _)| bot_is_audible(b));
        }
        sweep_over.retain(|(b, _, _)| bot_is_ready(b));
        for (bot, _, _) in &sweep_over {
            let Some(held) = held_tags(bot) else { continue };
            // One per bot per pass, same as the buy side: a sweep that empties a
            // whole inventory in one tick is a burst of GUI work that the mod
            // drops on the floor anyway.
            let Some((plan, value)) = f.best_orphan(products, &tracked, &held) else {
                continue;
            };
            eprintln!(
                "bazaar-finder: ORPHAN SELL {} x{:.0} @ {:.1} (~{:.0}k, untracked stock) -> {bot}",
                plan.name,
                plan.units,
                plan.unit_price,
                value / 1000.0
            );
            if !t.dry_run && fleet.send(bot, plan.to_wire()) {
                f.adopt_orphan_sell(&plan, bot);
            }
        }
    }

    // 2. Buy orders that were outbid and are going nowhere.
    for (pos, why) in f.stale_buys(products) {
        let plan = Plan {
            tag: pos.tag.clone(),
            name: pos.name.clone(),
            side: Side::Buy,
            unit_price: pos.buy_price,
            units: pos.units,
            net_unit: 0.0,
            margin_pct: 0.0,
            score: 0.0,
        };
        eprintln!(
            "bazaar-finder: CANCEL {} x{:.0} @ {:.1} ({why})",
            pos.name, pos.units, pos.buy_price
        );
        if !t.dry_run {
            let bot = if pos.bot.is_empty() {
                targets[0].0.clone()
            } else {
                pos.bot.clone()
            };
            fleet.send(&bot, plan.to_cancel_wire());
        }
        f.close(&pos.tag);
    }

    // 2b. Sell offers stranded behind the book. Cancel them; the sell pass above
    //     re-offers at the live front of book on the next tick, so the price is
    //     still decided in exactly one place.
    for (pos, front) in f.stale_sells(products) {
        let ask = pos.sell_price.unwrap_or(0.0);
        let plan = Plan {
            tag: pos.tag.clone(),
            name: pos.name.clone(),
            side: Side::Sell,
            unit_price: ask,
            units: pos.units_in_offer,
            net_unit: 0.0,
            margin_pct: 0.0,
            score: 0.0,
        };
        eprintln!(
            "bazaar-finder: REQUOTE {} x{:.0} @ {:.1} -> front {:.1} ({:.0}% above book, requote {}/{})",
            pos.name,
            pos.units_in_offer,
            ask,
            front,
            (ask / front.max(0.01) - 1.0) * 100.0,
            pos.repriced + 1,
            t.sell_max_requotes
        );
        if !t.dry_run {
            let bot = if pos.bot.is_empty() {
                targets[0].0.clone()
            } else {
                pos.bot.clone()
            };
            fleet.send(&bot, plan.to_cancel_wire());
        }
        f.record_sell_cancelled(&pos.tag);
    }

    // 3. Fill free slots, best coins-per-slot-hour first, one product per bot
    //    per pass so a single tick cannot dump the whole purse into the bazaar.
    // ⛔ Capacity is the sum of what each bot has FREE, not a fleet total minus
    // everything held. The old form double-counted: with `BZ_SLOTS_PER_BOT=1`,
    // three positions all sitting on ONE bot zeroed the capacity for the whole
    // fleet, so two idle bots were frozen out and the finder issued nothing --
    // silently, because this returned without a word. Observed live on era54.
    let mut held_by_bot: HashMap<String, i64> = HashMap::new();
    for pos in f.positions() {
        // A position whose sell offer is already on the book does NOT occupy a
        // buy slot: the offer rests on the bazaar (charged against Hypixel's own
        // 14-order cap and enforced by the max-orders chat), and duplicate
        // protection still blocks a second buy of the same tag (`already
        // holding` in `rank_buys`). Holding a finder slot for the whole
        // `sell_settle_min` timer is what pinned 2 of 8 slots to sells that
        // never filled — measured 2026-08-15: 56 sells issued across 13
        // products, only 15.4% chat-confirmed fills, 61.5% closed by the timer
        // on an assumption. Freeing the slot at sell-issue time is what lets a
        // 12.9-min buy cycle (p50, 55% fill) reuse it.
        if pos.sell_issued_at_ms.is_some() {
            continue;
        }
        *held_by_bot.entry(pos.bot.clone()).or_insert(0) += 1;
    }
    let capacity: i64 = targets
        .iter()
        .map(|(bot, _, _)| (t.slots_per_bot - held_by_bot.get(bot).copied().unwrap_or(0)).max(0))
        .sum();
    if capacity <= 0 {
        if throttled("at-capacity") {
            eprintln!(
                "bazaar-finder: every bot is at its {} slot limit ({} position(s) open across {} bot(s))",
                t.slots_per_bot,
                f.positions().len(),
                targets.len()
            );
        }
        return;
    }
    // Inventory slots already promised to buy orders that have not filled yet.
    // `inv_free` cannot see them, because the units are not in the inventory
    // until the order fills and is claimed, so without this a bot with two
    // resting orders gets both of them sized against the same empty slots.
    // Only the UNFILLED remainder counts: anything already claimed is in
    // `inv_free` and subtracting it again would size us down twice.
    let mut committed: HashMap<String, f64> = HashMap::new();
    for pos in f.positions() {
        let held = confirmed_units(&pos.bot, &pos.tag).unwrap_or(0.0);
        let outstanding = (pos.units - held).max(0.0);
        if outstanding <= 0.0 {
            continue;
        }
        let per_slot = if is_unstackable(&pos.tag, &pos.name) {
            1.0
        } else {
            t.units_per_slot
        };
        *committed.entry(pos.bot.clone()).or_insert(0.0) += (outstanding / per_slot).ceil();
    }
    let targets: Vec<(String, f64, i64)> = targets
        .into_iter()
        .map(|(bot, purse, slots)| {
            let c = committed.get(&bot).copied().unwrap_or(0.0) as i64;
            (bot, purse, (slots - c).max(0))
        })
        .collect();
    let budget = targets
        .iter()
        .map(|(_, purse, _)| *purse)
        .fold(0.0, f64::max)
        * t.purse_share;
    if budget < t.min_unit_price {
        if throttled("no-budget") {
            eprintln!(
                "bazaar-finder: richest purse x{} is {budget:.0}, under the {:.0} minimum unit price",
                t.purse_share, t.min_unit_price
            );
        }
        return;
    }
    // Rank against the most capable bot we have: the per-bot check below still
    // refuses a plan that a given bot cannot actually take, so ranking on the
    // best capacity never issues something unaffordable, it only avoids hiding
    // a good product because some other bot happens to be full.
    let best_slots = targets
        .iter()
        .map(|(_, _, slots)| *slots)
        .max()
        .unwrap_or(0);
    let (plans, rejects) = f.rank_buys(products, budget, best_slots);
    // The reject histogram used to print only when NOTHING passed, which is the
    // one case where it is least interesting. It is the whole tuning signal --
    // which gate is doing the work, and whether a knob has quietly started
    // rejecting everything -- so it has to be reachable in normal operation too.
    if !plans.is_empty() && throttled("rejects") {
        let mut rj: Vec<_> = rejects.iter().map(|(r, n)| (*r, *n)).collect();
        rj.sort_by_key(|(_, n)| -*n);
        eprintln!(
            "bazaar-finder: {} tradeable, rejected {}",
            plans.len(),
            rj.iter()
                .take(6)
                .map(|(r, n)| format!("{r} {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if plans.is_empty() {
        let mut rj: Vec<_> = rejects.into_iter().collect();
        rj.sort_by_key(|(_, n)| -*n);
        eprintln!(
            "bazaar-finder: nothing tradeable ({})",
            rj.iter()
                .take(4)
                .map(|(r, n)| format!("{r} {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        return;
    }
    for (plan_i, bot_i) in assign_plans(&plans, &targets, capacity, &held_by_bot, t) {
        let plan = &plans[plan_i];
        let bot = &targets[bot_i].0;
        eprintln!(
            "bazaar-finder: BUY {} x{:.0} @ {:.1} (net {:.0}/u, {:.2}%, {:.0} coins/slot-h) -> {bot}",
            plan.name, plan.units, plan.unit_price, plan.net_unit, plan.margin_pct, plan.score
        );
        if !t.dry_run && fleet.send(bot, plan.to_wire()) {
            f.record_buy(plan, bot);
        }
    }
}

/// Hand out at most one plan per bot per pass, best-ranked first, returning
/// `(plan index, bot index)` pairs.
///
/// Every plan was sized against the RICHEST bot, so a poorer bot usually cannot
/// afford the top-ranked one. Walking down the ranking for each bot instead of
/// skipping the bot is what keeps the rest of the fleet working: the previous
/// version offered every bot the same single plan and moved on, so in an 11h dry
/// run the two fattest purses took 75% of all 1,383 orders and the smallest bot
/// took 6. One plan per bot per pass is deliberate — it stops a single tick from
/// emptying the purse into the bazaar.
fn assign_plans(
    plans: &[Plan],
    targets: &[(String, f64, i64)],
    capacity: i64,
    held_by_bot: &HashMap<String, i64>,
    t: &Tuning,
) -> Vec<(usize, usize)> {
    let mut taken = vec![false; plans.len()];
    let mut out = Vec::new();
    for (bot_i, (bot, purse, slots)) in targets.iter().enumerate() {
        if out.len() >= capacity.max(0) as usize {
            break;
        }
        // ⚠️ `slots_per_bot` has to be enforced PER BOT here, not just as a
        // fleet-wide capacity above. One plan per bot per pass bounds a single
        // tick, but across passes nothing stopped one bot accumulating the whole
        // fleet's allowance: observed live with `BZ_SLOTS_PER_BOT=1` and FOUR
        // positions on osd134_. That concentrates the risk and walks into
        // Hypixel's own 14-orders-per-account cap.
        let already = held_by_bot.get(bot).copied().unwrap_or(0)
            + out.iter().filter(|(_, b)| *b == bot_i).count() as i64;
        if already >= t.slots_per_bot {
            continue;
        }
        let affordable = |p: &Plan| {
            p.unit_price * p.units <= purse * t.purse_share
                // This bot specifically must have the inventory room, since the
                // plan was sized against whichever bot had the most.
                && !(is_unstackable(&p.tag, &p.name)
                    && p.units > (slots - t.unstackable_reserve).max(0) as f64)
        };
        if let Some(i) = (0..plans.len()).find(|&i| !taken[i] && affordable(&plans[i])) {
            taken[i] = true;
            out.push((i, bot_i));
        }
    }
    out
}

pub fn spawn(db_path: &str, fleet: std::sync::Arc<dyn Fleet>) {
    if !enabled() {
        return;
    }
    let path = db_path.to_string();
    std::thread::spawn(move || run(&path, fleet));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Several tests hand-mutate the global `holdings()` map, and Rust runs
    /// tests in one process on several threads. Two tests clearing/reinserting
    /// the same map concurrently make each other's fixtures vanish (seen as a
    /// flaky `the_holder_is_found...` on a slower parallel box). Any test that
    /// touches `holdings()` holds this lock for its whole body.
    static HOLDINGS_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn product(ask: f64, bid: f64, flow: i64) -> BazaarProduct {
        BazaarProduct {
            buy_price: ask,
            sell_price: bid,
            buy_volume: 100_000,
            sell_volume: 100_000,
            buy_moving_week: flow * 7,
            sell_moving_week: flow * 7,
            buy_orders: 50,
            sell_orders: 50,
            top_insta_buy: ask,
            top_insta_buy_amount: 1000,
            top_insta_buy_orders: 3,
            top_insta_sell: bid,
            top_insta_sell_amount: 1000,
            top_insta_sell_orders: 3,
            // A healthy book: real supply resting at the top on both sides.
            ask_levels: vec![(ask, 1000), (ask * 1.02, 5000)],
            bid_levels: vec![(bid, 1000), (bid * 0.98, 5000)],
        }
    }

    fn plan(tag: &str, unit_price: f64, units: f64) -> Plan {
        Plan {
            tag: tag.into(),
            name: tag.into(),
            side: Side::Buy,
            unit_price,
            units,
            net_unit: 1.0,
            margin_pct: 10.0,
            score: 1.0,
        }
    }

    fn held(tag: &str, buy: f64, ask: f64, units: f64, orphan: bool) -> Position {
        Position {
            tag: tag.into(),
            name: tag.into(),
            units,
            buy_price: buy,
            placed_at_ms: 0,
            sell_issued_at_ms: Some(0),
            sell_price: Some(ask),
            bot: "b".into(),
            repriced: 0,
            baseline_units: 0.0,
            units_in_offer: units,
            units_sold: 0.0,
            confirmed_at_ms: Some(0),
            units_filled: units,
            orphan,
        }
    }

    fn with_pos(t: Tuning, pos: Position) -> BazaarFinder {
        let mut f = BazaarFinder::new("/nonexistent/bz.sqlite");
        f.tuning = t;
        f.open.lock().unwrap().insert(pos.tag.clone(), pos);
        f
    }

    fn requote_tuning() -> Tuning {
        Tuning {
            sell_requote_min: 20.0,
            sell_requote_gap: 0.01,
            sell_max_requotes: 4,
            ..Tuning::default()
        }
    }

    /// The measured prod case: ICE_HUNK offered at 4,384 into a book whose best
    /// ask is 2,588. 25 of 25 resting offers looked like this.
    #[test]
    fn an_offer_stranded_above_the_book_is_requoted() {
        let f = with_pos(
            requote_tuning(),
            held("ICE_HUNK", 2000.0, 4384.0, 695.0, false),
        );
        let mut m = HashMap::new();
        m.insert("ICE_HUNK".to_string(), product(2588.5, 2400.0, 10_000));
        let out = f.stale_sells(&m);
        assert_eq!(out.len(), 1, "a 69%-above-book offer must be requoted");
        assert!((out[0].1 - tick_down(2588.5)).abs() < 0.001);
    }

    /// ⛔ Off by default. `sell_requote_min = 0` must be byte-identical.
    #[test]
    fn requote_is_off_unless_armed() {
        let f = with_pos(Tuning::default(), held("T", 2000.0, 4384.0, 100.0, false));
        let mut m = HashMap::new();
        m.insert("T".to_string(), product(2588.5, 2400.0, 10_000));
        assert!(f.stale_sells(&m).is_empty());
    }

    /// Churning on a one-tick difference costs a GUI cycle and gains nothing.
    #[test]
    fn a_marginal_gap_is_not_worth_the_round_trip() {
        let f = with_pos(requote_tuning(), held("T", 100.0, 1000.0, 50.0, false));
        let mut m = HashMap::new();
        // front of book only 0.5% under our ask, gap threshold is 1%.
        m.insert("T".to_string(), product(995.0, 900.0, 10_000));
        assert!(f.stale_sells(&m).is_empty());
    }

    /// A real position whose book fell under cost must be HELD, not dumped.
    #[test]
    fn a_real_position_is_never_requoted_below_break_even() {
        let f = with_pos(requote_tuning(), held("T", 1000.0, 1400.0, 50.0, false));
        let mut m = HashMap::new();
        // front of book 900 is under break-even (1000 / 0.9875 = 1012.7)
        m.insert("T".to_string(), product(900.0, 800.0, 10_000));
        assert!(
            f.stale_sells(&m).is_empty(),
            "selling a real position under cost is the one thing this must not do"
        );
    }

    /// 🔥 An ORPHAN's break-even is an artefact: `adopt_orphan_sell` sets
    /// `buy_price` to the ask, so break-even is always the ask times the tax and
    /// is ALWAYS above its own offer. That is why the same orphans cycled
    /// offer -> 180min timer -> re-adopt forever. They have no basis to defend.
    #[test]
    fn an_orphan_is_exempt_from_the_manufactured_break_even() {
        let f = with_pos(requote_tuning(), held("T", 1000.0, 1000.0, 50.0, true));
        let mut m = HashMap::new();
        m.insert("T".to_string(), product(900.0, 800.0, 10_000));
        assert_eq!(
            f.stale_sells(&m).len(),
            1,
            "orphan stock must be allowed to clear"
        );
    }

    /// A book we are permanently losing must not be chased forever.
    #[test]
    fn requotes_are_capped() {
        let mut pos = held("T", 100.0, 1000.0, 50.0, false);
        pos.repriced = 4;
        let f = with_pos(requote_tuning(), pos);
        let mut m = HashMap::new();
        m.insert("T".to_string(), product(500.0, 400.0, 10_000));
        assert!(f.stale_sells(&m).is_empty());
    }

    /// ⛔ Cancelling returns the units. Counting them as sold would invent
    /// revenue and, through the ledger, invent profit.
    #[test]
    fn cancelling_an_offer_does_not_bank_it_as_sold() {
        let f = with_pos(requote_tuning(), held("T", 100.0, 1000.0, 50.0, false));
        f.record_sell_cancelled("T");
        let p = &f.positions()[0];
        assert_eq!(p.units_sold, 0.0, "cancel must never look like a sale");
        assert_eq!(p.units_in_offer, 0.0);
        assert!(p.sell_price.is_none());
        assert!(p.sell_issued_at_ms.is_none());
        assert_eq!(p.repriced, 1);
        // and the units are sellable again
        assert_eq!(p.units_filled - p.units_sold - p.units_in_offer, 50.0);
    }

    /// ⛔ Off by default: `outbid_keep_hours = 0` must cancel on any outbid,
    /// exactly as before.
    #[test]
    fn outbid_keep_is_off_by_default() {
        assert!(!keep_when_outbid(10.0, 40_000.0, 100.0, 100.1, 0.0, 0.02));
    }

    /// The user's case: jumped by 0.1 with a 20-unit order on a book trading
    /// 40,000 units an hour. That queue drains in under two seconds.
    #[test]
    fn a_thin_jump_on_a_fast_book_is_kept() {
        assert!(keep_when_outbid(20.0, 40_000.0, 100.0, 100.1, 0.5, 0.02));
    }

    /// The other half of the same case: a 71,077-unit wall (the real
    /// ENCHANTED_SPRUCE_LOG bid side) on a book doing 500/h is 142 hours deep.
    #[test]
    fn a_wall_on_a_slow_book_is_abandoned() {
        assert!(!keep_when_outbid(71_077.0, 500.0, 100.0, 100.1, 0.5, 0.02));
    }

    /// A big price gap is the market moving, not a queue-jump. Depth is
    /// irrelevant then — our price is simply stale.
    #[test]
    fn a_large_price_gap_cancels_regardless_of_depth() {
        assert!(
            !keep_when_outbid(1.0, 1e9, 100.0, 110.0, 24.0, 0.02),
            "10% above us is a moved market, not someone shaving a tick"
        );
    }

    /// A book with no flow never drains, however small the queue.
    #[test]
    fn no_flow_means_no_keep() {
        assert!(!keep_when_outbid(1.0, 0.0, 100.0, 100.1, 24.0, 0.02));
    }

    /// Only strictly-better prices count as ahead of us; our own level does not.
    #[test]
    fn units_ahead_counts_only_better_prices() {
        let levels = vec![(110.0, 50), (105.0, 30), (100.0, 900), (95.0, 7000)];
        assert_eq!(units_ahead_of(&levels, 100.0), 80.0);
        assert_eq!(units_ahead_of(&levels, 111.0), 0.0);
        // every level is better than 94: 50 + 30 + 900 + 7000
        assert_eq!(units_ahead_of(&levels, 94.0), 7980.0);
    }

    /// 🔥 A position booked under an offline bot must be sellable from whichever
    /// bot is actually holding the units. Measured on prod: 2 of 6 bazaar tags in
    /// one inventory had positions attributed to a different, offline bot.
    #[test]
    fn the_holder_is_found_regardless_of_who_we_asked_to_buy() {
        let _holdings_guard = HOLDINGS_TEST_LOCK.lock().unwrap();
        holdings().lock().unwrap().clear();
        holdings().lock().unwrap().insert(
            "zShadowReaper_".into(),
            HashMap::from([("TIGER_SHARK_TOOTH".to_string(), 11.0)]),
        );
        assert_eq!(
            bot_holding("TIGER_SHARK_TOOTH", 11.0).as_deref(),
            Some("zShadowReaper_")
        );
        // Not enough units is not a holder.
        assert_eq!(bot_holding("TIGER_SHARK_TOOTH", 12.0), None);
        // A tag nobody holds has no holder.
        assert_eq!(bot_holding("NOTHING", 1.0), None);
        holdings().lock().unwrap().clear();
    }

    /// Split across accounts: sell the biggest piece, do not dribble.
    #[test]
    fn the_largest_holder_wins() {
        let _holdings_guard = HOLDINGS_TEST_LOCK.lock().unwrap();
        holdings().lock().unwrap().clear();
        {
            let mut h = holdings().lock().unwrap();
            h.insert("small".into(), HashMap::from([("T".to_string(), 5.0)]));
            h.insert("big".into(), HashMap::from([("T".to_string(), 500.0)]));
        }
        assert_eq!(bot_holding("T", 5.0).as_deref(), Some("big"));
        holdings().lock().unwrap().clear();
    }

    /// The dry run showed the two richest bots taking 75% of every order while
    /// the smallest took 6 of 1,383, because each bot was offered exactly one
    /// plan -- the top-ranked one, sized against the richest purse -- and simply
    /// skipped when it could not afford it.
    #[test]
    fn a_poor_bot_gets_the_best_plan_it_can_actually_afford() {
        let t = Tuning {
            purse_share: 1.0,
            unstackable_reserve: 2,
            ..Tuning::default()
        };
        // Ranked expensive-first, as rank_buys returns them.
        let plans = vec![
            plan("A", 100.0, 100.0),
            plan("B", 10.0, 100.0),
            plan("C", 1.0, 100.0),
        ];
        let targets = vec![
            ("rich".into(), 10_000.0, 34),
            ("poor".into(), 1_000.0, 34),
            ("broke".into(), 100.0, 34),
        ];
        assert_eq!(
            assign_plans(&plans, &targets, 99, &HashMap::new(), &t),
            vec![(0, 0), (1, 1), (2, 2)],
            "each bot should walk down the ranking to the first plan it can take"
        );
        // A bot that cannot afford even the cheapest plan is simply passed over,
        // and must not consume a plan that a later bot could have used.
        let targets = vec![("broke".into(), 1.0, 34), ("rich".into(), 10_000.0, 34)];
        assert_eq!(
            assign_plans(&plans, &targets, 99, &HashMap::new(), &t),
            vec![(0, 1)]
        );
    }

    #[test]
    fn assignment_never_exceeds_free_slots() {
        let t = Tuning {
            purse_share: 1.0,
            ..Tuning::default()
        };
        let plans = vec![
            plan("A", 1.0, 1.0),
            plan("B", 1.0, 1.0),
            plan("C", 1.0, 1.0),
        ];
        let targets: Vec<_> = (0..3).map(|i| (format!("bot{i}"), 1e9, 34i64)).collect();
        assert_eq!(
            assign_plans(&plans, &targets, 2, &HashMap::new(), &t).len(),
            2
        );
        assert!(assign_plans(&plans, &targets, 0, &HashMap::new(), &t).is_empty());
    }

    /// One plan per bot per pass: the same plan must never be handed to two bots
    /// in one tick, or a single product eats the whole fleet's capacity at once.
    /// `slots_per_bot` bounds ONE bot, not the fleet. Live with
    /// `BZ_SLOTS_PER_BOT=1`, one bot ended up holding four positions, because a
    /// fleet-wide capacity says nothing about where the orders land.
    #[test]
    fn a_bot_at_its_slot_limit_gets_nothing_more() {
        let t = Tuning {
            purse_share: 1.0,
            slots_per_bot: 2,
            ..Tuning::default()
        };
        let plans = vec![
            plan("A", 1.0, 1.0),
            plan("B", 1.0, 1.0),
            plan("C", 1.0, 1.0),
        ];
        let targets = vec![
            ("full".to_string(), 1e9, 34i64),
            ("free".to_string(), 1e9, 34i64),
        ];

        let held = HashMap::from([("full".to_string(), 2i64)]);
        assert_eq!(
            assign_plans(&plans, &targets, 99, &held, &t),
            vec![(0, 1)],
            "the bot already at 2 gets nothing; the free one takes the best plan"
        );

        // Within one pass a bot still only takes one, so two passes are needed to
        // reach its limit -- but the limit must count what it already holds.
        let held = HashMap::from([("full".to_string(), 1i64)]);
        let got = assign_plans(&plans, &targets, 99, &held, &t);
        assert_eq!(got.len(), 2, "both bots still have room for one more");
    }

    #[test]
    fn a_plan_is_issued_to_at_most_one_bot() {
        let t = Tuning {
            purse_share: 1.0,
            ..Tuning::default()
        };
        let plans = vec![plan("A", 1.0, 1.0)];
        let targets: Vec<_> = (0..4).map(|i| (format!("bot{i}"), 1e9, 34i64)).collect();
        assert_eq!(
            assign_plans(&plans, &targets, 99, &HashMap::new(), &t),
            vec![(0, 0)]
        );
    }

    fn finder(stats: ProductStats) -> BazaarFinder {
        let mut f = BazaarFinder::new("/tmp/bz-test.sqlite");
        f.names.insert("T".into(), "Test Item".into());
        f.stats.insert("T".into(), stats);
        f
    }

    fn healthy() -> ProductStats {
        ProductStats {
            samples: 1440,
            stability: 0.99,
            median_margin_pct: 10.0,
            median_ask_bid_ratio: 1.12,
            undercut_per_hour: 8.0,
            vol_1h_pct: 0.5,
            // Anchored at the same book the `product()` fixture quotes, so the
            // manipulation guard is inert unless a test moves one of them.
            recent_bid_median: 1000.0,
            recent_ask_median: 1120.0,
        }
    }

    #[test]
    fn prices_strictly_inside_the_book() {
        let f = finder(healthy());
        let plan = f
            .evaluate_buy("T", &product(1120.0, 1000.0, 10_000), 10_000_000.0, 34)
            .unwrap();
        assert!(plan.unit_price > 1000.0, "buy order must beat the best bid");
        assert!(
            plan.unit_price < 1000.2,
            "and only just: {}",
            plan.unit_price
        );
        assert_eq!(plan.side, Side::Buy);
    }

    #[test]
    fn blacklisted_products_are_refused_however_good_the_book_looks() {
        // FLAWED_ONYX_GEM filled 0 of 138 real placements. Its book still reads
        // healthy on every shape metric we have, which is the whole point: the
        // blacklist carries information the book does not.
        let mut f = finder(healthy());
        f.tuning.blacklist.insert("T".to_string());
        assert_eq!(
            f.evaluate_buy("T", &product(1120.0, 1000.0, 10_000), 1e9, 34),
            Err("blacklisted (does not fill)")
        );
        // And it must fire ahead of the book gates, so the reason is the true
        // one rather than whichever gate the product incidentally trips.
        let mut st = healthy();
        st.median_ask_bid_ratio = 2.28;
        let mut f = finder(st);
        f.tuning.blacklist.insert("T".to_string());
        assert_eq!(
            f.evaluate_buy("T", &product(2280.0, 1000.0, 10_000), 1e9, 34),
            Err("blacklisted (does not fill)")
        );
    }

    #[test]
    fn a_pumped_ask_is_refused_and_then_stays_refused() {
        // The ask is 25% above where it has been for the last half hour. The
        // spread alone would not give this away, which is why the guard reads
        // each side against its own anchor.
        let f = finder(healthy());
        assert_eq!(
            f.evaluate_buy("T", &product(1400.0, 1000.0, 10_000), 1e9, 34),
            Err("price moved too far, too fast")
        );
        // THE POINT: the book now reads perfectly normal again, and we still
        // refuse it, because one anomalous print starts a cooldown. Without
        // this, a pump that flickers gets an order in on a quiet pass.
        assert_eq!(
            f.evaluate_buy("T", &product(1120.0, 1000.0, 10_000), 1e9, 34),
            Err("quarantined (recent price manipulation)")
        );
    }

    #[test]
    fn a_pumped_bid_is_refused_too() {
        // The other direction: paying up for inventory whose real value is lower.
        let f = finder(healthy());
        assert_eq!(
            f.evaluate_buy("T", &product(1120.0, 1250.0, 10_000), 1e9, 34),
            Err("price moved too far, too fast")
        );
    }

    #[test]
    fn an_ordinary_wobble_is_not_manipulation() {
        // 3% off the anchor, well inside the 10% trigger: must trade normally.
        let f = finder(healthy());
        assert!(f
            .evaluate_buy("T", &product(1120.0, 1030.0, 10_000), 1e9, 34)
            .is_ok());
    }

    #[test]
    fn a_missing_anchor_is_not_a_trigger() {
        // A product with no sample in the recent window has a NaN anchor. That
        // is an absence of evidence, and must not read as a 100% move.
        let mut st = healthy();
        st.recent_bid_median = f64::NAN;
        st.recent_ask_median = f64::NAN;
        let f = finder(st);
        assert!(f
            .evaluate_buy("T", &product(1120.0, 1000.0, 10_000), 1e9, 34)
            .is_ok());
    }

    #[test]
    fn an_empty_blacklist_changes_nothing() {
        // The default path: no BZ_FILTER_FILE means the gate is inert.
        let f = finder(healthy());
        assert!(f.tuning.blacklist.is_empty());
        assert!(f
            .evaluate_buy("T", &product(1120.0, 1000.0, 10_000), 10_000_000.0, 34)
            .is_ok());
    }

    #[test]
    fn wide_dead_books_are_rejected() {
        // The MELON/SPIKED_BAIT shape: a spread so wide it has plainly never
        // been traded through.
        let mut st = healthy();
        st.median_ask_bid_ratio = 2.28;
        let f = finder(st);
        // Live book deliberately TIGHT, so this isolates the historical gate:
        // a product that has been 2.28x for 25 days is dead even in a moment
        // when its top of book happens to look reasonable.
        assert_eq!(
            f.evaluate_buy("T", &product(1120.0, 1000.0, 10_000), 1e9, 34),
            Err("book too wide to be real")
        );
    }

    #[test]
    fn over_farmed_books_are_rejected() {
        // BOOSTER_COOKIE: 0.35% margin, outbid ~44 times an hour.
        let mut st = healthy();
        st.undercut_per_hour = 44.3;
        let f = finder(st);
        assert_eq!(
            f.evaluate_buy("T", &product(1120.0, 1000.0, 10_000), 1e9, 34),
            Err("outbid too often")
        );
    }

    #[test]
    fn unstable_spreads_are_rejected() {
        let mut st = healthy();
        st.stability = 0.82;
        let f = finder(st);
        assert_eq!(
            f.evaluate_buy("T", &product(1120.0, 1000.0, 10_000), 1e9, 34),
            Err("spread not persistent")
        );
    }

    #[test]
    fn one_sided_flow_is_rejected() {
        let f = finder(healthy());
        let mut p = product(1120.0, 1000.0, 10_000);
        p.sell_moving_week = 7; // 1/day: nobody sells into our buy order
        assert_eq!(
            f.evaluate_buy("T", &p, 1e9, 34),
            Err("flow too thin on one side")
        );
    }

    #[test]
    fn a_hole_in_the_sell_side_is_refused_on_live_width() {
        // Hot Potato Book as seen live: ask 85,379 against bid 35,372, showing
        // 138%. Its own 24h history said 7.4%, and the original gate caught it
        // by comparing the two.
        //
        // ⚠️ It is caught LIVE instead, on width, and the distinction matters:
        // a "gap under the ask" cannot be read from the book at all, because
        // asks are sorted ascending — the top ask IS the cheapest offer, so
        // there is by definition nothing below it to find. What made HPB fake
        // was not a hole in the book but that new sellers would undercut back
        // to ~38,500 within minutes. The book cannot show future supply; the
        // 2.41x width it produces right now is visible immediately.
        let mut st = healthy();
        st.median_margin_pct = 7.36;
        st.median_ask_bid_ratio = 1.09;
        st.recent_bid_median = 35372.0;
        st.recent_ask_median = 85379.0;
        let f = finder(st);
        let mut p = product(85379.0, 35372.0, 10_000);
        p.ask_levels = vec![(85379.0, 5), (90000.0, 4000)];
        assert_eq!(
            f.evaluate_buy("T", &p, 1e9, 34),
            Err("book too wide right now")
        );
    }

    #[test]
    fn a_wide_live_book_is_refused_even_when_history_looks_tight() {
        // ENCHANTED_SPRUCE_LOG: ask 1,754 / bid 644 = 2.7x, with 5,313 units at
        // the ask and 71,077 at the bid. Deep on BOTH sides, so no depth test
        // can see it; the 24h median ratio was ~1.25 because the regime changed
        // mid-window, so the historical gate could not see it either. Width,
        // measured live, is the only thing that catches this.
        let mut st = healthy();
        st.median_ask_bid_ratio = 1.25; // history says tight...
        st.median_margin_pct = 22.9;
        st.recent_bid_median = 644.6; // ...and it is not a recent move either
        st.recent_ask_median = 1754.3;
        let f = finder(st);
        let mut p = product(1754.3, 644.6, 200_000);
        p.ask_levels = vec![(1754.3, 5313)];
        p.bid_levels = vec![(644.6, 71_077)];
        assert_eq!(
            f.evaluate_buy("T", &p, 1e9, 34),
            Err("book too wide right now")
        );
    }

    #[test]
    fn the_live_width_gate_bounds_the_margin_structurally() {
        // With the live ratio capped at 1.6, nothing above ~58% after tax can
        // reach the ranking — which is what makes a history-relative margin cap
        // unnecessary rather than merely absent.
        let mut st = healthy();
        st.recent_ask_median = 1599.0; // not a recent move, just a wide-ish book
        let f = finder(st);
        let mut p = product(1599.0, 1000.0, 100_000);
        p.ask_levels = vec![(1599.0, 50_000)];
        let plan = f.evaluate_buy("T", &p, 1e12, 34).unwrap();
        assert!(plan.margin_pct < 60.0, "got {:.1}%", plan.margin_pct);
    }

    #[test]
    fn a_book_too_thin_to_absorb_our_own_size_is_refused() {
        // Nothing behind the top at all: there is no price we can rely on
        // getting out at, so decline rather than guess one.
        let f = finder(healthy());
        let mut p = product(1120.0, 1000.0, 10_000);
        p.ask_levels = vec![(1120.0, 3)];
        assert_eq!(
            f.evaluate_buy("T", &p, 1e9, 34),
            Err("sell side too thin for this size")
        );
    }

    #[test]
    fn real_depth_is_banked_in_full_even_when_history_disagrees() {
        // THE POINT of reading the live book: a spread far richer than this
        // product's own history, but backed by genuine resting supply, is a real
        // opportunity. The old model capped earnings at the 24h median and threw
        // the rest away; history is a reference, not the decision.
        let mut st = healthy();
        st.median_margin_pct = 8.0; // history says 8%...
        let f = finder(st);
        let mut p = product(1120.0, 1000.0, 10_000);
        p.ask_levels = vec![(1120.0, 9000)]; // ...but 9000 units rest at the ask
        let plan = f.evaluate_buy("T", &p, 1e9, 34).unwrap();
        assert!(
            plan.margin_pct > 9.0,
            "a depth-backed spread must not be capped at the median, got {:.1}%",
            plan.margin_pct
        );
    }

    #[test]
    fn unstackable_orders_are_capped_to_inventory_slots() {
        // Enchantment books cost one inventory slot per unit, and the mod cuts
        // the order to `empty - 2` and drops it entirely at zero. Sizing past
        // that inflates coins-per-slot-hour for a third of the bazaar.
        let mut f = finder(healthy());
        f.names
            .insert("ENCHANTMENT_SHARPNESS_6".into(), "Enchanted Book".into());
        f.stats.insert("ENCHANTMENT_SHARPNESS_6".into(), healthy());
        let p = product(1120.0, 1000.0, 10_000);
        let plan = f
            .evaluate_buy("ENCHANTMENT_SHARPNESS_6", &p, 1e12, 12)
            .unwrap();
        assert_eq!(
            plan.units, 10.0,
            "12 free slots less the mod's reserve of 2"
        );

        // A stackable product of identical shape is bound by flow instead: 10
        // free slots hold 640 units, well above the 200 that flow allows.
        let plan = f.evaluate_buy("T", &p, 1e12, 12).unwrap();
        assert_eq!(plan.units, 200.0);

        // No room at all: refuse rather than send something the mod will drop.
        assert_eq!(
            f.evaluate_buy("ENCHANTMENT_SHARPNESS_6", &p, 1e12, 2),
            Err("no inventory slot for this buy")
        );
    }

    /// Stackable units also have to be claimed into the inventory before they
    /// can be offered back, and the mod caps only the unstackable case. Going
    /// live at a 2M coin cap made this bite: it bought 2,797 units of a
    /// 715-coin material, which is 44 slots of a 36-slot inventory.
    #[test]
    fn stackable_orders_are_capped_to_inventory_slots_too() {
        let f = finder(healthy());
        // Flow is the only other binding cap here, so lift it clear.
        let p = product(1120.0, 1000.0, 10_000_000);
        // 12 free slots less the reserve of 2 = 10 slots = 640 units.
        assert_eq!(f.evaluate_buy("T", &p, 1e12, 12).unwrap().units, 640.0);
        // 34 free slots less 2 = 32 slots = 2048 units.
        assert_eq!(f.evaluate_buy("T", &p, 1e12, 34).unwrap().units, 2048.0);
        // A bot with nothing free gets no order at all.
        assert_eq!(
            f.evaluate_buy("T", &p, 1e12, 2),
            Err("no inventory slot for this buy")
        );
    }

    #[test]
    fn size_is_capped_by_flow_not_just_purse() {
        let f = finder(healthy());
        // 10k units/day of flow, 2% share => 200 units, well under the budget.
        let plan = f
            .evaluate_buy("T", &product(1120.0, 1000.0, 10_000), 1e12, 34)
            .unwrap();
        assert_eq!(plan.units, 200.0);
    }

    /// The inventory upload is a total per tag with no provenance. One stray
    /// unit the bot already had made the finder sell x1 and then close a
    /// 1,083-unit position as fully sold, while its 1,999,110-coin buy order was
    /// still resting on the book.
    #[test]
    fn only_units_above_the_baseline_count_as_our_fill() {
        let _holdings_guard = HOLDINGS_TEST_LOCK.lock().unwrap();
        holdings()
            .lock()
            .unwrap()
            .insert("bot".into(), HashMap::from([("ICE_HUNK".to_string(), 1.0)]));
        // The bot already had that one unit when we ordered, so none of it is ours.
        assert_eq!(our_units("bot", "ICE_HUNK", 1.0), Some(0.0));
        // Now 501 are in hand: 500 of them are the fill.
        holdings().lock().unwrap().insert(
            "bot".into(),
            HashMap::from([("ICE_HUNK".to_string(), 501.0)]),
        );
        assert_eq!(our_units("bot", "ICE_HUNK", 1.0), Some(500.0));
        // A bot that has sold MORE than the baseline elsewhere never goes negative.
        assert_eq!(our_units("bot", "ICE_HUNK", 9999.0), Some(0.0));
        // No upload at all is unknown, not empty: it must not trigger a sell.
        assert_eq!(our_units("never-reported", "ICE_HUNK", 0.0), None);
    }

    /// A buy order fills in pieces, so the position is only finished once the
    /// pieces add up. Settling must not close a position whose buy order is
    /// still working.
    #[test]
    fn a_partially_sold_position_stays_open() {
        let f = finder(healthy());
        // Isolate from the shared state file new() restores from.
        f.open.lock().unwrap().clear();
        let plan = plan("ICE_HUNK", 1845.9, 1083.0);
        f.record_buy(&plan, "partial-fill-bot");

        f.record_sell_issued("ICE_HUNK", 2805.6, 500.0);
        let p = f.positions().into_iter().next().unwrap();
        assert_eq!(p.units_in_offer, 500.0);
        assert!(p.sell_issued_at_ms.is_some());

        f.record_sell_settled("ICE_HUNK");
        let p = f.positions().into_iter().next().unwrap();
        assert_eq!(p.units_sold, 500.0, "banked");
        assert_eq!(p.units_in_offer, 0.0);
        assert!(
            p.sell_issued_at_ms.is_none(),
            "free to offer the next piece"
        );
        assert!(
            p.units_sold < p.units,
            "578 units of the buy order still to come"
        );

        // The rest arrives and settles: now it is genuinely done.
        f.record_sell_issued("ICE_HUNK", 2805.6, 583.0);
        f.record_sell_settled("ICE_HUNK");
        let p = f.positions().into_iter().next().unwrap();
        assert_eq!(p.units_sold, 1083.0);
    }

    /// Every string here is copied verbatim out of the bots' own logs.
    #[test]
    fn hypixel_bazaar_chat_parses() {
        use BazaarChat::*;
        let cases = [
            (
                "[Bazaar] Buy Order Setup! 1,083x Hunk of Ice for 1,999,110 coins.",
                Some(BuyPlaced {
                    units: 1083.0,
                    name: "Hunk of Ice".into(),
                    coins: Some(1_999_110.0),
                }),
            ),
            (
                "[Bazaar] Sell Offer Setup! 15x Jelly for 1,000 coins.",
                Some(SellPlaced {
                    units: 15.0,
                    name: "Jelly".into(),
                    coins: Some(1000.0),
                }),
            ),
            (
                "[Bazaar] Your Buy Order for 64x Coal was filled!",
                Some(BuyFilled {
                    units: 64.0,
                    name: "Coal".into(),
                }),
            ),
            (
                "[Bazaar] Your Sell Offer for 64x Coal was filled!",
                Some(SellFilled {
                    units: 64.0,
                    name: "Coal".into(),
                }),
            ),
            (
                "[Bazaar] Your price isn't competitive enough with the best order/offer's price.",
                Some(NotCompetitive),
            ),
            (
                "[Bazaar] You reached your maximum of 14 Bazaar orders!",
                Some(MaxOrders { limit: 14 }),
            ),
            // A doubled space in the name is real: `Nx  Flawed Onyx Gemstone`.
            (
                "[Bazaar] Sell Offer Setup! 5x  Flawed Onyx Gemstone for 500 coins.",
                Some(SellPlaced {
                    units: 5.0,
                    name: "Flawed Onyx Gemstone".into(),
                    coins: Some(500.0),
                }),
            ),
            // Noise the finder must ignore rather than misread.
            ("[Bazaar] Putting goods in escrow...", None),
            ("[Bazaar] Claiming order...", None),
            (
                "[Bazaar] Cancelled! Refunded 1,000 coins from cancelling Buy Order!",
                None,
            ),
            ("some unrelated chat line", None),
        ];
        for (line, want) in cases {
            assert_eq!(parse_bazaar_chat(line), want, "parsing {line:?}");
        }
    }

    /// A position only becomes real when Hypixel says so, and only ends when
    /// Hypixel says the SELL filled. Inference off inventory got both wrong.
    #[test]
    fn chat_confirms_and_closes_a_position() {
        let f = finder(healthy());
        // Isolate from the shared state file new() restores from.
        f.open.lock().unwrap().clear();
        f.record_buy(&plan("ICE_HUNK", 1845.9, 1083.0), "b1");
        // Renamed to the display name the chat will use.
        {
            let mut open = f.open.lock().unwrap();
            open.get_mut("ICE_HUNK").unwrap().name = "Hunk of Ice".into();
        }
        assert!(
            f.positions()[0].confirmed_at_ms.is_none(),
            "a hope, not an order"
        );

        f.apply_chat(
            "b1",
            &BazaarChat::BuyPlaced {
                units: 1083.0,
                name: "Hunk of Ice".into(),
                coins: Some(1_999_110.0),
            },
        );
        assert!(
            f.positions()[0].confirmed_at_ms.is_some(),
            "now it is on the book"
        );

        // A partial sell fill leaves it open with the rest still to go.
        f.apply_chat(
            "b1",
            &BazaarChat::SellFilled {
                units: 83.0,
                name: "Hunk of Ice".into(),
            },
        );
        assert_eq!(f.positions()[0].units_sold, 83.0);

        f.apply_chat(
            "b1",
            &BazaarChat::SellFilled {
                units: 1000.0,
                name: "Hunk of Ice".into(),
            },
        );
        assert!(
            f.positions().is_empty(),
            "closed on the confirmed sell, not on a timer"
        );
    }

    /// Hypixel refused 939 of our orders as "not competitive" in the logs. Each
    /// one leaves a position the finder believes in and the game never had.
    #[test]
    fn a_refused_order_is_dropped_not_tracked() {
        let f = finder(healthy());
        // Isolate from the shared state file new() restores from.
        f.open.lock().unwrap().clear();
        f.record_buy(&plan("A", 100.0, 10.0), "b1");
        f.record_buy(&plan("B", 100.0, 10.0), "b1");
        f.apply_chat(
            "b1",
            &BazaarChat::BuyPlaced {
                units: 10.0,
                name: "A".into(),
                coins: Some(1000.0),
            },
        );

        f.apply_chat("b1", &BazaarChat::NotCompetitive);
        let left: Vec<String> = f.positions().into_iter().map(|p| p.tag).collect();
        assert_eq!(
            left,
            vec!["A".to_string()],
            "only the unconfirmed one is dropped"
        );

        // Another bot's refusal must not touch this bot's position.
        f.apply_chat("b2", &BazaarChat::NotCompetitive);
        assert_eq!(f.positions().len(), 1);
    }

    /// 🔥 The "collects stuff but never sells it" bug, 2026-08-15. An order that
    /// lands after we gave up on it is a REAL order with our coins in escrow.
    /// The finder used to answer it with a log line and nothing else, so the
    /// units arrived and no sell offer was ever made for them.
    #[test]
    fn a_late_confirmation_is_adopted_not_lost() {
        let f = finder(healthy());
        f.open.lock().unwrap().clear();
        assert!(
            f.positions().is_empty(),
            "nothing tracked, as after a PHANTOM drop"
        );

        f.apply_chat(
            "b1",
            &BazaarChat::BuyPlaced {
                units: 605.0,
                name: "Tasty Cheese".into(),
                coins: Some(499_972.0),
            },
        );

        let p = f.positions();
        assert_eq!(
            p.len(),
            1,
            "the order exists on the book, so it must exist here"
        );
        assert_eq!(p[0].units, 605.0);
        assert_eq!(p[0].bot, "b1");
        assert!(p[0].confirmed_at_ms.is_some());
        // Cost basis reconstructed from the chat total: 499,972 / 605.
        assert!(
            (p[0].buy_price - 826.4).abs() < 0.1,
            "real basis, not a guess"
        );
    }

    /// ⛔ Never adopt without a price. Every sell decision divides by
    /// `buy_price`, so a zero basis makes any ask look infinitely profitable.
    #[test]
    fn adoption_refuses_a_zero_cost_basis() {
        let f = finder(healthy());
        f.open.lock().unwrap().clear();
        f.apply_chat(
            "b1",
            &BazaarChat::BuyPlaced {
                units: 10.0,
                name: "X".into(),
                coins: None,
            },
        );
        assert!(
            f.positions().is_empty(),
            "better untracked than priced at zero"
        );
    }

    /// A phantom means no order, no escrow and no coins moved, so re-sending
    /// cannot duplicate anything. Barring the product for the full 30-minute
    /// rebuy cooldown is what turned 103 buy decisions into 27 live orders.
    #[test]
    fn a_phantom_retries_before_it_benches() {
        let f = finder(healthy());
        f.open.lock().unwrap().clear();
        let strikes: Vec<(i64, bool)> = (0..3).map(|_| f.close_phantom("T")).collect();
        assert_eq!(
            strikes,
            vec![(1, true), (2, true), (3, false)],
            "retry the flaky bot twice, then accept the item cannot be placed"
        );
        // And a product that does place clears the record, so a day of healthy
        // trading cannot accumulate strikes until it bans itself.
        f.apply_chat(
            "b1",
            &BazaarChat::BuyPlaced {
                units: 1.0,
                name: "T".into(),
                coins: Some(10.0),
            },
        );
        assert_eq!(
            f.close_phantom("T").0,
            1,
            "count restarts after a confirmation"
        );
    }

    /// 🔥 The deadlock of 2026-08-15. Orphaned stock filled the inventories, the
    /// mod refuses every buy at ≤4 free slots (silently, at `debug!`), and the
    /// whole finder went quiet with nothing in any log to say why.
    #[test]
    fn the_sweep_sells_the_biggest_orphan() {
        let mut f = finder(healthy());
        f.open.lock().unwrap().clear();
        f.names.insert("CHEESE".into(), "Tasty Cheese".into());
        f.names.insert("BONE".into(), "Enchanted Bone".into());
        let mut products = HashMap::new();
        products.insert("CHEESE".to_string(), product(1120.0, 1000.0, 10_000));
        products.insert("BONE".to_string(), product(1120.0, 1000.0, 10_000));

        let held = vec![("CHEESE".to_string(), 1072.0), ("BONE".to_string(), 100.0)];
        let (plan, _) = f
            .best_orphan(&products, &Default::default(), &held)
            .expect("stock worth selling");
        assert_eq!(plan.tag, "CHEESE", "the biggest pile frees the most slots");
        assert_eq!(plan.side, Side::Sell);
        // Top of the sell book, NOT the bid -- an orphan is sold, not dumped.
        assert!(
            plan.unit_price > 1000.0,
            "priced off the ask, not the 1000.0 bid"
        );
    }

    /// ⛔ The inventory also holds things the bot needs. Selling a bot's own
    /// booster cookie is a real loss, not a tidy-up.
    #[test]
    fn the_sweep_leaves_the_bots_own_things_alone() {
        let mut f = finder(healthy());
        f.open.lock().unwrap().clear();
        for (t, n) in [
            ("BOOSTER_COOKIE", "Booster Cookie"),
            ("CHEESE", "Tasty Cheese"),
            ("RARE", "Rare Gear"),
        ] {
            f.names.insert(t.into(), n.into());
        }
        let mut products = HashMap::new();
        for t in ["BOOSTER_COOKIE", "CHEESE", "RARE"] {
            products.insert(t.to_string(), product(1120.0, 1000.0, 10_000));
        }

        let tracked = ["CHEESE".to_string()].into_iter().collect();
        let held = vec![
            ("BOOSTER_COOKIE".to_string(), 40.0), // consumed by the bot itself
            ("CHEESE".to_string(), 1072.0),       // already a tracked position
            ("RARE".to_string(), 1.0),            // x1 reads as equipment
        ];
        assert!(
            f.best_orphan(&products, &tracked, &held).is_none(),
            "excluded, tracked and single-unit stock are all off limits"
        );
    }

    /// 🔥 The root cause of the phantoms. A warping bot accepts the order, opens
    /// the bazaar, clicks the item, and the server swap eats the next window.
    #[test]
    fn chat_reveals_a_bot_in_transit() {
        for l in [
            "Warping...",
            "Sending to server mini105DA...",
            "[MVP+] Da_Lehrer joined the lobby!",
            "You are already playing SkyBlock!",
        ] {
            assert!(is_transit_line(l), "{l} means the bot cannot drive a GUI");
        }
        for l in [
            "Buy Order Setup! 605x Tasty Cheese for 499,972 coins.",
            "Your Buy Order for 64x Coal was filled!",
        ] {
            assert!(!is_transit_line(l), "{l} is normal trading");
        }
    }

    /// Live 2026-08-14: four duplicate 2.0M Locust Larva buy orders inside a
    /// minute, 7.9M on one thin product, because each wrong close freed the
    /// `already holding` gate immediately.
    #[test]
    fn a_closed_product_cannot_be_re_bought_immediately() {
        let f = finder(healthy());
        f.open.lock().unwrap().clear();
        let p = product(1120.0, 1000.0, 10_000);
        assert!(
            f.evaluate_buy("T", &p, 1e12, 34).is_ok(),
            "buyable to start with"
        );

        f.record_buy(&plan("T", 1000.0, 10.0), "b1");
        f.close("T");
        let (plans, rejects) = f.rank_buys(&HashMap::from([("T".to_string(), p)]), 1e12, 34);
        assert!(plans.is_empty(), "must not re-buy what we just closed");
        assert_eq!(rejects.get("closed too recently"), Some(&1));
    }

    /// The mod points its socket at COFL or at the finder, never both. An order
    /// placed by a bot on COFL's socket can never be confirmed to us, and that
    /// blindness is what let 17 phantom orders through on 2026-08-14.
    #[test]
    fn a_bot_we_have_never_heard_from_is_not_tradeable() {
        assert!(
            !bot_is_audible("silent-bot"),
            "fail closed until proven otherwise"
        );
        note_chat("silent-bot", &["hello".to_string()]);
        // note_chat is a no-op unless BZ_FINDER=1, so drive the set directly to
        // pin the rule rather than the env.
        chat_bots().lock().unwrap().insert("talkative-bot".into());
        assert!(bot_is_audible("talkative-bot"));
        assert!(!bot_is_audible("another-silent-one"));
    }

    #[test]
    fn sell_never_goes_below_break_even() {
        let f = finder(healthy());
        let pos = Position {
            tag: "T".into(),
            name: "Test Item".into(),
            units: 10.0,
            buy_price: 1000.0,
            placed_at_ms: 0,
            sell_issued_at_ms: None,
            sell_price: None,
            bot: String::new(),
            repriced: 0,
            baseline_units: 0.0,
            units_in_offer: 0.0,
            units_sold: 0.0,
            confirmed_at_ms: None,
            units_filled: 10.0,
            orphan: false,
        };
        // Ask collapsed to just above what we paid: after 1.25% tax that is a
        // loss, so it must not be sold.
        assert_eq!(
            f.evaluate_sell(&pos, &product(1005.0, 900.0, 10_000)),
            Err("ask below break-even")
        );
        let plan = f
            .evaluate_sell(&pos, &product(1120.0, 1000.0, 10_000))
            .unwrap();
        assert!(plan.net_unit > 0.0);
        assert_eq!(plan.units, 10.0);
    }

    #[test]
    fn wire_format_matches_what_the_mod_parses() {
        let f = finder(healthy());
        let plan = f
            .evaluate_buy("T", &product(1120.0, 1000.0, 10_000), 10_000_000.0, 34)
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&plan.to_wire()).unwrap();
        assert_eq!(v["type"], "bazaarFlip");
        // `data` is a STRING holding the payload: the mod's WebSocketMessage
        // declares it as one, and a nested object there fails to deserialise.
        let inner: serde_json::Value = serde_json::from_str(v["data"].as_str().unwrap()).unwrap();
        assert_eq!(inner["itemName"], "Test Item");
        assert_eq!(inner["isBuyOrder"], true);
        assert_eq!(inner["isSell"], false);
        assert!(inner["amount"].as_i64().unwrap() > 0);
        // ⛔ No itemTag. With one, the mod searches `/bz <TAG>`, Hypixel returns
        // an empty grid and the bot drops the order. See `to_wire`.
        assert!(
            inner.get("itemTag").is_none(),
            "itemTag must not be sent on a bazaarFlip"
        );

        // The cancel path is different: it matches the open order by name inside
        // the orders GUI and never runs a `/bz` search, so the tag is harmless
        // there and is kept for disambiguation.
        let c: serde_json::Value = serde_json::from_str(&plan.to_cancel_wire()).unwrap();
        assert_eq!(c["type"], "cancelOrder");
        let cin: serde_json::Value = serde_json::from_str(c["data"].as_str().unwrap()).unwrap();
        assert_eq!(cin["itemName"], "Test Item");
    }
}
