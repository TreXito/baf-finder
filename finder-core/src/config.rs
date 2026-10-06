//! Port of the config.ts tunables the money core reads.
//!
//! These MUST be read from env at runtime, not baked in as consts. Prod
//! overrides most of them (MIN_PROFIT/MIN_CONFIDENCE/
//! CRAFT_CEILING_MULT/REF_MAX_RAM/RETENTION_DAYS...), so a compile-time
//! default silently runs DIFFERENT money settings than the deployed TS finder
//! even though the logic itself is a verbatim port. The defaults below are the
//! TS defaults, so an empty env reproduces the golden fixtures exactly.

use std::sync::LazyLock;

/// JS `Number(string)` coercion, quirks included. Returns NaN where JS does.
/// Only finite results matter to `num` (non-finite falls back to `def`), but
/// the coercion is mirrored faithfully because `Number("") === 0` is a REAL
/// difference: `MIN_PROFIT=` (set-but-empty) means 0 in TS, not the default.
fn js_number(s: &str) -> f64 {
    let t = s.trim(); // JS trims whitespace before coercing
    if t.is_empty() {
        return 0.0; // Number("") === 0
    }
    let (neg, body) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    // Radix literals: JS accepts 0x/0o/0b in Number(), but NOT with a sign.
    let radix = body
        .strip_prefix("0x")
        .or_else(|| body.strip_prefix("0X"))
        .map(|d| (d, 16))
        .or_else(|| {
            body.strip_prefix("0o")
                .or_else(|| body.strip_prefix("0O"))
                .map(|d| (d, 8))
        })
        .or_else(|| {
            body.strip_prefix("0b")
                .or_else(|| body.strip_prefix("0B"))
                .map(|d| (d, 2))
        });
    if let Some((digits, r)) = radix {
        if neg || t.starts_with('+') {
            return f64::NAN;
        }
        return u128::from_str_radix(digits, r)
            .map(|v| v as f64)
            .unwrap_or(f64::NAN);
    }
    if body == "Infinity" {
        return if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    // Rust's f64 parser accepts "inf"/"infinity"/"nan" (case-insensitive);
    // JS's Number() does not. Reject them so the coercion matches.
    let lower = body.to_ascii_lowercase();
    if lower.contains("inf") || lower.contains("nan") {
        return f64::NAN;
    }
    match body.parse::<f64>() {
        Ok(v) => {
            if neg {
                -v
            } else {
                v
            }
        }
        Err(_) => f64::NAN,
    }
}

/// Mirror of config.ts `num(name, def)`: unset => def, non-finite => def.
fn num(name: &str, def: f64) -> f64 {
    match std::env::var(name) {
        Ok(v) => {
            let n = js_number(&v);
            if n.is_finite() {
                n
            } else {
                def
            }
        }
        Err(_) => def,
    }
}

// priceIndex
pub static MIN_REFS: LazyLock<usize> = LazyLock::new(|| num("MIN_REFS", 5.0) as usize);
pub static REF_MAX_AGE_DAYS: LazyLock<f64> = LazyLock::new(|| num("REF_MAX_AGE_DAYS", 7.0));
pub static REF_CAP: LazyLock<usize> = LazyLock::new(|| num("REF_CAP", 80.0) as usize);
pub static SHORT_TERM_HOURS: LazyLock<f64> = LazyLock::new(|| num("SHORT_TERM_HOURS", 12.0));

/// Widen the short-term window (up to this many hours) on keys too illiquid to
/// put 3 sales inside `SHORT_TERM_HOURS`.
///
/// `compute_price_for` needs 3 recent sales to form `short_term`. Below that it
/// falls back to `long_term`, which pins `trend_pct` at 0 and disables both the
/// `min(short_term, long_term)` target and the `trend_pct <= -0.15` falling
/// reject. A key selling under ~6/day therefore quotes a `REF_MAX_AGE_DAYS`-old
/// median with no recency correction at all — and at 21 days that is three weeks
/// stale on precisely the thin, high-value keys where a crash is unsurvivable.
///
/// 0 or anything <= `SHORT_TERM_HOURS` (the default) keeps the fixed window, so
/// the price-index goldens are untouched. 48 is the prod setting: it tries 12h,
/// then 24h, then 48h, and takes the first that holds 3 sales.
///
/// This can only LOWER the target (`target = short_term.min(long_term)`), so it
/// costs flips and never causes an overpay.
pub static SHORT_TERM_WIDEN_HOURS: LazyLock<f64> =
    LazyLock::new(|| num("SHORT_TERM_WIDEN_HOURS", 0.0));

/// Still send a listing price for a held item whose estimate fails the
/// confidence gate, provided we know what we paid for it.
///
/// `minConfidence` asks "am I sure enough this is worth X to spend coins on it",
/// which is a BUY-side question. On the sell side the alternative to listing at
/// an uncertain price is holding forever and realising nothing, so the same gate
/// strands inventory. Measured over 21,466 inventory cycles: 64.3% of real held
/// items were dropped on confidence and only **1.2%** were ever priced at all.
///
/// Requires a known cost basis, which is what makes it safe: `price_inventory`
/// floors the listing at `paid * 1.05` and only relaxes that after days in
/// clearance (`FLOOR_RELIEF_PER_DAY`, capped by `MAX_LOSS_PCT`), so a bad
/// estimate cannot dump an item cheap. The worst case is listing too high and
/// not selling — exactly what skipping already guarantees.
pub static LIST_LOW_CONF: LazyLock<bool> = LazyLock::new(|| num("LIST_LOW_CONF", 0.0) != 0.0);

/// Item ids whose `display.Lore` carries a `Current weight: N lb` line that
/// drives the price, comma separated. Empty (the default) disables the scan
/// entirely, so the nbt goldens and the hot path are untouched.
///
/// `decode_item_bytes` reads `i[0].tag.ExtraAttributes` and nothing else, so a
/// value rendered only into the tooltip is invisible to pricing. LOUDMOUTH_BASS
/// is the case that found this: all 49,311 sales in 21 days pool into ONE key at
/// a 1.20M median, while live asks run 1.69M at weight 1 to 970M at weight 200+.
/// We can only ever MISS those, never overpay, because anything above 1.2M looks
/// expensive to us.
///
/// ⚠️ Kept to an allowlist deliberately. Scanning lore on every item would tax a
/// decode path that is 11us/auction and gates FIRSTFLIPMS roughly 1:1; matching
/// the id first costs one string compare.
pub static LORE_WEIGHT_ITEMS: LazyLock<Vec<String>> = LazyLock::new(|| {
    std::env::var("LORE_WEIGHT_ITEMS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
});

/// Weight below which no key fork happens at all.
///
/// ⚠️ This threshold is what stops the fix from being a regression. Historical
/// `sold` rows store PARSED attrs, not raw NBT, so no existing reference carries
/// a weight and none can be backfilled — the lore is gone. If every weight
/// forked the key, the ordinary weight-1 bass (75% of live listings, and the
/// bread-and-butter flip) would lose its entire reference history overnight and
/// go `notpriceable`.
///
/// Keeping light items on the unchanged key preserves that pool exactly, while
/// the heavy tail forks off into new keys that fill from new sales over a day or
/// two. Until they fill, a heavy item is `notpriceable` — no worse than today,
/// where it is mispriced at the pooled median and rejected anyway.
pub static LORE_WEIGHT_MIN: LazyLock<f64> = LazyLock::new(|| num("LORE_WEIGHT_MIN", 10.0));

/// Measure `volatility` from the interquartile range instead of the standard
/// deviation.
///
/// `volatility = stddev/median` is not robust: ONE sale can pin it at its 1.0
/// clamp, which zeroes `stability` in the confidence formula AND zeroes
/// `ref_strength` in `refine_confidence`, capping confidence at
/// `0.4*ref + 0.2*volume` = 0.6. `MIN_CONFIDENCE` is 0.85, so such a key is
/// rejected wholesale no matter how well evidenced it is.
///
/// Measured 2026-08-02: **57 of 989** items with >=200 sales in 7 days sit
/// pinned at 1.00, and drop below 0.5 once the top 2% of the pool is removed.
/// The cause is coin-transfer sales, not real dispersion — someone lists a
/// 0.04M `TREE_THE_FISH` at 420M to move coins to an alt, which is 9,333x the
/// median. `LOUDMOUTH_BASS` was the same story: two sales at 61-65M in a pool of
/// 36 took confidence from 0.955 to 0.600 while moving the median 1.36M -> 1.38M.
///
/// IQR/1.349 is the normal-consistency estimate of sigma and reuses the
/// percentiles `spread_pct` already computes. Chosen over MAD deliberately: a
/// GENUINELY bimodal pool (two real variants sharing a key, each a large share)
/// still spans the interquartile range and still reports high volatility, which
/// is correct — the median really is unreliable there. Only thin tails are
/// ignored, which is exactly the contamination we want gone.
///
/// ⚠️ Moves confidence on EVERY key, so it changes what passes `MIN_CONFIDENCE`
/// across the whole book. Default off.
pub static ROBUST_VOLATILITY: LazyLock<bool> =
    LazyLock::new(|| num("ROBUST_VOLATILITY", 0.0) != 0.0);

/// What fraction of `target` a held item OPENS at (non-model basis).
///
/// Default 1.05 is what the TS finder did and what the inventoryPricing golden
/// pins: open 5% ABOVE our own estimate and walk down. That is the wrong side of
/// the only cliff in the demand curve. Measured across 3 days of sales, price
/// relative to the item's own median:
///
/// ```text
/// listed vs median    median TTS   sold <30min
///  55%                  13 min        67%
///  80%                  11 min        71%
///  90%                  13 min        68%
///  95%                  15 min        65%
/// 100%                  22 min        56%
/// 110%                  30 min        50%
/// ```
///
/// Speed is FLAT below ~95% and falls off a cliff at par, so 0.95 is the whole
/// bargain: essentially all of the achievable turnover for the smallest possible
/// give-away. Discounting further buys nothing and only donates margin.
///
/// Our targets are well centred for this to be safe — median 1.01x the market
/// median across 30,256 flips — and the cost floor still refuses to list under
/// `paid * 1.05`, so a 0.95 open cannot create a loss; it just stops us opening
/// in the slowest band on the board.
///
/// Turnover is the binding constraint, not per-flip margin: >24h holds are 13%
/// of buys but 67% of slot-hours, and the fleet is uptime-bound.
pub static LIST_OPEN_FACTOR: LazyLock<f64> = LazyLock::new(|| num("LIST_OPEN_FACTOR", 1.05));

/// Floor under how far a market-following ask may be dragged, as a fraction of
/// the item's SALES median.
///
/// ⚠️ Exists because `LIST_FOLLOW_MARKET` without it cost real money within two
/// hours of going live: a 5-star Spiritual Juju Shortbow bought at 13.00M
/// against a 28.25M estimate was relisted at **13.65M** (exactly `paid*1.05`,
/// the cost floor) and taken 64 seconds later by a COFL user, while the item's
/// own 3-day median was **35.00M**. One cheap live listing had dragged `lbin`
/// down and we followed it to cost.
///
/// That is precisely what the original `lbin >= target * 0.92` gate defended
/// against, and the `LIST_MARKET_CAP` comment already said so — "robust to a
/// single lowball, unlike lbin". The lesson: `lbin` is ONE listing and anybody
/// can set it; a median cannot be moved by one listing but still falls with a
/// genuine decline, which is why it is the right anchor for a floor.
///
/// 0 disables the guard (raw lbin chasing — do not).
/// Hard floor on any market-following ask, as a fraction of our own target.
///
/// ⚠️ `lbin` and `market_median` are computed over a pool that need not match
/// this item's variant. When the item is genuinely worth more than the pool,
/// following the pool is a giveaway. A Jaded Sorrow Leggings valued at 14.41M
/// was dragged from a 12.97M ask down to the BASE pool's 6.97M lbin, clamped
/// back to 10.50M by the cost floor, and taken instantly; the 11.03M relist sold
/// instantly too, so the item was worth well above both.
///
/// The model path already had exactly this protection (`max(target * 0.9)`) and
/// market-following bypassed it. If the TARGET is the thing that is wrong, fix
/// pricing (`SHORT_TERM_WIDEN_HOURS`, `THIN_KEY_BASE_TREND`) — a listing-time
/// override cannot tell "our estimate is stale" from "this variant is premium".
pub static LIST_MIN_FRACTION: LazyLock<f64> = LazyLock::new(|| num("LIST_MIN_FRACTION", 0.9));

pub static LIST_LOWBALL_GUARD: LazyLock<f64> = LazyLock::new(|| num("LIST_LOWBALL_GUARD", 0.85));

/// Days in clearance after which the cost floor stops applying entirely, so a
/// dead item can list at whatever the market will actually pay.
///
/// `MAX_LOSS_PCT` caps the floor at `paid * 0.90`. Measured 2026-08-03: **266
/// held items worth 9.96B trade BELOW that** (market/paid p10 0.21, p50 0.65,
/// p90 0.85), so the floor sits above any price that could clear them and they
/// are structurally unlistable at any age. 198 of them, **6.97B**, are already
/// 7+ days old.
///
/// A large share is legacy `pageflipper` stock: that lane bought 24.50B between
/// 07-20 and 07-26, sold only **34%** of it (against 86% for the median lane),
/// and stopped buying entirely when the crawlers were banned. It is dead capital
/// sitting in slots, and the fleet is uptime/slot-bound.
///
/// An item that never sells realises ZERO and holds a slot for ever, so past
/// some age the market price beats the hold.
///
/// ⚠️ This CRYSTALLISES a real loss on everything it touches — a capital
/// recycling decision, not a pricing fix. 0 (default) = off, floor always
/// applies. 7 is the conservative setting.
pub static FLOOR_GIVE_UP_DAYS: LazyLock<f64> = LazyLock::new(|| num("FLOOR_GIVE_UP_DAYS", 0.0));

/// Clearance days credited per listing that expired without selling.
///
/// The clearance ladder is indexed to WALL-CLOCK days, but the feedback arrives
/// in hours. Measured 2026-08-07 over 289 physical items reconstructed from the
/// fleet's own auction history (`flip-ledger/itemseq.py`, via Coflnet's per-item
/// `flatNbt.uid`):
///
/// ```text
/// listing duration        p25 6.0h   MED 6.0h   p75 6.0h   <- always exactly 6h
/// dead time between them  p25 0.1h   MED 2.4h   p75 10.8h
/// what we do after a listing EXPIRES: raised 20%, unchanged 26%, lowered 54%
/// implied decay           MED -2.3%/day, i.e. the ladder, working as designed
/// ```
///
/// So every ~8.4h we get a hard signal that the ask was wrong, and answer it
/// with a 0.7% cut. An item opening 30% over market needs ~13 days and ~37
/// failed listings to walk down to market, and 46% of relists do not cut at all.
/// Meanwhile 55% of all our listings (541 of 981 ended) expire unsold.
///
/// Crediting each observed failure as extra clearance days reuses the ladder
/// wholesale — same 2%/day step, same 25% cap, same `LIST_MIN_FRACTION` lowball
/// floor, same `cost_floor` — so nothing new can undersell. It only changes how
/// fast the existing, already-bounded ladder advances, and only for items that
/// have demonstrably failed to sell.
///
/// ⚠️ `FLOOR_RELIEF_PER_DAY` keys on the same counter, so this also relaxes the
/// cost floor at the evidence rate. That is bounded by `MAX_LOSS_PCT` (paid*0.90)
/// and is the point: 42 stuck items worth 3.35B are pinned above a market that
/// has since fallen below what we paid.
///
/// 0 (default) = off, byte-identical. 1.0 = one failed listing counts as a day.
pub static LIST_FAIL_DAYS: LazyLock<f64> = LazyLock::new(|| num("LIST_FAIL_DAYS", 0.0));

/// Hours that must pass before a previous listing instruction counts as FAILED.
///
/// The mod re-polls its inventory every ~15s, so "we priced this item before" is
/// not evidence of anything on its own. A BIN runs for exactly 6.0h (measured
/// p25 = p50 = p75), so a gap this size means the item went to the auction house
/// and came back. Below it, we are just watching the same inventory poll churn.
pub static LIST_FAIL_MIN_GAP_H: LazyLock<f64> = LazyLock::new(|| num("LIST_FAIL_MIN_GAP_H", 5.0));

/// Cap on credited failures, so one permanently stuck item cannot run the
/// counter away. The ladder caps at 25% and the floor at `MAX_LOSS_PCT` anyway;
/// this keeps the arithmetic legible and bounds `LIST-PRICE` reporting.
pub static LIST_FAIL_MAX: LazyLock<f64> = LazyLock::new(|| num("LIST_FAIL_MAX", 12.0));

/// Never open a held item ABOVE the visible market.
///
/// `price_inventory` opens at `target * 1.05` and only consults the lowest live
/// BIN when `lbin >= target * 0.92`, so a market that has fallen well below our
/// target is ignored — precisely when following it matters most. Nothing
/// corrects it until the 6h clearance ladder, so the item sits for six hours
/// burning listing fees and a slot at a price nobody will pay.
///
/// The demand curve makes this unambiguous: at-or-above par is the only cliff
/// (110% of median = 30 min to sell and 50% sell-through, 95% = 15 min and 65%).
/// Opening above market is the worst available choice.
///
/// Only ever LOWERS the ask, and the cost floor still refuses to sell into a
/// loss, so it cannot dump an item cheap.
pub static LIST_FOLLOW_MARKET: LazyLock<bool> =
    LazyLock::new(|| num("LIST_FOLLOW_MARKET", 0.0) != 0.0);

/// When a key is too thin to form its own short-term view, inherit the BASE
/// key's trend instead of quoting a stale long-term median.
///
/// [`SHORT_TERM_WIDEN_HOURS`] fixed keys selling a few a day, but not the truly
/// thin ones. Real miss 2026-08-02 15:57, AFTER that fix shipped: a Hegemony
/// Artifact bought at 490M against an est of 675M with **samples=9**. The key
/// forks per enrichment type (`enrich_critical_chance` is not
/// `enrich_magic_find`), so it trades ~0.4/day and cannot put 3 sales in even a
/// 48h window. Both recency mechanisms stayed dead and it quoted the pre-crash
/// median while the market sat at 495M.
///
/// The base key `HEGEMONY_ARTIFACT~bid=10` had 480 references and was visibly
/// down ~25%. That signal existed the whole time and only the model path used
/// it (`ModifierModel`), never the stats lane.
///
/// Applied only when the base trend is NEGATIVE, so it can lower a target and
/// never raise one.
pub static THIN_KEY_BASE_TREND: LazyLock<bool> =
    LazyLock::new(|| num("THIN_KEY_BASE_TREND", 0.0) != 0.0);

/// Pool `enrich_*` extras into one `x:enrich` token instead of one key per type.
///
/// Enrichment TYPE does not move price; forking a key per type does. Measured on
/// HEGEMONY_ARTIFACT over 7 days against a 496.0M plain median (n=633):
///
/// | type | n | premium |
/// |---|---|---|
/// | `enrich_magic_find` | 21 | +9.4M |
/// | `enrich_sea_creature_chance` | 4 | +34.2M |
/// | `enrich_critical_chance` | 3 | +3.0M |
/// | `enrich_critical_damage` | 1 | +181.0M |
/// | `enrich_walk_speed` | 1 | +154.0M |
/// | **all pooled** | **30** | **+8.4M** |
///
/// Every type with real sample size is +3M..+9.4M, i.e. under 2% of the item.
/// The two triple-digit premiums are single pre-crash sales, not a signal.
///
/// The fork is what makes this expensive. Split 7 ways the key trades ~0.66/day,
/// so it can never put 3 sales in the widened short-term window and quotes a
/// 21-day median instead. Pooled it trades ~4.3/day, which can. On 2026-08-04 the
/// split key quoted **672.6M** for an enriched Hegemony whose base was 499.0M —
/// a +173M "premium" on a real +8.4M — because the two pools' 21-day medians are
/// measured over different EFFECTIVE spans (71% of plain sales land in the last
/// 4 days, 67% of enriched sales are 7-21 days old), so their ratio books the
/// market's 29% crash as an enrichment premium. See
/// [[finder-variant-key-fork]].
///
/// Off by default: it changes keys, so it moves every golden that has an
/// enriched item in it.
pub static ENRICH_POOL: LazyLock<bool> = LazyLock::new(|| num("ENRICH_POOL", 0.0) != 0.0);
pub static ATTR_MIN_SHARE: LazyLock<f64> = LazyLock::new(|| num("ATTR_MIN_SHARE", 0.15));
pub static FEATURE_MIN_VALUE: LazyLock<f64> = LazyLock::new(|| num("FEATURE_MIN_VALUE", 500_000.0));

/// Route variant components (rune=, dye=, potion=, cake=, hat=, bid=...) through
/// the same significance test every other value-bearing token already passes.
///
/// `base_key` appends `~<variant>` UNCONDITIONALLY, so a 1k rune forks the price
/// pool exactly as hard as a 40M dye -- and worse, it forks the BASE group too,
/// so pass-2 significance can never compare "with rune" against "without rune".
/// The item is only ever compared to itself, which is why
/// `ASPECT_OF_THE_VOID~rune=SNOW1` came back notpriceable while plain
/// ASPECT_OF_THE_VOID prices fine (missed flip, 56k buy / 24M profit).
///
/// With this ON, variant parts are emitted as `var:` features and dropped from
/// the base key, so pass 2 measures each one against real sales and only those
/// clearing ATTR_MIN_SHARE keep their own key. OFF (default) is byte-identical
/// to the old behaviour, so the goldens hold.
/// Take an obviously-good flip the pricing path gave up on.
///
/// Real miss, 2026-07-28: `ASPECT_OF_THE_VOID~rune=SNOW1` at 56,000 with a ~24M
/// resale, dropped `notpriceable`. `base_key` appends the variant, so a
/// rune-bearing item has neither a price pool NOR a `high_for_base` fallback,
/// while a plain Aspect of the Void has both. The gates measure prediction
/// QUALITY but are applied as if they measured RISK.
///
/// The rescue prices off the BARE base (variant stripped) using the MEDIAN, not
/// the p95 high: if the variant carries value, stripping it UNDER-estimates,
/// which is conservative. Combined with RESCUE_MIN_MULT this only fires on an
/// item listed at a small fraction of what the plain item reliably sells for,
/// which is safe at any size -- so there is no price ceiling by default.
///
/// RESCUE_MIN_MULT = 0 disables the rescue entirely.
/// RESCUE_MAX_BUY  = 0 means no ceiling (default); set it to cap exposure.
pub static RESCUE_MIN_MULT: LazyLock<f64> = LazyLock::new(|| num("RESCUE_MIN_MULT", 0.0));
pub static RESCUE_MAX_BUY: LazyLock<f64> = LazyLock::new(|| num("RESCUE_MAX_BUY", 0.0));

pub static VARIANT_SIG: LazyLock<bool> = LazyLock::new(|| num("VARIANT_SIG", 0.0) != 0.0);

/// Read the stack size (`i[0].Count`) and let it split the key.
///
/// `decode_item_bytes` navigates to `i[0].tag.ExtraAttributes`, so `Count` --
/// which sits on `i[0]` itself -- never reached [`ItemAttributes`]. Every stack
/// size of an item therefore shared ONE price pool: a live dump has KAT_FLOWER
/// asking 650k at 1x and 40.9M at 64x, and GOBLIN_OMELETTE 1.2M at 1x against
/// 82.5M at 64x, all priced off a single median.
///
/// Quantity is NOT a modifier to be blended in, and it is NOT a divisor either
/// (a 10x stack does not sell for 10x a single). It is part of what the item IS,
/// so it belongs in the base key alongside stars -- stacks pool with stacks.
///
/// This ALWAYS splits rather than going through significance, because the two
/// error directions are not symmetric: refusing a fair stack costs a flip, but
/// buying a single against a stack-inflated median costs money. Splitting
/// strands stack listings with no refs (they simply go unpriced) until the pool
/// refills, which takes at most `REF_MAX_AGE_DAYS`.
///
/// Historical refs were stored already-decoded and have no count, so they all
/// deserialize as 1 -- the old contaminated pools age out rather than being
/// rewritten.
pub static NBT_COUNT: LazyLock<bool> = LazyLock::new(|| num("NBT_COUNT", 0.0) != 0.0);

/// Read value-bearing ExtraAttributes keys the parser skipped, as `extras`
/// features (so they go through significance like every other feature).
///
/// `model` is the one that cost real money: COFL bought a `sumsung_2` Abicase
/// for 16.5M against a 25.67M target while all 2,886 Abicase sales in our store
/// looked identical to us and pooled at a 19.49M median, so a good one read as
/// below-median. `dye_donated` is the same shape on BUCKET_OF_DYE.
///
/// These go through significance rather than the base key on purpose: they are
/// pure additions, so an unknown-but-worthless one costs nothing, while putting
/// them in the base key would strand every Abicase with no refs at all. Pass 2
/// promotes them once real sales show they carry value.
pub static NBT_EXTRA_FIELDS: LazyLock<bool> = LazyLock::new(|| num("NBT_EXTRA_FIELDS", 0.0) != 0.0);

/// Stop hand-picking which NBT fields matter. Emit EVERY unread ExtraAttributes
/// key as an `extras` feature and let pass-2 significance decide.
///
/// The generalisation of [`NBT_EXTRA_FIELDS`], which does exactly this for a
/// hardcoded two-element list (`model`, `dye_donated`). The list is the problem:
/// every entry on it had to be found by hand first, so the parser can only ever
/// know about blind spots we already went looking for. Three have been found that
/// way and each cost money before it was found — `model` (8.66M on one Abicase),
/// `collected_coins` (crowns), `additional_coins` ([`MIDAS_TOTAL_COINS`], a 50M
/// Midas keyed as an 8.93M one).
///
/// The argument that this is safe is the same one `NBT_EXTRA_FIELDS` is already
/// built on, and it is strong: these land in `extras`, NOT the base key. An
/// unknown-but-worthless field costs nothing, because a feature that does not
/// clear `MIN_FEATURE_SAMPLES` and `ATTR_MIN_SHARE` never forks a pool. So the
/// asymmetry is total — a worthless field is a no-op, a valuable one is money.
///
/// ⚠️ REQUIRES `VARIANT_SIG=1` to be the safe version of itself, and that is now
/// live. Before it, variant parts forked the base key unconditionally, so
/// widening what we emit would have multiplied the key fragmentation that
/// [[finder-notpriceable-is-key-fragmentation]] measured at 51B. With
/// significance in front, breadth is free.
///
/// ⚠️ Numeric fields are banded by DIGIT COUNT (`collected_coins`' band, which
/// worked), not by a fixed step. A fixed step cannot serve both a 0-10 counter and
/// a 1e9 one, and raw numbers must never become features — that is unique-per-item
/// and would be pure fragmentation.
///
/// ⚠️ Cannot be backfilled: `sold.attrs` is stored already-decoded, so refs only
/// start carrying the new features as fresh sales land. Expect roughly a
/// `refMaxAgeDays` ramp before pass 2 can promote anything, and judge it on
/// `HIGHMISS` and coverage after that, not the day it ships.
pub static NBT_AUTO_FIELDS: LazyLock<bool> = LazyLock::new(|| num("NBT_AUTO_FIELDS", 0.0) != 0.0);

/// Put a PULSE_RING's `thunder_charge` band into the base key.
///
/// Unlike `NBT_EXTRA_FIELDS` above, this deliberately goes in the KEY and not
/// through significance, because the field does not say "this ring has a bonus",
/// it says WHICH ring this is. A Pulse Ring's displayed rarity is a function of
/// absorbed charge, and Recombobulating bumps the DISPLAYED tier one step while
/// adding no charge, so `tier` is a forgeable label and only the charge is real.
/// Measured over 1000 sold auctions (2026-07-29):
///
/// | charge | tier      | recomb | n   | median  |
/// |--------|-----------|--------|-----|---------|
/// | 0      | UNCOMMON  | no     | 817 | 2.84M   |
/// | 150k   | RARE      | no     | 74  | 12.0M   |
/// | 1M     | EPIC      | no     | 19  | 35.0M   |
/// | 1M     | LEGENDARY | YES    | 13  | 49.5M   |
/// | 5M     | LEGENDARY | no     | 17  | 110.0M  |
/// | 5M     | MYTHIC    | YES    | 20  | 121.4M  |
///
/// A recombed LEGENDARY is 45% of a true LEGENDARY. Without this, every ring
/// shares base key `PULSE_RING` and the only splitting feature is the learned
/// `recomb`, so pool `PULSE_RING#recomb` spans 2.7M..129M at a 33.9M median.
/// Prod priced a 0-charge recombed RARE (really worth ~2.7M) at 33.9M and called
/// it an 1130% flip, and separately passed on a real 120M MYTHIC for "margin".
pub static PULSE_CHARGE_BAND: LazyLock<bool> =
    LazyLock::new(|| num("PULSE_CHARGE_BAND", 0.0) != 0.0);

/// Read `collected_coins` (Crown of Avarice) as a banded `extras` feature.
///
/// The single largest pricing blind spot left on 2026-08-09. `collected_coins`
/// is an ExtraAttributes key the parser never read, so every crown shared one
/// pool. Over the last 21 days that pool held 557 sales spanning p25 430.0M to
/// max 2.349B — a 4.4x spread quoted as one 548.0M median. The live dump splits
/// perfectly on the field, with no overlap at all:
///
/// | collected_coins | n  | ask range        |
/// |-----------------|----|------------------|
/// | 0               | 8  | 625.0M .. 750.0M |
/// | 4.90M           | 1  | 650.0M           |
/// | 100.0M          | 1  | 850.0M           |
/// | 554.9M          | 1  | 1.350B           |
/// | 702.3M          | 1  | 1.681B           |
/// | 1.000B (capped) | 12 | 1.880B .. 2.050B |
///
/// The item's own lore names the mechanism: it grants `+0.015x Damage` and
/// Magic Find **for each digit of Coins consumed**, and `(Perk changes at 1B
/// Coins consumed)`. So the band is the DIGIT COUNT, not an arbitrary cut — the
/// price steps where the game steps. `collected_coins` caps at exactly 1e9.
///
/// This also retires a false belief. [[finder-crown-of-avarice-two-populations]]
/// recorded "renowned +28% vs ancient"; at the 1B cap ancient asks 1.885B..2.050B
/// and renowned 1.880B..1.950B, i.e. the same. That premium was `collected_coins`
/// read through a reforge that happened to correlate with it.
///
/// ⚠️ Deliberately an `extras` feature and NOT the base key, unlike
/// [`PULSE_CHARGE_BAND`] above. `sold.attrs` stores the PARSED attributes, not
/// raw NBT, so a parser change cannot be backfilled: all 1,253 historical crown
/// sales carry no band. A base-key fork would therefore start every band at zero
/// refs and drop it to the lbin lane — which is precisely the 2.0B wall that cost
/// us 172,180,532 on one crown. Through significance the cold start is a no-op
/// (an unlearned feature does not fork) and pass 2 promotes each band once real
/// sales carry it. At ~45 crown sales/day that is under a day per band.
///
/// Band 0 emits no feature at all, so a crown with no coins consumed keys
/// byte-identically to today.
pub static CROWN_COINS_BAND: LazyLock<bool> = LazyLock::new(|| num("CROWN_COINS_BAND", 0.0) != 0.0);

/// Band a Midas item on `winning_bid + additional_coins` instead of `winning_bid`
/// alone.
///
/// A Midas item's stats scale with the TOTAL coins sunk into it. `winning_bid` is
/// only what the original auction closed at; the owner can keep topping it up
/// afterwards, and that top-up lands in a SECOND field, `additional_coins`, which
/// the parser has never read. So the one number that sets the item's value was
/// being read at a fraction of its true size.
///
/// Found 2026-08-14 on auction `9e1fbca293774f5185015c9907c4fdc8`, a Withered
/// Midas' Sword listed at 10.0M:
///
/// ```text
/// winning_bid       8_930_000
/// additional_coins 41_070_000
///                  ----------
/// total            50_000_000   <- a round cap, i.e. deliberately maxed
/// ```
///
/// `floor(8.93M / 10M)` = band 0, so a 50M sword keyed `MIDAS_SWORD~bid=0` and was
/// priced against the cheapest Midas swords in the game. The finder rejected it at
/// 60% confidence. The same key produced the 2026-08-14 listing of a Midas' Sword
/// at 6.0M against an 11.0M lbin. One unread field, both incidents.
///
/// ⚠️ This cuts BOTH ways and the low bands are the ones that are poisoned. Every
/// topped-up sword in history was filed under its (too low) `winning_bid` band, so
/// `bid=0` is a mix of genuinely cheap swords and hidden maxed ones, and its median
/// is inflated. We therefore OVER-value cheap Midas swords today and UNDER-value
/// topped-up ones. Both correct themselves as the mislabelled refs age out at
/// `refMaxAgeDays`.
///
/// ⚠️ `sold.attrs` is stored already-decoded ([[flipfinder-midas-bid-banding]]), so
/// this cannot be backfilled; it only fixes items decoded from here on.
///
/// ✅ The diff is EMPTY for any item without `additional_coins`: the field
/// contributes 0 when absent, so the band, the token and the key are byte-identical
/// for every non-topped-up item. Only items carrying the field move, and they move
/// onto bands that already exist and already have refs. That is why this re-uses
/// `bid=`/`hibid=` rather than minting a new token the way the 2026-07-15 `hibid=`
/// change had to.
pub static MIDAS_TOTAL_COINS: LazyLock<bool> =
    LazyLock::new(|| num("MIDAS_TOTAL_COINS", 0.0) != 0.0);

/// Horizon (hours) the item-level sell-through probability is measured over.
///
/// 24h is the decision-relevant window: an item that has not sold in a day has
/// held a bot's auction slot through the whole span most flips clear in.
pub static TTS_SELL_HORIZON_H: LazyLock<f64> = LazyLock::new(|| num("TTS_SELL_HORIZON_H", 24.0));

/// Minimum item-level sell-through before a fast observed TTS may waive a tier's
/// volume floor (needs `TTS_LIQUIDITY`).
///
/// The waiver exists because volume is only a proxy for liquidity, and a real
/// measured TTS beats a proxy. But the measured TTS was survivorship-biased: it
/// only ever saw listings that SOLD. Measured 2026-08-05 over the 2,383 items
/// carrying at least 20 tracked listings, **455 read a naive median under 6h
/// while under half their listings sold within 24h** — `REDSTONE_ORE` reads
/// 0.22h off 2 sales
/// against 78 that never sold. Those 455 are exactly the items the waiver would
/// have unlocked, and every one of them is a slot that never comes back.
///
/// At 0.5 the waiver requires the median listing of the item to actually sell
/// inside the horizon. Set to 0 to restore the unguarded pre-2026-08-05 waiver.
pub static TTS_MIN_SELL_THROUGH: LazyLock<f64> = LazyLock::new(|| num("TTS_MIN_SELL_THROUGH", 0.5));

/// Discount a key's target by how rarely the item actually sells.
///
/// A key's target is the median of sales, and a sale is a listing that WON. On an
/// item where only a third of listings ever sell, that median is the median of
/// the winners, so quoting it as "what this is worth" over-states the price at
/// which the thing actually clears. It is the same survivorship bug that made
/// `fair_tts_h` read 0.22h on `REDSTONE_ORE`, applied to PRICE instead of TIME.
///
/// Measured 2026-08-07 over 4,090 of our own resolved flips (45d), realised
/// resale as a fraction of the finder's own estimate, bucketed by the item's
/// Kaplan-Meier sell-through:
///
/// | sell-through | n | median | mean |
/// |---|---|---|---|
/// | 0.00-0.35 | 42 | 0.910 | 0.881 |
/// | 0.35-0.50 | 73 | 0.970 | 0.974 |
/// | 0.50-0.70 | 863 | 0.975 | 0.982 |
/// | 0.70-0.85 | 1517 | 1.003 | 1.013 |
/// | 0.85+ | 468 | 1.007 | 0.996 |
///
/// Monotone, and it flattens out right around 0.70 — hence the pivot. ⚠️ Those
/// numbers are a LOWER bound: they can only be computed for flips that cleared,
/// and the ones that never cleared are precisely the badly-priced tail.
///
/// Same measurement showed the hold time this is really buying: median 20.9h
/// under 0.5 sell-through against 1.4h above 0.85.
/// Floor for a MODEL-priced item's opening ask, as a fraction of its target.
///
/// `use_lbin` is unconditionally true for a model-priced item, so this floor is
/// the ask whenever any competitor sits below it. It was hardcoded at 0.90, and
/// measured 2026-08-07 over 442 model resales that cleared inside 6h (so the
/// clearance ladder never ran) the median realised/estimate was **exactly 0.90**
/// -- as was every single item family within it. A constant, not a distribution:
/// we asked 0.9x and got 0.9x, on ~28B of flow.
///
/// The model target itself is close to right: model items that reached clearance
/// are priced by the ladder rather than by this floor and realised **0.99** in
/// the 6-24h band. So the 10% was a giveaway, not a justified uncertainty
/// discount. The opening haircut for model uncertainty already exists separately
/// (`fresh_target = target * 0.97`).
///
/// Default 0.90 = the historical behaviour, byte-identical.
pub static MODEL_LIST_FLOOR: LazyLock<f64> = LazyLock::new(|| num("MODEL_LIST_FLOOR", 0.90));

pub static LIQ_DISCOUNT: LazyLock<bool> = LazyLock::new(|| num("LIQ_DISCOUNT", 0.0) != 0.0);

/// Sell-through at or above which no liquidity discount applies. Above ~0.70 the
/// measured estimate error is already 1.00, so discounting there would only cost
/// flips.
pub static LIQ_DISCOUNT_PIVOT: LazyLock<f64> = LazyLock::new(|| num("LIQ_DISCOUNT_PIVOT", 0.70));

/// Discount per unit of sell-through below the pivot. 0.30 puts a 0.50
/// sell-through item at 0.94 and a 0.35 one at 0.895, tracking the measured
/// curve slightly conservatively (the measurement under-states the error).
pub static LIQ_DISCOUNT_K: LazyLock<f64> = LazyLock::new(|| num("LIQ_DISCOUNT_K", 0.30));

/// Hard floor on the discount, so a dead-looking item is marked down but never
/// re-valued to nothing. At the default this binds below ~0.20 sell-through.
pub static LIQ_DISCOUNT_FLOOR: LazyLock<f64> = LazyLock::new(|| num("LIQ_DISCOUNT_FLOOR", 0.85));

pub static MIN_FEATURE_SAMPLES: LazyLock<usize> =
    LazyLock::new(|| num("MIN_FEATURE_SAMPLES", 3.0) as usize);

/// Let a feature that makes an item CHEAPER fork the key, not just a dearer one.
///
/// Pass-2 significance asks `median(with_feature) - base >= base * ATTR_MIN_SHARE`.
/// That is ONE-SIDED: a feature worth +200% forks the key, a feature worth -70%
/// can never clear a positive threshold with a negative delta. So cheap variants
/// are structurally invisible — they stay pooled with the expensive majority,
/// inherit its median, and we buy them believing we found a discount.
///
/// Measured 2026-08-04, auction `ecc840645fdd47c99599ce7ce8d61a90`. The pool
/// `STARRED_MIDAS_SWORD*5~bid=0` is 10 gilded (~390M) and 2 sharp (~100M), a
/// 3.5x split on REFORGE alone:
///
/// - `reforge:gilded` median 390M vs base 385.5M ⇒ delta +4.5M, under the 57.8M
///   bar. The majority DEFINES the base, so the majority can never be
///   significant against it.
/// - `reforge:sharp` median 100M vs base 385.5M ⇒ delta **-285.5M**, refused for
///   being negative though it is 5x the bar in magnitude.
///
/// Neither forks, so a ~100M sword was quoted 350.1M and we bought at 111.2M —
/// from a seller who had paid 90.0M for it three hours earlier.
///
/// Two-sided, `reforge:sharp` forks, leaving that key 2 refs (< `MIN_REFS`), so
/// it prices as notpriceable and the flip never fires. 39 item*star groups split
/// >=1.8x on reforge with n>=4 each side, up to 11.26x (`HEARTFIRE_DAGGER`,
/// none 45M vs coldfusion 507M). See [[finder-reforge-not-in-key]].
///
/// ⚠️ This only ever REMOVES flips. Expect emit to fall.
/// ⚠️ Bounded by `MIN_FEATURE_SAMPLES` (3): a minority of 1-2 sales still cannot
/// fork, which is why the Midas above needs the separate pool-representativeness
/// guard as well.
///
/// Off by default: it changes keys, so it moves every golden with a
/// value-reducing feature in it.
pub static SIG_TWO_SIDED: LazyLock<bool> = LazyLock::new(|| num("SIG_TWO_SIDED", 0.0) != 0.0);

/// Sample bar for forking a feature that makes an item CHEAPER (needs
/// [`SIG_TWO_SIDED`]). Defaults to [`MIN_FEATURE_SAMPLES`], i.e. no asymmetry.
///
/// The two directions are not equally risky. Forking a DEARER feature splits a
/// pool we then price off, so it wants real evidence. Forking a CHEAPER one
/// leaves the minority under `MIN_REFS`, the item goes notpriceable, and the flip
/// never fires — being wrong costs a missed flip, not an overpay.
///
/// This is what the Midas needs. `reforge:sharp` had n=2 against
/// `MIN_FEATURE_SAMPLES=3`, so even two-sided significance could not fork it, and
/// a ~100M sword kept quoting the gilded pool's 350.1M. At 2 it forks, that key
/// holds 2 refs (< `MIN_REFS`), and the flip is refused.
pub static SIG_CHEAP_MIN_SAMPLES: LazyLock<usize> =
    LazyLock::new(|| num("SIG_CHEAP_MIN_SAMPLES", 3.0) as usize);

/// Cap a target by what the item's OWN feature actually sells for, when the pool
/// it was priced from is mostly other things.
///
/// A key's pool shares only the features that cleared significance. Everything
/// else is mixed in, so a minority variant inherits the majority's median. That
/// is what happened to `STARRED_MIDAS_SWORD*5~bid=0`: 10 gilded (~390M) and 2
/// sharp (~100M) in one pool, target 350.1M, and we paid 111.2M for a sharp.
///
/// `SIG_CHEAP_MIN_SAMPLES` fixes this by forking the key, but it is bounded below
/// by sample count — at n=1 nothing can fork. This is the price-time backstop:
/// pass 2 already computed a median for EVERY feature group, so if the item's own
/// feature has a median materially under the pool's, take it.
///
/// **Downward only.** It can lower a target, never raise one, so the worst case is
/// a refused flip rather than an overpay — the same asymmetry
/// `SIG_CHEAP_MIN_SAMPLES` rests on.
///
/// ⚠️ Applied OUTSIDE `compute_price_for`, which is memoised by KEY alone
/// (`pf_memo`). Two items sharing a key but differing in a non-significant
/// feature would otherwise inherit each other's adjustment. Same trap as
/// [[finder-pet-exp-gradient]].
pub static POOL_REPR_GUARD: LazyLock<bool> = LazyLock::new(|| num("POOL_REPR_GUARD", 0.0) != 0.0);
/// Minimum sales of the item's own feature before its median is usable.
pub static POOL_REPR_MIN_SAMPLES: LazyLock<usize> =
    LazyLock::new(|| num("POOL_REPR_MIN_SAMPLES", 2.0) as usize);
/// How far under the target the feature median must sit before it binds.
pub static POOL_REPR_MIN_GAP: LazyLock<f64> = LazyLock::new(|| num("POOL_REPR_MIN_GAP", 0.15));

/// Hard RAM guard: shed the oldest refs past this COUNT. Prod runs 1_800_000 on
/// the RAM-bound box. 0 or less disables it (matches the TS `cap <= 0` check).
pub static REF_MAX_RAM: LazyLock<usize> =
    LazyLock::new(|| num("REF_MAX_RAM", 2_200_000.0).max(0.0) as usize);

/// Reference retention window for the on-disk store. Prod runs 36500 (~never
/// prune): the raw history is deliberately kept even though pricing only weights
/// the last REF_MAX_AGE_DAYS.
pub static RETENTION_DAYS: LazyLock<i64> = LazyLock::new(|| num("RETENTION_DAYS", 14.0) as i64);

/// How far back `unlisted_purchases_by_item_uuid` looks, in seconds.
///
/// That query runs at the TOP of every sweep, before any flip can be evaluated.
/// Unbounded it SCANNED all of `posted` and sorted in a temp B-tree -- 190ms per
/// sweep, growing with the table, and it matched 64,310 of 139,235 rows because
/// `bought_at` marks the flagged auction ENDING (bought by anyone), not our own
/// purchase. The map only links a bot's relist back to its buy, and a relist
/// follows its buy by minutes, so 3 days is already generous. With
/// `idx_posted_unlisted` this is ~14ms.
pub static UNLISTED_LOOKBACK_S: LazyLock<i64> =
    LazyLock::new(|| num("UNLISTED_LOOKBACK_S", 259_200.0) as i64);

// craftCost
pub static CRAFT_CEILING_MULT: LazyLock<f64> = LazyLock::new(|| num("CRAFT_CEILING_MULT", 1.1));
pub static ESSENCE_PER_STAR_COST: LazyLock<f64> =
    LazyLock::new(|| num("ESSENCE_PER_STAR_COST", 1_000_000.0));

// sniper
/// Instrumentation ONLY (never gates a decision): log the reject reason for any
/// candidate whose best-estimate profit clears this, so "why did we not emit the
/// big flip a competitor took" is answerable from prod logs. These are rare
/// (single digits/hour), so the log cost is nil. 0 disables.
pub static HIGH_MISS_MIN_PROFIT: LazyLock<f64> =
    LazyLock::new(|| num("HIGH_MISS_MIN_PROFIT", 5_000_000.0));
// REMOVED 2026-07-30: `MAX_PROFIT` and `TOO_GOOD_MIN_ROI`, the "too good to be
// true" veto. Both are now ignored if set in the environment. See the tombstone
// above `after_tax` in `sniper.rs` for the three prod flips it destroyed and why
// no threshold can work. Reference quality is `MIN_CONFIDENCE`'s job.

/// Absolute-profit override for `MIN_MARGIN` on liquid, well-evidenced keys.
///
/// `MIN_MARGIN` is a flat percentage, so the coins it demands scale with the ask
/// and expensive items are held to an enormous absolute bar. See
/// `big_ticket_margin_ok` in `sniper.rs` for the audit: every predicted-margin
/// band from 0-12% was positive EV with a 78-95% hit rate and ~1h time to sell.
///
/// 0 (the default) disables the override completely, so `MIN_MARGIN` behaves
/// exactly as it does today and the goldens are untouched. This is a RISK
/// parameter, not a bug fix: the audit can only see items that sold, so it is an
/// upper bound. Suggested first setting, which is the 6-9% band and above:
///
/// ```text
/// MARGIN_OVERRIDE_MIN_PROFIT=25000000
/// MARGIN_OVERRIDE_MIN_MARGIN=0.06
/// MARGIN_OVERRIDE_MIN_SAMPLES=8
/// MARGIN_OVERRIDE_MIN_VOLUME=2
/// ```
pub static MARGIN_OVERRIDE_MIN_PROFIT: LazyLock<f64> =
    LazyLock::new(|| num("MARGIN_OVERRIDE_MIN_PROFIT", 0.0));
pub static MARGIN_OVERRIDE_MIN_MARGIN: LazyLock<f64> =
    LazyLock::new(|| num("MARGIN_OVERRIDE_MIN_MARGIN", 0.06));
pub static MARGIN_OVERRIDE_MIN_SAMPLES: LazyLock<i64> =
    LazyLock::new(|| num("MARGIN_OVERRIDE_MIN_SAMPLES", 8.0) as i64);
pub static MARGIN_OVERRIDE_MIN_VOLUME: LazyLock<f64> =
    LazyLock::new(|| num("MARGIN_OVERRIDE_MIN_VOLUME", 2.0));
pub static MIN_PROFIT: LazyLock<f64> = LazyLock::new(|| num("MIN_PROFIT", 250_000.0));
pub static MIN_MARGIN: LazyLock<f64> = LazyLock::new(|| num("MIN_MARGIN", 0.12));

/// Slack on the two COARSE pre-gates that run before an item is really priced
/// (`cheap_median * 1.3` and `high_for_base * 1.5`). Those anchors are raw pool
/// medians: no attribute adjustment, no minor-feature credit, no trend. Judging
/// them with the full `MIN_MARGIN` makes the pre-gate STRICTER than the real
/// lanes it is supposed to pre-filter for, so it discards flips the stats/model
/// lanes would have taken — 70 of 89 recoverable >=5M misses in one prod day
/// were killed here without ever being priced. Widening this costs CPU only:
/// every real money gate still runs afterwards, so nothing unsafe can pass.
/// 1.0 = exactly the old behavior (goldens/TS parity). 1/(1-MIN_MARGIN) ≈ 1.14
/// reduces the pre-gate to a pure "price is absurd vs the pool" sanity bound and
/// leaves every margin decision to the lane that can actually price the item.
pub static PRESCREEN_SLACK: LazyLock<f64> = LazyLock::new(|| num("PRESCREEN_SLACK", 1.0).max(1.0));
pub static CLEAN_SNIPE_MARGIN: LazyLock<f64> = LazyLock::new(|| num("CLEAN_SNIPE_MARGIN", 0.35));
pub static MIN_VOLUME_PER_DAY: LazyLock<f64> = LazyLock::new(|| num("MIN_VOLUME_PER_DAY", 0.0));
pub static MEDIAN_MAX_UNDERCUTS: LazyLock<i64> =
    LazyLock::new(|| num("MEDIAN_MAX_UNDERCUTS", 3.0) as i64);

/// When live listings already undercut our resale, reprice to the cheapest of
/// them and re-test margin instead of discarding the flip.
///
/// `MEDIAN_MAX_UNDERCUTS` asks "are there already N listings below our resale?"
/// and answers by vetoing. For a snipe that is the wrong question: we are buying
/// at `price`, so what matters is whether it still pays AFTER undercutting them.
///
/// Real miss, 2026-07-29: a Heroic Glacial Scythe asking 2,980,000 against a
/// 29,326,963 resale, 26.3M of profit, killed by `reason=undercuts`. Even
/// undercutting to the cheapest competitor leaves multiples of the ask.
///
/// This can only ever LOWER the estimate, never raise it, and the margin and
/// profit gates are both re-run against the lowered number. So it can turn
/// a reject into an emit but can never inflate what we claim a flip is worth,
/// and it cannot cause an overpay: the buy price was already fixed.
pub static UNDERCUT_REPRICE: LazyLock<bool> = LazyLock::new(|| num("UNDERCUT_REPRICE", 0.0) != 0.0);
pub static SELLER_RELIST_LIMIT: LazyLock<usize> =
    LazyLock::new(|| num("SELLER_RELIST_LIMIT", 3.0) as usize);
pub static LBIN_MIN_SOLD: LazyLock<i64> = LazyLock::new(|| num("LBIN_MIN_SOLD", 3.0) as i64);
pub static LBIN_MIN_LISTINGS: LazyLock<usize> =
    LazyLock::new(|| num("LBIN_MIN_LISTINGS", 4.0) as usize);

/// The lbin lane's resale `reference` is the 2nd-cheapest LIVE listing, which
/// can be an overpriced wall unrelated to what the item actually sells for: a
/// lone 98M listing on a soul that sells for ~36M manufactured a phantom 153%
/// flip. Cap that reference at the sold-history median times this multiplier so
/// a live wall can never price a relist above what the item demonstrably sells
/// for. It only bites when list[1] exceeds the sold median; a normally priced
/// book leaves the reference untouched, so real undercut flips survive. Set
/// absurdly high (e.g. 1e9) to restore the old wall-anchored behavior.
pub static LBIN_REF_SOLD_MULT: LazyLock<f64> = LazyLock::new(|| num("LBIN_REF_SOLD_MULT", 1.10));
pub static MIN_CONFIDENCE: LazyLock<f64> = LazyLock::new(|| num("MIN_CONFIDENCE", 0.0));

/// Liquidity waiver for the confidence "too good to be true" margin penalty
/// (`CONF_LIQ_WAIVER` = 0 -> OFF, byte-identical to the old behavior, so goldens
/// and TS parity are preserved). A big margin on a DEEP, ACTIVE market (many
/// samples + high daily volume) is a genuine underprice, not a shaky reference,
/// so the margin penalty is blended toward 1.0 by how liquid the item is. Thin or
/// low-volume items are unaffected (liq -> 0). `CONF_LIQ_WAIVER` is the max waiver
/// strength at full liquidity (1.0 = fully waive the penalty); `_SAMPLES`/`_VOLUME`
/// are the sample count / daily volume at which liquidity counts as "full".
/// Strength of the "too good to be true" margin penalty in `refine_confidence`.
///
/// 1.0 = the original `1/(1 + (roi-0.6)*0.8)`; 0.0 = no margin penalty at all.
///
/// The intuition behind the penalty is real: a huge apparent margin USUALLY means
/// the reference is wrong (stale, thin, manipulated), not that free money is
/// lying around. It does not survive contact with our own ledger. Across 46,561
/// flips we bought AND resold, predicted margin is strongly POSITIVELY related to
/// realised margin -- >200% predicted returns a 730% realised median -- and
/// within that high-margin band confidence fails to separate good from bad:
///
///   conf <0.70 (the band the penalty creates): n=372, median realised ROI 345%,
///                                              1.13B coins, 3.2% loss rate
///   conf 0.70-0.85:                            n=458, 400%, 3.45B, 0.0% losses
///   conf >=0.85:                               n=1390, 953%, 12.1B, 0.1% losses
///
/// So the penalty scores a flip DOWN for being good, and the flips it demotes
/// still return a 345% median. A 3.2% loss rate against that is not a reason to
/// refuse the trade.
/// Cap the re-list price at this multiple of the item's OBSERVED market median.
/// 0 = off (byte-identical to the old behaviour).
///
/// We have both an underselling and an overselling problem, and they are the two
/// tails of the SAME distribution, not one bias. Across 30,256 flips, target vs
/// the market median for the same item+star group: p05 0.76x, p50 **1.01x**,
/// p95 **2.20x** — the centre is right, 25.1% are >15% too high and 10.4% >15%
/// too low. Flips that resold and flips that did not have the SAME median
/// (1.01x), so the tails decide, not the centre. A global scale change would
/// help one tail and worsen the other; this clamps only the high tail.
///
/// Anchored on the MEDIAN of recent sales, not `lbin`, so a single lowballed
/// listing cannot drag us down — that is the risk the `lbin >= target * 0.92`
/// guard was built for, and the same guard is why over-estimates currently
/// ignore the market entirely.
/// ⚠️ The anchor BLENDS VARIANTS, so this must be set to catch the tail, not the
/// centre. `FIGSTONE_AXE*5` holds `moonglade` at 17.34M and `none` at 5.0M; a
/// premium variant SHOULD list above that median. p50 of target/market is 1.01x
/// and p75 is 1.15x — much of which is legitimate — while p95 is 2.20x. A cap
/// near 1.0 would underprice good variants, i.e. cause the underselling this is
/// meant to avoid. Somewhere around 1.5 clamps only estimates no variant premium
/// can justify. Validate against time-to-sell before trusting a tighter value.
pub static LIST_MARKET_CAP: LazyLock<f64> = LazyLock::new(|| num("LIST_MARKET_CAP", 0.0));
/// Minimum recent sales before the item+star median is trusted as a market
/// anchor. A median off 3 sales is not a market.
pub static LIST_MARKET_MIN_REFS: LazyLock<i64> =
    LazyLock::new(|| num("LIST_MARKET_MIN_REFS", 20.0) as i64);

/// Stop [`LIST_MARKET_CAP`] from pricing us UNDER the cheapest live BIN.
///
/// The cap exists to stop us asking above a market that has fallen. When
/// `lbin > market_median` the market has NOT fallen — the live book sits above
/// the sales median — and the cap then puts our ask below every competing
/// seller for nothing. With this on, a capped ask is floored at `lbin - 1`,
/// which still undercuts the book (so it sells just as fast) but keeps the
/// difference.
///
/// Measured on prod 2026-08-16: 20.2% of everything we sold within five minutes
/// went for less than 90% of the peer-market median for that item, versus 7.5%
/// in the 2-6h band. `LIST-UNDER` caught the mechanism live on a [Lvl 100]
/// Rabbit — listed 11,650,000 (exactly `market_median`) with `lbin` at
/// 13,550,001 and our own 74-sample, conf-0.96 valuation at 14,312,000.
///
/// ⚠️ Only floors an ask the CAP pulled down. The clearance ladder, the lowball
/// guard and `cost_floor` are untouched, so no new way to overprice appears.
/// 0 = off (byte-identical).
pub static LIST_CAP_LBIN_FLOOR: LazyLock<bool> =
    LazyLock::new(|| num("LIST_CAP_LBIN_FLOOR", 0.0) != 0.0);

pub static CONF_MARGIN_PENALTY: LazyLock<f64> = LazyLock::new(|| num("CONF_MARGIN_PENALTY", 1.0));

pub static CONF_LIQ_WAIVER: LazyLock<f64> = LazyLock::new(|| num("CONF_LIQ_WAIVER", 0.0));
pub static CONF_LIQ_SAMPLES: LazyLock<f64> = LazyLock::new(|| num("CONF_LIQ_SAMPLES", 30.0));
pub static CONF_LIQ_VOLUME: LazyLock<f64> = LazyLock::new(|| num("CONF_LIQ_VOLUME", 20.0));

/// Hypixel's AH tax on a COMPLETED sale, as a fraction of the sale price.
///
/// Distinct from the LISTING fee (see [`LIST_ATTEMPTS_EXPECTED`]) which is
/// charged up front on the ask, whether or not the item ever sells.
/// `sniper::after_tax` historically charged the listing-fee tier ONCE and
/// treated it as the whole cost of selling, so every profit gate ran on a
/// number that ignored the sale tax entirely.
///
/// Measured 2026-08-09 on a real round trip: a 603,900,000 sale was billed
/// 21,137,700, i.e. **3.5002%**. 0 = off (byte-identical to the old model).
pub static AH_SALE_TAX: LazyLock<f64> = LazyLock::new(|| num("AH_SALE_TAX", 0.0));

/// Replace the ad-hoc fee arithmetic with COFL's `ProfitAfterFees` verbatim.
///
/// COFL bills ONE tier-banded percentage on the target and nothing else:
///
/// ```text
/// reduction = 2%                       (listing 1%  + claim 1%)
///   > 10M   -> 3%                      (listing 2%  + claim 1%)
///   >=100M  -> 3.5%                    (listing 2.5%+ claim 1%)
///   Aura    -> +1% and RETURN          (does not stack with Derpy)
///   Derpy and target >= 1M -> +3%      (claiming tax x4)
/// profit = target * (100 - reduction) / 100 - cost
/// ```
///
/// ⚠️ **This flag also fixes a double-count.** [`AH_SALE_TAX`] was set to 0.035
/// from a real measurement — 21,137,700 billed on a 603,900,000 sale. That
/// 3.5002% is COFL's WHOLE `>=100M` reduction, i.e. it already contains the 2.5%
/// listing fee. The old model then charged the listing tier AGAIN, multiplied by
/// [`LIST_ATTEMPTS_EXPECTED`]:
///
/// ```text
///           >=100M    10-100M    <10M
/// old       9.525%     8.320%   5.910%   (tier*2.41 + 3.5)
/// correct   7.025%     5.820%   3.410%   (tier*2.41 + 1.0, same attempts)
/// COFL      3.500%     3.000%   2.000%   (tier*1.00 + 1.0)
/// ```
///
/// So the deployed gate has been ~2.7x too punitive at the top tier, which
/// depresses both the profit number and the ROI% on every emitted flip.
///
/// With this on, [`LIST_ATTEMPTS_EXPECTED`] keeps working but multiplies ONLY
/// the listing half (an item is claimed once no matter how often it is relisted),
/// and `AH_SALE_TAX` is ignored entirely — the claim tax comes from COFL's table.
/// Set `LIST_ATTEMPTS_EXPECTED=1` for exact COFL parity.
///
/// 0 = off (byte-identical to the old arithmetic).
pub static AH_FEE_COFL: LazyLock<bool> = LazyLock::new(|| num("AH_FEE_COFL", 0.0) != 0.0);

/// Waive the CONFIDENCE and VOLUME gates for a flip whose ABSOLUTE profit is
/// large, even at an ordinary ROI.
///
/// Distinct from [`EXTREME_FLIP_MIN_PROFIT`], which needs BOTH a big profit and
/// a huge ROI (500%) because its argument is "the buy price is a trivial
/// fraction of the reference, so a reference error costs the buy price, not the
/// margin". That argument does not cover a large, ordinary-margin trade, and
/// those are the ones worth the most coins.
///
/// Worked case, prod 2026-08-16, auction `a3764a24e5e24e4fa7bdc821e28050ef`:
///
/// ```text
/// Auspicious Titanium Drill DR-X655   BIN 500,000,000
/// our reference 755,221,618   profit 202,167,299   ROI 40%
/// confidence 0.597   samples 7   volume/day 0.44
/// -> "confidence 60% < minConfidence 70%", delivered 0
/// ```
///
/// The valuation was right: the eight most recent sales of that item were
/// 739.9M to 920.0M against a 500M ask, and it was taken by someone else 74
/// seconds after listing. A 202M buffer absorbs a reference error that a 5M one
/// cannot — at 40% ROI the reference can be wrong by 25% and the trade is still
/// +66M — which is precisely the case the confidence gate is a bad proxy for.
///
/// ⚠️ `BIG_FLIP_MIN_SAMPLES` is the guard that keeps this honest: it waives a
/// quality gate, so it must not fire on a pool with no evidence at all. Seven
/// sales is thin but real; one is not a market.
///
/// ⛔ Waives ONLY confidence and volume. profit, ROI, profit/hour, guards,
/// blacklist, flood and routing all still apply. 0 = off (byte-identical).
pub static BIG_FLIP_MIN_PROFIT: LazyLock<f64> = LazyLock::new(|| num("BIG_FLIP_MIN_PROFIT", 0.0));
/// Minimum ROI for [`BIG_FLIP_MIN_PROFIT`]. Never a free pass on a thin margin.
pub static BIG_FLIP_MIN_ROI_PCT: LazyLock<f64> =
    LazyLock::new(|| num("BIG_FLIP_MIN_ROI_PCT", 20.0));
/// Minimum sold-sample depth for [`BIG_FLIP_MIN_PROFIT`].
pub static BIG_FLIP_MIN_SAMPLES: LazyLock<i64> =
    LazyLock::new(|| num("BIG_FLIP_MIN_SAMPLES", 5.0) as i64);

/// Force the Derpy branch of [`AH_FEE_COFL`] on (1) or off (0).
///
/// Unset = COFL's calendar guess (124h active every 124 days from
/// 2024-08-26T07:15Z). The rotation is an approximation of a real in-game
/// election, so this exists to override it the moment the game disagrees, and to
/// pin the value in tests.
pub static AH_FEE_DERPY: LazyLock<Option<bool>> =
    LazyLock::new(|| match std::env::var("AH_FEE_DERPY") {
        Ok(v) if !v.trim().is_empty() => Some(js_number(&v) != 0.0),
        _ => None,
    });

/// Freeze the clock the fee model reads, in unix ms. Tests and backtests only;
/// unset means the real clock.
pub static AH_FEE_NOW_MS: LazyLock<f64> = LazyLock::new(|| num("AH_FEE_NOW_MS", 0.0));

/// How many times we expect to LIST an item before it clears.
///
/// The listing fee is 2.5% (>=100M) of the ASK and is charged on EVERY attempt.
/// BIN runs 6.0h and ~55% of listings expire unsold, so the median flip pays it
/// several times; the real cycle is ~25h, i.e. ~4 attempts.
///
/// Worked example that motivated this — Ancient Crown of Avarice, bought
/// 610,000,000, sold 603,900,000, **net -172,180,532**:
/// ```text
/// gross                                          -6,100,000
/// ah tax          3.5002% of 603,900,000        -21,137,700
/// listing attempt 1,650,308,078 @2.5001% x3    -123,776,712
/// listing attempt   846,596,805 @2.5001%        -21,166,120
/// ```
/// 144.9M of a 172.2M loss was listing fees. The trade lost 6.1M.
///
/// ⚠️ The ask sets the fee, so an inflated target bills you 2.5% of the inflated
/// number every 6h until it clears. Overpricing is NOT a free option, which is
/// the assumption the clearance ladder was built on.
///
/// 1.0 = off (byte-identical: one listing fee, exactly as before).
pub static LIST_ATTEMPTS_EXPECTED: LazyLock<f64> =
    LazyLock::new(|| num("LIST_ATTEMPTS_EXPECTED", 1.0));

/// Scale [`LIST_ATTEMPTS_EXPECTED`] by the item's POOL confidence instead of
/// charging every flip the population mean.
///
/// A single constant is a mean-of-a-mixture error, and it is wrong in BOTH
/// directions. Measured over 4,429 bought flips with confidence and hold time
/// ([[finder-listing-attempts-are-predictable]]):
///
/// ```text
/// conf band     P(sold)  E[attempts]  fee@2%
/// 0.00-0.70      65.0%      4.25       12.0%
/// 0.70-0.80      92.2%      1.95        7.4%
/// 0.80-0.90      92.3%      1.87        7.2%
/// 0.90-1.01      93.7%      1.48        6.5%
/// ALL            92.9%      1.68        6.9%
/// deployed constant          2.41        8.3%
/// ```
///
/// 93% of bought flips sell and sell fast (median hold at conf>=0.90 is 1.7h, one
/// BIN cycle); the fee weight lives entirely in the 7% that do not. Charging the
/// liquid majority 2.41 attempts understates their profit by ~30% and pushes them
/// under the client's ROI gate, while junk at conf<0.70 is charged 2.41 against a
/// real 4.25 and looks better than it is.
///
/// ⚠️ This preserves the SHAPE and NOT the level, deliberately. The memory is
/// explicit that the 2.9x spread across bands is trustworthy but the absolute
/// anchor is not (1.68 derived from hold time vs 2.41 from real listing records —
/// possibly methodology, not error). So the table is stored NORMALISED by the
/// derived population mean and multiplied by `LIST_ATTEMPTS_EXPECTED`, making this
/// a pure REALLOCATION of fee across confidence bands with the population mean
/// left where prod already set it. Changing the level stays a separate, one-line
/// `LIST_ATTEMPTS_EXPECTED` decision that can be tuned independently.
///
/// ⚠️ Keyed on POOL confidence (`PriceStats::confidence`), never the refined
/// confidence on the flip card. `refine_confidence` takes `profit / price` as an
/// input, so keying the fee off it would make profit depend on confidence depend
/// on profit. Pool confidence is computed from samples/volume/volatility alone.
/// The bands were measured against refined confidence, so this is a proxy; it is
/// the closest non-circular one available.
///
/// ⚠️ Lanes with no pool stats (the lbin lane, the prescreen) keep the constant.
///
/// 0 = off (byte-identical: every lane uses the flat constant).
pub static LIST_ATTEMPTS_BY_CONF: LazyLock<bool> =
    LazyLock::new(|| num("LIST_ATTEMPTS_BY_CONF", 0.0) != 0.0);

/// Cap on an lbin-derived LISTING target, as a multiple of the base value.
///
/// When no sold-history and no model price exist, `est_listing` falls back to
/// `target = the single lowest live BIN`, guarded only by `lb > base * 5`. On a
/// Crown of Avarice (base 421,000,000) that permits 2.105B, so a wall of
/// 1.88-1.93B listings set our ask to **1,999,644,670** on a crown whose own
/// 36-sale pool says 700,000,000. We then paid 2.5% of that every 6h.
///
/// Clamping beats rejecting: returning None means the item is never listed at
/// all and holds a slot forever ([[finder-list-low-conf]]), so this lowers the
/// target instead. 0 = off (byte-identical).
pub static LBIN_FALLBACK_CAP: LazyLock<f64> = LazyLock::new(|| num("LBIN_FALLBACK_CAP", 0.0));

/// Hours after a purchase during which an item UUID we hold counts as our own
/// listing, so another bot in the fleet cannot buy it back.
///
/// The existing `is_own_listing` guard only learns a listing once the sweep
/// links it out of the dump, which is up to ~80s after it appears (20s BIN grace
/// + the 60s publish metronome). Measured 2026-08-09: our bots re-buy each
/// other at **+60s and +119s**, so the guard is structurally too late. 603
/// bot-to-bot trades worth 19.0B in 7d, 5.4% of capital, ~1.06B destroyed in
/// fees for nothing.
///
/// Keyed on the cost-basis clock rather than the listing clock, so it closes the
/// window without permanently blacklisting an item we later sell on. 0 = off.
pub static SELF_BUY_GUARD_H: LazyLock<f64> = LazyLock::new(|| num("SELF_BUY_GUARD_H", 0.0));

/// Let the learned-significance pass also run for bazaar-mapped features.
///
/// Pass 2 skips any feature with a bazaar product (`if
/// self.feature_value(feat).is_some() { continue; }`), so its significance is
/// decided ONLY by material cost against `max(FEATURE_MIN_VALUE, base *
/// ATTR_MIN_SHARE)`. Two consequences, both measured 2026-08-09:
///
/// - `reforge:ancient` maps to `PRECURSOR_GEAR` at 385,354 against a 2,325,000
///   bar, so it can NEVER fork a Necron's Helmet key, and is then SUBTRACTED as
///   a minor feature. The learned pass would have seen median 25.0M vs base
///   15.5M over n=1877 and forked immediately. Controlled Ancient premium among
///   recomb *5: +3.5M, i.e. 9x the stone's price.
/// - The bar scales with the item, so on a 421M Crown of Avarice it is 63.15M
///   and `recomb` (9.9M) fails outright. Nothing forks; the key pools 450M and
///   2.3B crowns together.
///
/// ⛔ This is NOT what [`SIG_TWO_SIDED`] fixed: that changes the direction and
/// sample bars INSIDE the learned pass, which these features never enter.
///
/// When on, bazaar significance becomes a FLOOR rather than a verdict: a feature
/// forks if the material cost clears the bar OR the sales say it moves price.
///
/// **Modes.** `0` = off. `1` = reforges and `recomb` only. `2` = every
/// bazaar-mapped feature.
///
/// ⛔ **Do not run mode 2.** Simulated 2026-08-09 over the live 21d refs: of
/// 2,563 base keys with >=40 refs, **1,155 gain a forking feature** and 2,315
/// key-pairs move their median, by 11% at the median and up to 756x
/// (`SKELETON_MASTER_CHESTPLATE` 119,000 -> 90,000,000). The movers are ordinary
/// enchants — `growth5`, `protection5`, `ferocious_mana6` — where the bazaar
/// price is the RIGHT measure, because the book is a thing you can buy and
/// apply, so its cost genuinely IS its premium. Mode 2 mostly discovers
/// "enchanted vs blank", shreds pools, and pushes items under `MIN_REFS`.
///
/// Mode 1 targets the cases where cost and premium are structurally unrelated:
/// - **Reforges.** The stone is a one-off consumable whose price says nothing
///   about what the reforge is worth ON the item. `PRECURSOR_GEAR` is 385,354;
///   Ancient on a recombobulated Necron's Helmet *5 is worth **+3.5M**, 9x the
///   stone. Many reforges map to `Some("")` = 0.0, which is `is_some()`, so they
///   are pinned at "insignificant" no matter what the sales say.
/// - **`recomb`.** The bar is `base * ATTR_MIN_SHARE`, so it scales with the
///   item while the Recombobulator does not. At 9.9M it clears the bar on a 15M
///   helmet and fails it on a 421M Crown of Avarice (bar 63.15M) — exactly
///   backwards, since recomb is worth MORE on dearer gear (+15.8M measured on
///   Necron *5).
pub static SIG_LEARN_OVER_BAZAAR: LazyLock<f64> =
    LazyLock::new(|| num("SIG_LEARN_OVER_BAZAAR", 0.0));

/// Is `feat` one the learned pass may overrule the bazaar on? See
/// [`SIG_LEARN_OVER_BAZAAR`] for why the answer is not "all of them".
pub fn sig_learn_applies(feat: &str) -> bool {
    match *SIG_LEARN_OVER_BAZAAR {
        m if m >= 2.0 => true,
        m if m >= 1.0 => feat == "recomb" || feat.starts_with("reforge:"),
        _ => false,
    }
}

/// Hypixel's bed/grace window: a newly created BIN is not purchasable until
/// `start + BED_GRACE_MS`. Measured 2026-07-27 as a hard 20.000s wall on the
/// paginated dump (youngest BIN ever seen 19.95s over 10 refreshes), which is
/// exactly how long the dump withholds a listing. Only a mid-bed source (the
/// per-player lookup via NetherAPI, which serves BINs 3-7s old) ever sees an
/// auction while this is still counting down. Env-tunable in case Hypixel
/// moves it.
pub static BED_GRACE_MS: LazyLock<f64> = LazyLock::new(|| num("BED_GRACE_MS", 20_000.0));

/// How long BEFORE a bed lifts we hand the flip to a bot.
///
/// Seller-follow can see a BIN ~5s after listing, i.e. **15s before it is
/// buyable** — but a bot that gets it then just sits with the auction window
/// open for 15s doing nothing. That is pure downtime on the bot AND it is the
/// "window open for a long time" pattern that has twice earned a 30-day ban.
///
/// Pushing late costs us nothing competitively: during the grace window **nobody
/// can buy the auction**, so there is no race to be early for. We only need
/// enough runway for `/viewauction` (~91ms) plus the mod's own pre-click lead.
/// 1.5s is ~5x that, and still 10x tighter than holding the window for 15s.
/// Typical gap between a dump being published and us holding its bytes. Used
/// only to estimate when a pre-API find stops being pre-API. Measured p50 ~7.1s
/// (the Cloudflare revalidation floor), not a tunable that changes behaviour.
pub static DUMP_DETECT_LAG_MS: LazyLock<f64> = LazyLock::new(|| num("DUMP_DETECT_LAG_MS", 7_200.0));

pub static BED_PUSH_LEAD_MS: LazyLock<f64> =
    LazyLock::new(|| num("BED_PUSH_LEAD_MS", 1_500.0).clamp(300.0, 19_000.0));

/// Waive the BinMaster filter's per-tier `min_confidence` and `min_volume`
/// checks for a flip whose margin is so extreme that a reference error costs
/// the BUY PRICE, not the margin.
///
/// Measured 2026-08-15 on the COFL recall join (8,735 filtered flips, 6,235
/// never posted, 110.2B): the confidence and volume tiers measure reference
/// QUALITY, but they are applied as if they measured RISK. A Fleet Topaz Drill
/// at buy 253,100 against a 22.95M reference (8214% ROI, 20.79M profit) was
/// declined `conf<88%` — even a 99% estimate error still clears profit. At that
/// distance the reference being shaky is already priced into the margin, so the
/// quality gates stop being evidence about the trade.
///
/// This is the push-side twin of [`RESCUE_MIN_MULT`]: rescue prices the
/// UNPRICEABLE obvious mispricing; this waives the tier gates for the PRICED
/// one. Both rest on the same asymmetry — under-estimating only costs flips,
/// never money, because the buy price is fixed.
///
/// Skips ONLY `min_confidence` and `min_volume` inside each Consider tier;
/// `min_profit`, `min_profit_percent`, `max_time_to_sell`, matchers and the
/// blacklist all still apply exactly. 0 = off (byte-identical to the old
/// behaviour, so the filter goldens hold).
pub static EXTREME_FLIP_MIN_PROFIT: LazyLock<f64> =
    LazyLock::new(|| num("EXTREME_FLIP_MIN_PROFIT", 0.0));
pub static EXTREME_FLIP_MIN_ROI_PCT: LazyLock<f64> =
    LazyLock::new(|| num("EXTREME_FLIP_MIN_ROI_PCT", 500.0));

/// Price a fragmented key off progressively coarser keys instead of refusing.
///
/// Measured 2026-08-15 (HIGHMISS on prod): `notpriceable` is 62% of reject
/// events and 91% of missed profit (~46.5B per era), all `basis=high`, on keys
/// like `MOLTEN_CLOAK*5#ench:ultimate_chimera5+gems:2+reforge:ancient`. 7 of the
/// top 10 misses have ≥5 BASE sales in 21 days (Hyperion has 2,220) — the item
/// is abundant, the exact FEATURE COMBO is not. This is key fragmentation, not
/// thin history ([[finder-notpriceable-is-key-fragmentation]]).
///
/// The ladder drops features from the key by ascending KNOWN value (the
/// cosmetic runes and extra fields first, the material enchants last) until a
/// pool with ≥ `MIN_REFS` prices the item, degrading confidence by
/// [`KEY_LADDER_CONF_STEP`] per dropped feature. Dropping a feature can only
/// UNDER-price a valuable variant — the coarser pool is the cheap majority —
/// so the error direction is a missed flip, never an overpay, the same safety
/// argument [`RESCUE_MIN_MULT`] already ships on.
///
/// ⛔ This is NOT lowering `MIN_REFS`: every rung is priced off its own
/// ≥`MIN_REFS` pool, never off 2 samples. Pets and skins carry no `#`-features
/// and are unaffected.
///
/// 0 = off (byte-identical to the old behaviour, goldens untouched).
pub static KEY_LADDER: LazyLock<bool> = LazyLock::new(|| num("KEY_LADDER", 0.0) != 0.0);

/// Confidence kept per dropped feature on the ladder (0.85 = -15% per rung).
pub static KEY_LADDER_CONF_STEP: LazyLock<f64> =
    LazyLock::new(|| num("KEY_LADDER_CONF_STEP", 0.85).clamp(0.1, 1.0));

/// Star-significance key gating: when ON, a star level joins the learned
/// significance pass like any other feature and forks the key ONLY when the
/// sales say it moves the price.
///
/// Without it, `base_key` unconditionally emits `*N`, so `BOUQUET_OF_LIES` has
/// eleven pools (`*1` .. `*10` + bare) even though a 1-star Bouquet sells for
/// the clean price — every starred unit lands in its own thin
/// low-volume/last-sale-fragmented pool while the liquid clean pool sits
/// unused next door. Measured 2026-08-15 22:19 UTC: Bouquet of Lies*1 at
/// 13 samples, 0.8 vol/day, last sale 98h (push refused on guard `stale`),
/// while the unstarred pool traded the same day at LBIN 17M. Stars are the
/// only dimension with this unconditional fork; every feature in the `#`
/// suffix already passes `feature_is_significant` first.
///
/// With it, Pass 2b measures each `bare|star:N` group against the unstarred
/// group with the exact same bars Pass 2 uses (one-sided by default: +15%
/// premium at ≥ `MIN_FEATURE_SAMPLES`; the flag does NOT invent new
/// thresholds). Insignificant stars FOLD: the group's refs pool under the
/// bare key, so volume/freshness/samples aggregate structurally instead of
/// waiting for the ladder. A star that genuinely moves the price (e.g. *10
/// premium gear) keeps its own key, and pool EMISSION is the only thing the
/// flag gates — OFF keeps every key byte-identical to today and leaves the
/// goldens untouched, ON only re-keyes stars the data says are price-irrelevant.
///
/// Default-to-fold when there is no unstarred comparison group at all: an
/// item that only ever trades starred gets ONE unified pool instead of ten
/// fragments. The fold error is the same one every non-forking feature
/// already carries (below the significance bar, or too thin to prove).
///
/// 0 = off (byte-identical to the old behaviour, goldens untouched).
pub static STAR_SIG: LazyLock<bool> = LazyLock::new(|| num("STAR_SIG", 0.0) != 0.0);

#[cfg(test)]
mod tests {
    use super::js_number;

    #[test]
    fn js_number_matches_js_coercion() {
        // The quirk that actually bites: set-but-empty is 0, not a fallback.
        assert_eq!(js_number(""), 0.0);
        assert_eq!(js_number("   "), 0.0);
        // Plain decimals (what prod actually sets).
        assert_eq!(js_number("300000"), 300_000.0);
        assert_eq!(js_number("10000000000"), 10_000_000_000.0);
        assert_eq!(js_number("0.6"), 0.6);
        assert_eq!(js_number("1.15"), 1.15);
        assert_eq!(js_number("36500"), 36_500.0);
        assert_eq!(js_number("-5"), -5.0);
        assert_eq!(js_number("+5"), 5.0);
        assert_eq!(js_number("1e3"), 1000.0);
        assert_eq!(js_number(" 42 "), 42.0);
        // Radix literals.
        assert_eq!(js_number("0x10"), 16.0);
        assert_eq!(js_number("0b101"), 5.0);
        assert_eq!(js_number("0o17"), 15.0);
        assert!(js_number("-0x10").is_nan()); // signed radix => NaN in JS
                                              // Infinity coerces like JS (then `num` finite-gates it to the default).
        assert!(js_number("Infinity").is_infinite());
        // Rust's parser would accept these; JS does not.
        assert!(js_number("inf").is_nan());
        assert!(js_number("nan").is_nan());
        assert!(js_number("infinity").is_nan());
        assert!(js_number("abc").is_nan());
        assert!(js_number("12abc").is_nan());
    }
}
