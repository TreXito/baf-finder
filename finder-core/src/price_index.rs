//! Port of `baf-flip-finder/src/priceIndex.ts` — THE money core.
//! Behavior is fixed by `goldens/priceIndex/queries.json` (267 queries) replayed
//! over the 30,942-ref slice with the pinned clock. Ported verbatim, including:
//!  - the `gk.slice(0, indexOf('|'))` first-`|` split (variant baseKeys can carry
//!    '|', so their features never get statistical significance — bug-compatible),
//!  - seller-dedup with empty seller → unique `anon:N` (Deviation D3),
//!  - NaN/±Infinity via math.rs (Deviation D1),
//!  - Date.now() pinned to `now_ms` (goldens use pinnedNowMs).

use crate::bazaar::Bazaar;
use crate::config::*;
use crate::math::{median, median_of, min_max, percentile, stddev, subset_multiset};
use crate::nbt::ItemAttributes;
use crate::pet_levels::pet_level_band;
use indexmap::{IndexMap, IndexSet};
use rustc_hash::FxBuildHasher;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

/// A stored reference auction used to price keys.
#[derive(Debug, Clone, Deserialize)]
pub struct Reference {
    pub price: f64,
    #[serde(rename = "soldAt")]
    pub sold_at: f64,
    pub seller: String,
    /// Observed time-to-sell (ms) = sold_at - listing.start, when the listing was
    /// tracked. Feeds the fair-price TTS estimate (volume→TTS migration, Phase 0).
    /// Nullable + defaulted so goldens/oracle JSON without it still load.
    #[serde(default, rename = "ttsMs")]
    pub tts_ms: Option<f64>,
    pub attrs: ItemAttributes,
}

#[derive(Debug, Clone)]
struct PoolEntry {
    price: f64,
    sold_at: f64,
    seller: String,
    tts_ms: Option<f64>,
}

/// Observed fair-price time-to-sell for a key (volume→TTS migration, Phase 0).
/// `fair_tts_h` is the MEDIAN hours-to-sell among sales within ±15% of the key's
/// median price; cheap flips (which clear fast because they are underpriced, not
/// because demand is real) are excluded. Survivorship-biased until the `censored`
/// table fills (only sold listings have tts_ms) — measurement only, never gates.
#[derive(Debug, Clone)]
pub struct TtsInfo {
    pub fair_tts_h: f64,
    pub n_fair: i64,
    pub n_all: i64,
    /// P(a listing of this ITEM sells within [`TTS_SELL_HORIZON_H`] hours),
    /// Kaplan-Meier over sold listings (events) and `censored` ones (listings
    /// still unsold when we stopped watching). `None` when no censored data
    /// exists for the item, which is the cold-start case and must NOT be read as
    /// illiquid — see [`crate::survival`].
    ///
    /// This is the honest half of the TTS story. `fair_tts_h` says how fast the
    /// winners won; this says how many listings won at all. An item can read
    /// 0.22h on the first and 2% on the second.
    pub sell_through: Option<f64>,
    /// Censored (never-sold) listings backing `sell_through`, item-level.
    pub n_censored: i64,
}

#[derive(Debug, Clone)]
struct CleanVal {
    median: f64,
    count: i64,
    volume_per_day: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct KeyStats {
    pub target: f64,
    pub samples: i64,
    pub volume_per_day: f64,
    pub spread_pct: f64,
    pub lowest_ref: f64,
    pub highest_ref: f64,
    pub last_sold_ago_h: f64,
    pub confidence: f64,
    pub volatility: f64,
    pub manipulated: bool,
    pub trend_pct: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SnipeStats {
    pub target: f64,
    pub samples: i64,
    pub volume_per_day: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DominanceStats {
    pub target: f64,
    pub samples: i64,
    pub volume_per_day: f64,
    pub manipulated: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ThinKeyEvidence {
    pub median: f64,
    pub n: i64,
}

/// JS number → string for key building ("5" for 5.0, "5.5" for 5.5).
fn jn(x: f64) -> String {
    let mut s = String::new();
    push_jn(&mut s, x);
    s
}

/// [`jn`] appended straight onto a buffer, byte-identical, no intermediate alloc.
///
/// Key building is the rebuild's hot loop: `base_key` runs 3x per reference and
/// `candidate_features` once, so a 3.7M-ref rebuild makes ~11M + ~15M of these.
/// Every one used to allocate a String via `format!` only to be interpolated into
/// a second `format!` and dropped.
///
/// The fast path is exact, not an approximation. For an integral f64 with
/// |x| < 1e15 the i64 conversion is lossless and Rust's `Display` for f64 emits
/// those same digits with no decimal point, so the bytes match.
///
/// ⚠️ Two values must NOT take it. `-0.0` formats as `-0` as a float but `0` as an
/// integer, and anything past 1e15 loses integer precision. Both fall through to
/// the identical `write!` the old code used, so output is preserved exactly.
fn push_jn(s: &mut String, x: f64) {
    if x.fract() == 0.0 && x.abs() < 1e15 && !(x == 0.0 && x.is_sign_negative()) {
        let mut buf = itoa_buf();
        s.push_str(fmt_i64(&mut buf, x as i64));
        return;
    }
    use std::fmt::Write;
    let _ = write!(s, "{x}");
}

/// Stack buffer big enough for any i64 in decimal, sign included.
pub(crate) fn itoa_buf() -> [u8; 20] {
    [0u8; 20]
}

/// i64 → decimal into a caller-owned buffer. No allocation, no `core::fmt`.
pub(crate) fn fmt_i64(buf: &mut [u8; 20], mut v: i64) -> &str {
    if v == 0 {
        return "0";
    }
    let neg = v < 0;
    let mut i = buf.len();
    // Accumulate on the NEGATIVE side so i64::MIN cannot overflow on negation.
    if !neg {
        v = -v;
    }
    while v != 0 {
        i -= 1;
        buf[i] = b'0' + (-(v % 10)) as u8;
        v /= 10;
    }
    if neg {
        i -= 1;
        buf[i] = b'-';
    }
    // Every byte written above is ASCII.
    std::str::from_utf8(&buf[i..]).unwrap_or("0")
}

/// Stack size as a key component: `x64` for a 64-stack, empty for a single.
///
/// Deliberately part of the BASE key rather than a feature, and deliberately not
/// a divisor -- see [`NBT_COUNT`]. Applied on every branch, including pets and
/// books, so the invariant is unconditional: two listings with different stack
/// sizes never share a key.
fn qty_suffix(a: &ItemAttributes) -> String {
    let mut s = String::new();
    push_qty_suffix(&mut s, a);
    s
}

/// [`qty_suffix`] appended in place. Empty for the overwhelmingly common
/// single-item case, so the usual cost is one branch and no allocation at all.
fn push_qty_suffix(s: &mut String, a: &ItemAttributes) {
    if *NBT_COUNT && a.count > 1 {
        s.push('x');
        let mut buf = itoa_buf();
        s.push_str(fmt_i64(&mut buf, a.count as i64));
    }
}

/// Dispersion of a key's prices, as a fraction of its median, clamped to 1.0.
///
/// `prices` must be sorted ascending. See [`crate::config::ROBUST_VOLATILITY`]
/// for why the standard deviation is the wrong estimator here: a single
/// coin-transfer sale pins the result at the clamp, which caps confidence at 0.6
/// against a 0.85 floor and rejects the key wholesale.
fn price_dispersion(prices: &[f64], median: f64, robust: bool) -> f64 {
    if median <= 0.0 || prices.is_empty() {
        return 1.0;
    }
    let spread = if robust {
        // IQR/1.349 is the normal-consistency estimate of sigma.
        (percentile(prices, 0.75) - percentile(prices, 0.25)) / 1.349
    } else {
        stddev(prices)
    };
    (spread / median).min(1.0)
}

/// The recent-sales sample `compute_price_for` forms `short_term` from, sorted.
///
/// A FIXED short-term window silently disables itself on any key selling under
/// ~6/day: `recent` never reaches 3, so `short_term` collapses to `long_term`,
/// `trend_pct` is pinned at 0, and BOTH recency mechanisms — the
/// `min(short_term, long_term)` target and the `trend_pct <= -0.15` falling
/// reject — go dead. That happens on exactly the thin, high-value keys where a
/// stale price is least survivable.
///
/// Measured cost of not widening (2026-08-02): `HEGEMONY_ARTIFACT` fell 30% in
/// four days on a supply shock (clean 07-30 560M -> 08-02 480M, 1-3 sales/day
/// rising past 100). The recomb+enriched key kept quoting its 21-day median of
/// 690M the whole way down, and we bought 7 at 492-530M believing a 25%
/// discount while actually paying at or above the falling market; 6 never sold.
/// At a 24h window the 07-31 short-term median is 532M against a 530M ask,
/// which dies on margin, and `trend_pct` is -0.23, which trips the reject.
///
/// `widen_h <= base_h` reproduces the fixed window exactly, which is what the
/// price-index goldens pin.
fn recent_window(deduped: &[(f64, f64)], now_s: f64, base_h: f64, widen_h: f64) -> Vec<f64> {
    widen_until(base_h, widen_h, 3, |hours| {
        let mut v: Vec<f64> = deduped
            .iter()
            .filter(|e| e.1 >= now_s - hours * 3600.0)
            .map(|e| e.0)
            .collect();
        sort_asc(&mut v);
        v
    })
}

/// The widening ladder itself, shared so the two recency windows cannot drift.
///
/// `compute_price_for` (via [`recent_window`], min 3) and
/// [`PriceIndex::compute_base_trend_pct`] (min 5) ask the same question — "enough
/// recent sales to say something about NOW" — and differ only in how many samples
/// they need and what they collect. They were written separately, and the trend
/// half kept a hardcoded 12h→24h ladder that `SHORT_TERM_WIDEN_HOURS` never
/// reached. Below 5 sales/day it therefore returned 0.0, i.e. "flat", on an item
/// that was falling.
///
/// Measured cost (2026-08-03): `RELIC_OF_COINS` trades ~1.8/day. 12h had 2 sales
/// and 24h had 3, so the trend read FLAT while the item was down 22% (7-21d
/// median 457M, last 4d 357M). `estimate_for` consequently applied neither its
/// `trend <= -0.15` refusal nor its `est *= 1+trend` discount, quoted 409.5M, and
/// we bought at 340M against a market that had already moved to ~350M. At the 48h
/// the config actually asks for, n=5 and trend is -15.6%, which refuses outright.
/// 53% of items worth 10M+ were blind this way, `INFERNAL_CRIMSON_BOOTS`
/// (349M→265M) and `ARTIFACT_OF_COINS` (87.5M→67.1M) among them.
///
/// `widen_h <= base_h` reproduces the fixed window exactly, which is what the
/// price-index goldens pin.
/// Rungs must strictly ascend, so `last` skips a widen target that repeats 24h
/// rather than collecting it twice.
fn widen_until<T>(
    base_h: f64,
    widen_h: f64,
    min_n: usize,
    mut collect: impl FnMut(f64) -> Vec<T>,
) -> Vec<T> {
    let mut out = collect(base_h);
    if out.len() >= min_n {
        return out;
    }
    let mut last = base_h;
    for h in [24.0, widen_h] {
        if h <= last || h > widen_h {
            continue;
        }
        out = collect(h);
        last = h;
        if out.len() >= min_n {
            break;
        }
    }
    out
}

/// The base key identifies an item structurally.
pub fn base_key(a: &ItemAttributes) -> String {
    let qty = qty_suffix(a);
    if let Some(pet) = &a.pet {
        let band = pet_level_band(&pet.pet_type, &pet.tier, pet.exp);
        let candy = if pet.candied && band != "max" {
            ":candy"
        } else {
            ""
        };
        return format!("PET:{}:{}:{}{}{}", pet.pet_type, pet.tier, band, candy, qty);
    }
    if a.id == "ENCHANTED_BOOK" {
        let mut ench: Vec<(String, f64)> = a
            .enchantments
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        // Object.entries(...).sort() = sort by the "k,v" string.
        ench.sort_by(|x, y| format!("{},{}", x.0, jn(x.1)).cmp(&format!("{},{}", y.0, jn(y.1))));
        let joined = ench
            .iter()
            .map(|(k, v)| format!("{k}={}", jn(*v)))
            .collect::<Vec<_>>()
            .join(",");
        return format!(
            "BOOK:{}{}",
            if joined.is_empty() { "plain" } else { &joined },
            qty
        );
    }
    // The hottest line in the rebuild: 3 base_key calls per reference. The old
    // shape allocated up to four Strings (stars, the jn inside it, variant, and
    // the final format!) to concatenate four known pieces. Same bytes, one alloc.
    let mut out = String::with_capacity(a.id.len() + qty.len() + a.variant.len() + 8);
    out.push_str(&a.id);
    if let Some(l) = a.upgrade_level {
        out.push('*');
        push_jn(&mut out, l);
    }
    out.push_str(&qty);
    // With VARIANT_SIG the variant is emitted as `var:` features instead, so it
    // goes through significance like everything else. Keeping it here as well
    // would double-count it AND keep the base group fragmented.
    if !a.variant.is_empty() && !*VARIANT_SIG {
        out.push('~');
        out.push_str(&a.variant);
    }
    out
}

/// Does a feature move price enough, in EITHER direction, to deserve its own key?
///
/// See [`SIG_TWO_SIDED`]. `two_sided = false` is the historical one-sided test and
/// is what the price-index goldens pin.
///
/// The two directions carry ASYMMETRIC RISK and so take different sample bars.
/// Forking a DEARER feature splits a pool we then price off, so it wants real
/// evidence. Forking a CHEAPER one strands the minority below `MIN_REFS`, which
/// makes the item notpriceable and the flip simply never fires — the cost of
/// being wrong is a missed flip, not an overpay. `cheap_min_samples` may
/// therefore sit below `min_samples`.
fn feature_is_significant(
    n: usize,
    feat_median: f64,
    base: f64,
    min_share: f64,
    min_samples: usize,
    cheap_min_samples: usize,
    two_sided: bool,
) -> bool {
    if base <= 0.0 {
        return false;
    }
    let delta = feat_median - base;
    let bar = base * min_share;
    if delta >= bar {
        return n >= min_samples;
    }
    if two_sided && -delta >= bar {
        return n >= cheap_min_samples.max(1);
    }
    false
}

/// The lowest feature median that sits far enough under `target` to bind.
///
/// Pure half of [`PriceIndex::feature_floor`]. Takes the MINIMUM rather than the
/// first match: an item can carry several under-represented features and the
/// cheapest is the binding evidence. Returns `None` when nothing clears
/// `min_gap`, so a pool that merely disagrees a little changes nothing.
fn binding_feature_floor(medians: &[f64], target: f64, min_gap: f64) -> Option<f64> {
    if target <= 0.0 {
        return None;
    }
    let bar = target * (1.0 - min_gap);
    medians
        .iter()
        .copied()
        .filter(|m| *m > 0.0 && *m < bar)
        .min_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal))
}

/// How much of a key's target survives the item's sell-through.
///
/// Pure half of the [`LIQ_DISCOUNT`] haircut, so the calibration is directly
/// testable without an index. Linear below `pivot`, clamped at `floor`, and
/// **never above 1.0** — this may only ever lower a target.
///
/// `st = None` is UNKNOWN, not illiquid, and returns 1.0. Cold-start items have
/// no censored history, and the 2026-08-07 measurement backs this up: our buys
/// on unknown-sell-through items cleared at 80.1%, i.e. exactly the overall
/// average, so there is nothing to correct for there.
fn liquidity_factor(st: Option<f64>, pivot: f64, k: f64, floor: f64) -> f64 {
    let Some(st) = st.filter(|v| v.is_finite()) else {
        return 1.0;
    };
    let shortfall = (pivot - st).max(0.0);
    (1.0 - k * shortfall).clamp(floor.clamp(0.0, 1.0), 1.0)
}

/// [`liquidity_factor`] at the live configuration, for logging and for callers
/// outside this module. Returns 1.0 whenever [`LIQ_DISCOUNT`] is off, so a log
/// line always states the factor that was really applied.
pub fn liquidity_factor_for(st: Option<f64>) -> f64 {
    if !*LIQ_DISCOUNT {
        return 1.0;
    }
    liquidity_factor(
        st,
        *LIQ_DISCOUNT_PIVOT,
        *LIQ_DISCOUNT_K,
        *LIQ_DISCOUNT_FLOOR,
    )
}

/// Collapse `enrich_critical_chance`, `enrich_magic_find`, … to one `enrich`.
///
/// See [`ENRICH_POOL`] for the measurement. Every type is worth the same +8.4M,
/// so the only thing the per-type fork buys is seven pools too thin to have a
/// recent price. Applies to indexing and lookup alike because both sides build
/// their key through here.
fn pooled_extra(n: &str) -> &str {
    pooled_extra_inner(n, *ENRICH_POOL)
}

/// Pure half of [`pooled_extra`], flag injected so both states are testable
/// without touching process env.
fn pooled_extra_inner(n: &str, on: bool) -> &str {
    if on && n.starts_with("enrich_") {
        "enrich"
    } else {
        n
    }
}

/// Candidate value-bearing feature tokens on an item (pre-significance).
pub fn candidate_features(a: &ItemAttributes) -> Vec<String> {
    // ⚠️ Every string here is built with push_str rather than `format!`, and the
    // ORDER and BYTES are unchanged -- `price_index_golden` and `sniper_golden`
    // pin these exact feature names, so this is a pure allocation/formatting
    // change. `format!` pays for an Arguments array and a dynamic-dispatch walk
    // through core::fmt per piece; concatenating two known strs does not need any
    // of that, and this runs ~15x per reference over millions of references.
    let mut f: Vec<String> = Vec::new();
    // One helper for the overwhelmingly common "prefix + one str" shape.
    fn pre(p: &str, s: &str) -> String {
        let mut out = String::with_capacity(p.len() + s.len());
        out.push_str(p);
        out.push_str(s);
        out
    }
    for (n, t) in &a.attributes {
        let mut s = String::with_capacity(5 + n.len() + 4);
        s.push_str("attr:");
        s.push_str(n);
        push_jn(&mut s, *t);
        f.push(s);
    }
    if a.id != "ENCHANTED_BOOK" {
        for (n, l) in &a.enchantments {
            let mut s = String::with_capacity(5 + n.len() + 4);
            s.push_str("ench:");
            s.push_str(n);
            push_jn(&mut s, *l);
            f.push(s);
        }
    }
    for s in &a.scrolls {
        f.push(pre("scroll:", s));
    }
    if let Some(r) = &a.reforge {
        f.push(pre("reforge:", r));
    }
    if let Some(s) = &a.skin {
        f.push(pre("skin:", s));
    }
    if a.recombobulated {
        f.push("recomb".to_string());
    }
    if a.gem_slots > 0 {
        let mut s = String::with_capacity(9);
        s.push_str("gems:");
        let mut buf = itoa_buf();
        s.push_str(fmt_i64(&mut buf, a.gem_slots as i64));
        f.push(s);
    }
    for g in &a.gems {
        f.push(pre("gem:", g));
    }
    for p in &a.parts {
        f.push(pre("part:", p));
    }
    if let Some(pet) = &a.pet {
        if let Some(h) = &pet.held_item {
            f.push(pre("pethled:", h));
        }
        if let Some(s) = &pet.skin {
            f.push(pre("petskin:", s));
        }
    }
    for (n, cnt) in &a.extras {
        let n = pooled_extra(n);
        if *cnt > 1.0 {
            let mut s = String::with_capacity(2 + n.len() + 5);
            s.push_str("x:");
            s.push_str(n);
            s.push('*');
            push_jn(&mut s, *cnt);
            f.push(s);
        } else {
            f.push(pre("x:", n));
        }
    }
    // Variant parts as candidate features (build_variant joins with '|'), so
    // pass-2 significance measures what each is actually worth against sales of
    // the same item without it.
    if *VARIANT_SIG && !a.variant.is_empty() {
        for part in a.variant.split('|') {
            if !part.is_empty() {
                f.push(format!("var:{part}"));
            }
        }
    }
    f
}

/// Features that ALWAYS split the key.
///
/// `recomb` was tried here (same reasoning as skin/tier-boost: always a
/// meaningfully different, more valuable item, and the learned-significance
/// pass has a cold-start problem for rare-but-large features). Reverted:
/// `craft_cost.rs::zero_star_baseline` and the higher-star cap lookup in
/// `PriceIndex::price_cap_from_higher_stars` both clone an item and re-key it
/// via `final_key` to find a comparable baseline/cap — forcing `recomb` into
/// the key broke those synthetic lookups (a 0-star-but-recombobulated key is
/// real but nearly sample-free; nobody recombs before starring), tanking
/// `craft_cost_golden_parity` from 0 to 8-10 mismatches, several off by
/// hundreds of millions, all in the direction of an artificially LOW ceiling
/// (i.e. suppressing real flips on exactly the recombobulated high-end gear
/// this was meant to help). A per-callsite fix (e.g. clearing recomb only on
/// the synthetic clone) made it worse for DIVAN_CHESTPLATE, so the
/// relationship isn't a simple "always strip it" — needs a real design pass,
/// not a quick patch. Left to the existing learned-significance system.
///
/// NOTE: this was never the explanation for the 164,000,000-vs-320k Enderman
/// EPIC pet gap found while investigating it — that gap is still open; recomb
/// cannot be the cause since Hypixel doesn't allow recombobulating pets at
/// all, so `a.recombobulated` is always false for them.
pub fn always_splits(f: &str) -> bool {
    f.starts_with("skin:")
        || f.starts_with("petskin:")
        || f == "x:shiny"
        || f == "pethled:PET_ITEM_TIER_BOOST"
}

/// Dominance predicate: candidate `c` is same-or-better than reference `r` on
/// every value axis (both share a base key).
pub fn dominates_ref(c: &ItemAttributes, r: &ItemAttributes) -> bool {
    for (k, v) in &r.attributes {
        if c.attributes.get(k).copied().unwrap_or(0.0) < *v {
            return false;
        }
    }
    for (k, v) in &r.enchantments {
        if c.enchantments.get(k).copied().unwrap_or(0.0) < *v {
            return false;
        }
    }
    for (k, v) in &r.extras {
        if c.extras.get(k).copied().unwrap_or(0.0) < *v {
            return false;
        }
    }
    if r.recombobulated && !c.recombobulated {
        return false;
    }
    if r.reforge.is_some() && c.reforge != r.reforge {
        return false;
    }
    if r.skin.is_some() && c.skin != r.skin {
        return false;
    }
    if r.gem_slots > c.gem_slots {
        return false;
    }
    if !subset_multiset(&r.scrolls, &c.scrolls) {
        return false;
    }
    if !subset_multiset(&r.gems, &c.gems) {
        return false;
    }
    if !subset_multiset(&r.parts, &c.parts) {
        return false;
    }
    if let Some(rp) = &r.pet {
        match &c.pet {
            None => return false,
            Some(cp) => {
                if rp.held_item.is_some() && cp.held_item != rp.held_item {
                    return false;
                }
                if rp.skin.is_some() && cp.skin != rp.skin {
                    return false;
                }
            }
        }
    }
    true
}

pub struct PriceIndex {
    refs: Vec<Reference>,
    now_ms: i64,
    bazaar: Bazaar,
    base_value: FxHashMap<String, f64>,
    base_high: FxHashMap<String, f64>,
    base_sold_count: FxHashMap<String, i64>,
    clean_value: FxHashMap<String, CleanVal>,
    significant: FxHashSet<String>,
    /// Learned `bare_bk|star:N` groups whose premium passes the Pass-2 bars.
    /// Filled by Pass 2b regardless of [`STAR_SIG`]; the flag gates only the
    /// EMISSION in [`PriceIndex::final_key`], so the learned set exists either
    /// way and tests can interrogate it without racing the env LazyLock.
    star_significant: FxHashSet<String>,
    /// `bk|feat` -> median price of sales carrying that feature. Populated only
    /// when `POOL_REPR_GUARD` is on; see [`PriceIndex::feature_floor`].
    feature_median: FxHashMap<String, f64>,
    sig_sig_by_bk: FxHashMap<String, String>,
    price_pool: FxHashMap<String, Vec<PoolEntry>>,
    /// Per-key observed fair-price TTS (volume→TTS migration, Phase 0). Additive,
    /// non-gating: nothing in the pricing/filter path reads it yet.
    fair_tts: FxHashMap<String, TtsInfo>,
    /// Item-level hours-observed for listings that were still unsold when the
    /// censor sweep took them (`censored.lifetime_ms`). Empty ⇒ every
    /// `sell_through` is `None` and behaviour is exactly pre-censoring.
    censored_h_by_item: FxHashMap<String, Vec<f64>>,
    /// Item id → Kaplan-Meier P(sells within `TTS_SELL_HORIZON_H`).
    ///
    /// Separate from `fair_tts` on purpose: that map only gets an entry when the
    /// key has a *fair-priced* sale carrying a `tts_ms`, which is a much narrower
    /// condition than "we know how often this item sells". The liquidity discount
    /// wants the broader answer, so it reads this directly.
    sell_through_by_item: FxHashMap<String, f64>,
    /// Final key → item id. ⚠️ A key is NOT parseable back to an item id: pets key
    /// as `PET:{type}:{tier}:{band}` and books as `BOOK:{ench}`, neither of which
    /// carries `attrs.id`. Populated alongside the survival data only.
    key_item: FxHashMap<String, String>,
    base_refs_by_key: FxHashMap<String, Vec<usize>>,
    // Per-key memos (interior-mutable; transparent caches — the index is immutable
    // after build, so they never need clearing). Mirror the TS pfMemo/quickTargetMemo/
    // trendMemo: screen + median both evaluate each candidate, so without these Rust
    // does ~2x the priceFor/quickTarget work.
    pf_memo: std::cell::RefCell<FxHashMap<String, Option<KeyStats>>>,
    quick_target_memo: std::cell::RefCell<FxHashMap<String, Option<f64>>>,
    trend_memo: std::cell::RefCell<FxHashMap<String, f64>>,
}

fn sort_asc(xs: &mut [f64]) {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
}

/// The star suffix `*N` sits between the id and the qty (`BOUQUET_OF_LIES*1`,
/// `X*1x3`) and is the only `*` a key's base portion can carry: ids come from
/// an enum-stripped string and the qty suffix is `x{n}`. A `~variant` tail
/// rides along untouched. Feature values like `x:hpb*10` live behind '#', so
/// this must only ever run on the base portion. Returns the base without the
/// `*N` level; `None` when there is no star suffix — or the char after `*` is
/// not a digit, which is not a star separator worth guessing at.
fn strip_star_suffix(base: &str) -> Option<String> {
    let pos = base.find('*')?;
    let after = &base[pos + 1..];
    let digits = after
        .bytes()
        .take_while(|b| b.is_ascii_digit() || *b == b'.')
        .count();
    if digits == 0 {
        return None;
    }
    Some(format!("{}{}", &base[..pos], &after[digits..]))
}

impl PriceIndex {
    fn now_secs(&self) -> f64 {
        self.now_ms as f64 / 1000.0
    }

    pub fn key_count(&self) -> usize {
        self.price_pool.len()
    }

    /// Observed fair-price TTS for a key (volume→TTS migration, Phase 0). None when
    /// the key has no fair-priced sale with a recorded tts_ms. Measurement only.
    pub fn tts_info(&self, key: &str) -> Option<&TtsInfo> {
        self.fair_tts.get(key)
    }

    /// Kaplan-Meier P(a listing of this item sells within the horizon).
    ///
    /// `None` means unknown — no censored data for the item — which is the
    /// cold-start case and must never be read as illiquid.
    pub fn sell_through_for_item(&self, item_id: &str) -> Option<f64> {
        self.sell_through_by_item.get(item_id).copied()
    }

    /// [`PriceIndex::sell_through_for_item`] via a final key, for callers that
    /// hold a key rather than the attrs (the FLIP logger). Goes through the
    /// stored map because a key cannot be parsed back to an item id.
    pub fn sell_through_for_key(&self, key: &str) -> Option<f64> {
        self.key_item
            .get(key)
            .and_then(|i| self.sell_through_for_item(i))
    }

    /// Bazaar accessors (craftCost/modifierModel reach the index's bazaar the way
    /// the TS reaches the module-global one).
    pub fn bazaar_ready(&self) -> bool {
        self.bazaar.bazaar_ready(self.now_ms)
    }
    pub fn bazaar_price(&self, id: &str) -> Option<f64> {
        self.bazaar.bazaar_price(id)
    }

    /// The references this index was built from.
    ///
    /// `build` takes ownership of the whole `Vec` and keeps it, so callers that
    /// also need the refs afterwards (`ModifierModel::rebuild`) must NOT clone
    /// them in — at a 14-day window that is 2.52M refs at ~1.57KB each, i.e. a
    /// **~4GB** second copy, which is what made `bg rebuild` double RSS and
    /// capped how far `REF_MAX_AGE_DAYS` could be raised. Read them back through
    /// here instead: `&idx.references()` and `&idx` are both shared borrows, so
    /// they coexist.
    pub fn references(&self) -> &[Reference] {
        &self.refs
    }

    pub fn build(refs: Vec<Reference>, bazaar: Bazaar, now_ms: i64) -> Self {
        Self::build_with_survival(refs, bazaar, now_ms, FxHashMap::default())
    }

    /// `build`, plus the right-censored half of the time-to-sell data: per item
    /// id, the hours each never-sold listing was observed for.
    ///
    /// Passing an empty map is identical to [`PriceIndex::build`] — every
    /// `TtsInfo::sell_through` comes back `None` and nothing downstream changes.
    /// That is what the goldens and the COMPARE path do.
    pub fn build_with_survival(
        refs: Vec<Reference>,
        bazaar: Bazaar,
        now_ms: i64,
        censored_h_by_item: FxHashMap<String, Vec<f64>>,
    ) -> Self {
        let mut idx = PriceIndex {
            censored_h_by_item,
            refs,
            now_ms,
            bazaar,
            base_value: FxHashMap::default(),
            base_high: FxHashMap::default(),
            base_sold_count: FxHashMap::default(),
            clean_value: FxHashMap::default(),
            significant: FxHashSet::default(),
            star_significant: FxHashSet::default(),
            feature_median: FxHashMap::default(),
            sig_sig_by_bk: FxHashMap::default(),
            price_pool: FxHashMap::default(),
            fair_tts: FxHashMap::default(),
            sell_through_by_item: FxHashMap::default(),
            key_item: FxHashMap::default(),
            base_refs_by_key: FxHashMap::default(),
            pf_memo: std::cell::RefCell::new(FxHashMap::default()),
            quick_target_memo: std::cell::RefCell::new(FxHashMap::default()),
            trend_memo: std::cell::RefCell::new(FxHashMap::default()),
        };
        idx.build_inner();
        idx
    }

    fn build_inner(&mut self) {
        let cutoff = self.now_secs() - *REF_MAX_AGE_DAYS * 86400.0;
        let fresh: Vec<usize> = (0..self.refs.len())
            .filter(|&i| self.refs[i].sold_at >= cutoff && self.refs[i].price > 0.0)
            .collect();

        // Pass 1: base value per base key.
        let mut base_groups: IndexMap<String, Vec<f64>, FxBuildHasher> = IndexMap::default();
        for &i in &fresh {
            let bk = base_key(&self.refs[i].attrs);
            base_groups.entry(bk).or_default().push(self.refs[i].price);
        }
        for (bk, prices) in base_groups.iter_mut() {
            sort_asc(prices);
            let len = prices.len();
            self.base_sold_count.insert(bk.clone(), len as i64);
            let bv_idx = (len as f64 * 0.2).floor() as usize;
            let bv = prices
                .get(bv_idx)
                .copied()
                .unwrap_or_else(|| median(prices));
            self.base_value.insert(bk.clone(), bv);
            let bh_idx = (len as f64 * 0.95).floor() as usize;
            let bh = prices
                .get(bh_idx)
                .copied()
                .or_else(|| prices.last().copied())
                .unwrap_or(0.0);
            self.base_high.insert(bk.clone(), bh);
        }

        // Pass 2: feature significance.
        let mut feat_groups: IndexMap<String, Vec<f64>, FxBuildHasher> = IndexMap::default();
        for &i in &fresh {
            let bk = base_key(&self.refs[i].attrs);
            for feat in candidate_features(&self.refs[i].attrs) {
                let gk = format!("{bk}|{feat}");
                feat_groups.entry(gk).or_default().push(self.refs[i].price);
            }
        }
        let mut significant_ordered: IndexSet<String, FxBuildHasher> = IndexSet::default();
        for (gk, prices) in feat_groups.iter_mut() {
            // gk.slice(0, indexOf('|')) / slice(indexOf('|')+1) — FIRST '|'.
            let (bk, feat) = gk.split_once('|').unwrap();
            // A bazaar price used to end the question here, so a feature whose
            // observed premium dwarfs its material cost never got measured.
            // `reforge:ancient` is PRECURSOR_GEAR at 385,354 against a 2,325,000
            // bar and so could never fork a Necron's Helmet key, while its
            // learned evidence is median 25.0M vs base 15.5M over n=1877. Under
            // `SIG_LEARN_OVER_BAZAAR` we measure it anyway and let
            // `feature_forks_key` take the OR.
            if !sig_learn_applies(feat) && self.feature_value(feat).is_some() {
                continue;
            }
            // Cheapest gate either direction could need, so a group too thin for
            // both still short-circuits before the sort.
            let floor_n = if *SIG_TWO_SIDED {
                (*MIN_FEATURE_SAMPLES).min((*SIG_CHEAP_MIN_SAMPLES).max(1))
            } else {
                *MIN_FEATURE_SAMPLES
            };
            if prices.len() < floor_n {
                continue;
            }
            let base = self.base_value.get(bk).copied().unwrap_or(0.0);
            if base <= 0.0 {
                continue;
            }
            sort_asc(prices);
            let m = median(prices);
            // Retain the median for features that do NOT go on to fork the key.
            // Pass 2 already paid for it, and it is the only record of what a
            // non-significant feature is worth — which is what `feature_floor`
            // needs to notice that this pool is not evidence for THIS item.
            if *POOL_REPR_GUARD && prices.len() >= *POOL_REPR_MIN_SAMPLES {
                self.feature_median.insert(gk.clone(), m);
            }
            if feature_is_significant(
                prices.len(),
                m,
                base,
                *ATTR_MIN_SHARE,
                *MIN_FEATURE_SAMPLES,
                *SIG_CHEAP_MIN_SAMPLES,
                *SIG_TWO_SIDED,
            ) {
                self.significant.insert(gk.clone());
                significant_ordered.insert(gk.clone());
            }
        }

        // Pass 2b: star significance. Stars are the one dimension that forks
        // unconditionally — `base_key` bakes `*N` in even when the level is
        // price-irrelevant (a 1-star Bouquet of Lies sells for the clean
        // price, yet gets its own eleven-family of thin pools; measured
        // 2026-08-15: `BOUQUET_OF_LIES*1` at 13 samples / 0.8 vol / last sale
        // 98h while the unstarred pool traded LBIN 17M that day). Same exact
        // bars as Pass 2, default one-sided: only a measured PREMIUM keeps
        // `star:N` forking; anything else folds into the bare pool when
        // [`STAR_SIG`] is on. Comparison base is the STRICT-unstarred Pass-1
        // group (an unstarred ref's bk is already the bare one), so an item
        // that never traded unstarred has no bar and stays folded — one
        // unified pool of its starred refs instead of ten fragments, which is
        // still strictly more samples than any shard had. Runs unconditionally
        // (cheap: one group push per starred ref, versus one per feature in
        // Pass 2) so the learned set is inspectable; the flag gates only the
        // key EMISSION in `final_key`.
        let mut star_groups: IndexMap<String, Vec<f64>, FxBuildHasher> = IndexMap::default();
        for &i in &fresh {
            let a = &self.refs[i].attrs;
            let Some(l) = a.upgrade_level else { continue };
            if a.pet.is_some() || a.id == "ENCHANTED_BOOK" {
                continue;
            }
            let Some(bare) = strip_star_suffix(&base_key(a)) else {
                continue;
            };
            star_groups
                .entry(format!("{bare}|star:{}", jn(l)))
                .or_default()
                .push(self.refs[i].price);
        }
        for (gk, prices) in star_groups.iter_mut() {
            let (bare, _feat) = gk.split_once('|').unwrap();
            let floor_n = if *SIG_TWO_SIDED {
                (*MIN_FEATURE_SAMPLES).min((*SIG_CHEAP_MIN_SAMPLES).max(1))
            } else {
                *MIN_FEATURE_SAMPLES
            };
            if prices.len() < floor_n {
                continue;
            }
            // No unstarred comparison group ⇒ nothing to prove or disprove ⇒
            // stays folded (the default), refs pool under the bare key.
            let base = self.base_value.get(bare).copied().unwrap_or(0.0);
            if base <= 0.0 {
                continue;
            }
            sort_asc(prices);
            let m = median(prices);
            if feature_is_significant(
                prices.len(),
                m,
                base,
                *ATTR_MIN_SHARE,
                *MIN_FEATURE_SAMPLES,
                *SIG_CHEAP_MIN_SAMPLES,
                *SIG_TWO_SIDED,
            ) {
                self.star_significant.insert(gk.clone());
            }
        }

        // Per-bk significance signature (insertion order of `significant`).
        for gk in &significant_ordered {
            let (bk, feat) = gk.split_once('|').unwrap();
            let e = self.sig_sig_by_bk.entry(bk.to_string()).or_default();
            e.push_str(feat);
            e.push(';');
        }

        // Pass 3: final-key pools, minor-adjusted.
        // `key_item` is the key→item-id lookup Pass 3b needs: `censored` carries no
        // attrs and no price, so the survival half of TTS can only ever be joined at
        // ITEM level, while the pools are keyed by final key. One entry per key
        // (~43k), not per ref, so it costs nothing next to the pools themselves.
        let mut key_item: FxHashMap<String, String> = FxHashMap::default();
        for &i in &fresh {
            // One walk for both, not two. See `key_and_minor`.
            let (fk, minor) = self.key_and_minor(&self.refs[i].attrs);
            let price = self.refs[i].price;
            let adj = (price - minor).max(price * 0.5); // == adjusted_price
            let (sold_at, seller) = (self.refs[i].sold_at, self.refs[i].seller.clone());
            if !self.censored_h_by_item.is_empty() && !key_item.contains_key(&fk) {
                key_item.insert(fk.clone(), self.refs[i].attrs.id.clone());
            }
            self.price_pool.entry(fk).or_default().push(PoolEntry {
                price: adj,
                sold_at,
                seller,
                tts_ms: self.refs[i].tts_ms,
            });
        }
        for pool in self.price_pool.values_mut() {
            // newest-first (stable, matching TS Array.sort stability)
            pool.sort_by(|x, y| y.sold_at.partial_cmp(&x.sold_at).unwrap());
        }

        // Pass 3b-pre: item-level sell-through, Kaplan-Meier.
        //
        // The events are EVERY tracked sale of the item, not just the fair-priced
        // ones: `censored` has no price column, so conditioning the events on price
        // while the censored half stays unconditioned would drop real events and
        // under-state sell-through. Speed (`fair_tts_h`) is price-conditioned;
        // completion (`sell_through`) is not. They answer different questions and
        // the gate wants both.
        let mut sell_through: FxHashMap<String, (Option<f64>, i64)> = FxHashMap::default();
        if !self.censored_h_by_item.is_empty() {
            let mut events_by_item: FxHashMap<&str, Vec<f64>> = FxHashMap::default();
            for r in &self.refs {
                if r.sold_at < cutoff {
                    continue;
                }
                if let Some(t) = r.tts_ms {
                    events_by_item
                        .entry(r.attrs.id.as_str())
                        .or_default()
                        .push(t / 3_600_000.0);
                }
            }
            let horizon = *TTS_SELL_HORIZON_H;
            let empty: Vec<f64> = Vec::new();
            for (item, cens) in self.censored_h_by_item.iter() {
                let ev = events_by_item.get(item.as_str()).unwrap_or(&empty);
                let st = crate::survival::sell_through(ev, cens, horizon);
                if let Some(v) = st {
                    self.sell_through_by_item.insert(item.clone(), v);
                }
                sell_through.insert(item.clone(), (st, cens.len() as i64));
            }
        }

        // Pass 3b (Phase 0, NON-GATING): observed fair-price TTS per key. Median
        // hours-to-sell among sales within ±15% of the key's median price, so
        // cheap flips (fast because underpriced, not because demand is real) are
        // excluded — the user's "flips excluded" instinct. Built into a local map
        // first to avoid borrowing self mutably while iterating price_pool.
        let mut fair_tts: FxHashMap<String, TtsInfo> = FxHashMap::default();
        for (fk, pool) in self.price_pool.iter() {
            let mut prices: Vec<f64> = pool.iter().map(|e| e.price).collect();
            if prices.is_empty() {
                continue;
            }
            sort_asc(&mut prices);
            let med = median(&prices);
            if med <= 0.0 {
                continue;
            }
            let (lo, hi) = (med * 0.85, med * 1.15);
            let mut fair: Vec<f64> = Vec::new();
            let mut n_all = 0i64;
            for e in pool {
                if let Some(t) = e.tts_ms {
                    n_all += 1;
                    if e.price >= lo && e.price <= hi {
                        fair.push(t);
                    }
                }
            }
            if fair.is_empty() {
                continue;
            }
            sort_asc(&mut fair);
            let (st, n_cens) = key_item
                .get(fk.as_str())
                .and_then(|item| sell_through.get(item))
                .copied()
                .unwrap_or((None, 0));
            fair_tts.insert(
                fk.clone(),
                TtsInfo {
                    fair_tts_h: median(&fair) / 3_600_000.0,
                    n_fair: fair.len() as i64,
                    n_all,
                    sell_through: st,
                    n_censored: n_cens,
                },
            );
        }
        self.fair_tts = fair_tts;
        self.key_item = key_item;

        // Clean-snipe lane.
        let bks: Vec<String> = self.base_value.keys().cloned().collect();
        for bk in bks {
            let pool = match self.price_pool.get(&bk) {
                Some(p) if p.len() >= *MIN_REFS => p,
                _ => continue,
            };
            let mut per_seller: FxHashMap<String, (f64, f64)> = FxHashMap::default();
            let mut anon = 0i64;
            for e in pool {
                let seller = if e.seller.is_empty() {
                    let s = format!("anon:{anon}");
                    anon += 1;
                    s
                } else {
                    e.seller.clone()
                };
                match per_seller.get(&seller) {
                    Some(cur) if e.price >= cur.0 => {}
                    _ => {
                        per_seller.insert(seller, (e.price, e.sold_at));
                    }
                }
            }
            let deduped: Vec<(f64, f64)> = per_seller.values().copied().collect();
            if deduped.len() < *MIN_REFS {
                continue;
            }
            let mut prices: Vec<f64> = deduped.iter().map(|e| e.0).collect();
            sort_asc(&mut prices);
            let (tmin, tmax) = min_max(&deduped.iter().map(|e| e.1).collect::<Vec<_>>());
            let span_days = ((tmax - tmin) / 86400.0).clamp(0.5, *REF_MAX_AGE_DAYS);
            self.clean_value.insert(
                bk,
                CleanVal {
                    median: median(&prices),
                    count: deduped.len() as i64,
                    volume_per_day: deduped.len() as f64 / span_days,
                },
            );
        }

        // Base-refs grouping for dominanceFloor (built eagerly; TS is lazy).
        let mut m: FxHashMap<String, Vec<usize>> = FxHashMap::default();
        for i in 0..self.refs.len() {
            if self.refs[i].sold_at < cutoff || self.refs[i].price <= 0.0 {
                continue;
            }
            let bk = base_key(&self.refs[i].attrs);
            m.entry(bk).or_default().push(i);
        }
        for arr in m.values_mut() {
            arr.sort_by(|&a, &b| {
                self.refs[b]
                    .sold_at
                    .partial_cmp(&self.refs[a].sold_at)
                    .unwrap()
            });
        }
        self.base_refs_by_key = m;
    }

    pub fn sig_signature(&self, bk: &str) -> String {
        self.sig_sig_by_bk.get(bk).cloned().unwrap_or_default()
    }

    pub fn high_for_base(&self, bk: &str) -> f64 {
        self.base_high.get(bk).copied().unwrap_or(0.0)
    }

    pub fn base_value_for(&self, bk: &str) -> f64 {
        self.base_value.get(bk).copied().unwrap_or(0.0)
    }

    pub fn sold_count_for_base(&self, bk: &str) -> i64 {
        self.base_sold_count.get(bk).copied().unwrap_or(0)
    }

    /// Sales per day for a BASE key, over the span its pool actually covers.
    ///
    /// `sold_count_for_base / REF_MAX_AGE_DAYS` would be wrong: `REF_CAP` caps
    /// the pool, so on a dense key 80 sales span ~15 hours, and dividing by 21
    /// days understates volume by ~30x. Same derivation as `clean_value`.
    pub fn base_volume_per_day(&self, bk: &str) -> f64 {
        let pool = match self.price_pool.get(bk) {
            Some(p) if !p.is_empty() => p,
            _ => return 0.0,
        };
        let times: Vec<f64> = pool.iter().map(|e| e.sold_at).collect();
        let (tmin, tmax) = min_max(&times);
        let span_days = ((tmax - tmin) / 86400.0).clamp(0.5, *REF_MAX_AGE_DAYS);
        pool.len() as f64 / span_days
    }

    pub fn clean_snipe(&self, a: &ItemAttributes) -> Option<SnipeStats> {
        if a.pet.is_some() || a.upgrade_level.is_some() || !a.variant.is_empty() {
            return None;
        }
        if !candidate_features(a).is_empty() {
            return None;
        }
        self.clean_value.get(&base_key(a)).map(|cv| SnipeStats {
            target: cv.median,
            samples: cv.count,
            volume_per_day: cv.volume_per_day,
        })
    }

    pub fn minor_feature_value(&self, a: &ItemAttributes) -> f64 {
        let bk = base_key(a);
        let base = self.base_value.get(&bk).copied().unwrap_or(0.0);
        let bar = (*FEATURE_MIN_VALUE).max(base * *ATTR_MIN_SHARE);
        let mut sum = 0.0;
        for f in candidate_features(a) {
            // ONE `feature_value` per feature. It used to be called twice here
            // (via `bazaar_significant`, then again for the sum) and my
            // `feature_forks_key` call added a third. That is not free on this
            // path: for an `ench:` token it `to_uppercase()`-allocates and walks
            // the bazaar map descending by level, and `minor_feature_value` runs
            // once per REFERENCE during the rebuild and again on the median lane
            // (`minor_credit`). An 8-enchant item was doing 24 of them.
            let Some(v) = self.feature_value(&f) else {
                continue; // not bazaar-priced; the learned pass owns it
            };
            if v >= bar {
                continue; // significant by material cost: priced BY the key
            }
            // A feature the sales promoted is also priced by the key, so
            // charging it here would subtract it from comparables that all have
            // it. Only reachable under `SIG_LEARN_OVER_BAZAAR`; the verdict is
            // already known to be Some(false), so pass it rather than recompute.
            if self.feature_forks_key_with(&f, &bk, Some(false)) {
                continue;
            }
            sum += v;
        }
        sum
    }

    pub fn adjusted_price(&self, price: f64, a: &ItemAttributes) -> f64 {
        (price - self.minor_feature_value(a)).max(price * 0.5)
    }

    fn quick_target(&self, key: &str) -> Option<f64> {
        if let Some(cached) = self.quick_target_memo.borrow().get(key).copied() {
            return cached;
        }
        let v = match self.price_pool.get(key) {
            Some(pool) if pool.len() >= 3 => {
                let mut prices: Vec<f64> = pool.iter().map(|e| e.price).collect();
                Some(median_of(&mut prices))
            }
            _ => None,
        };
        self.quick_target_memo
            .borrow_mut()
            .insert(key.to_string(), v);
        v
    }

    pub fn cheap_median(&self, key: &str) -> Option<f64> {
        self.quick_target(key)
    }

    pub fn thin_key_evidence(&self, key: &str) -> Option<ThinKeyEvidence> {
        let pool = self.price_pool.get(key)?;
        if pool.is_empty() {
            return None;
        }
        let take = if pool.len() > 12 { 12 } else { pool.len() };
        let mut window: Vec<f64> = pool.iter().take(take).map(|e| e.price).collect();
        Some(ThinKeyEvidence {
            median: median_of(&mut window),
            n: pool.len() as i64,
        })
    }

    pub fn dominance_floor(&self, a: &ItemAttributes) -> Option<DominanceStats> {
        let pool = self.base_refs_by_key.get(&base_key(a))?;
        if pool.len() < *MIN_REFS {
            return None;
        }
        let mut per_seller: FxHashMap<String, (f64, f64)> = FxHashMap::default();
        let mut seller_counts: FxHashMap<String, i64> = FxHashMap::default();
        let mut anon = 0i64;
        let mut dominated = 0i64;
        let limit = pool.len().min(600);
        for &ri in pool.iter().take(limit) {
            let r = &self.refs[ri];
            if !dominates_ref(a, &r.attrs) {
                continue;
            }
            dominated += 1;
            if !r.seller.is_empty() {
                *seller_counts.entry(r.seller.clone()).or_insert(0) += 1;
            }
            let sk = if r.seller.is_empty() {
                let s = format!("anon:{anon}");
                anon += 1;
                s
            } else {
                r.seller.clone()
            };
            match per_seller.get(&sk) {
                Some(cur) if r.price >= cur.0 => {}
                _ => {
                    per_seller.insert(sk, (r.price, r.sold_at));
                }
            }
        }
        let deduped: Vec<(f64, f64)> = per_seller.values().copied().collect();
        if deduped.len() < *MIN_REFS {
            return None;
        }
        let top = seller_counts.values().copied().max().unwrap_or(0).max(0);
        let top_share = if dominated > 0 {
            top as f64 / dominated as f64
        } else {
            0.0
        };
        let mut prices: Vec<f64> = deduped.iter().map(|e| e.0).collect();
        sort_asc(&mut prices);
        let (tmin, tmax) = min_max(&deduped.iter().map(|e| e.1).collect::<Vec<_>>());
        let span_days = ((tmax - tmin) / 86400.0).clamp(0.5, *REF_MAX_AGE_DAYS);
        Some(DominanceStats {
            target: median(&prices),
            samples: deduped.len() as i64,
            volume_per_day: deduped.len() as f64 / span_days,
            manipulated: top_share >= 0.33,
        })
    }

    pub fn base_trend_pct(&self, bk: &str) -> f64 {
        if let Some(cached) = self.trend_memo.borrow().get(bk).copied() {
            return cached;
        }
        let trend = self.compute_base_trend_pct(bk);
        self.trend_memo.borrow_mut().insert(bk.to_string(), trend);
        trend
    }

    fn compute_base_trend_pct(&self, bk: &str) -> f64 {
        let pool = match self.price_pool.get(bk) {
            Some(p) => p,
            None => return 0.0,
        };
        let lt = match self.quick_target(bk) {
            Some(v) if v != 0.0 => v,
            _ => return 0.0,
        };
        if pool.len() < *MIN_REFS {
            return 0.0;
        }
        let now_s = self.now_secs();
        let collect = |hours: f64| -> Vec<f64> {
            let cutoff = now_s - hours * 3600.0;
            let mut out = Vec::new();
            for e in pool {
                if e.sold_at < cutoff {
                    break;
                }
                out.push(e.price);
                if out.len() >= 200 {
                    break;
                }
            }
            out
        };
        // Same ladder as the short-term median window (see `widen_until`); a
        // hardcoded 24h here is what made this return "flat" on thin keys.
        //
        // `.max(24.0)`: unlike the short-term median, this window's 24h rung was
        // never gated behind the widen config — it always ran. Gating it would
        // NARROW the trend whenever widening is off, which is the opposite of the
        // fix and is what the modifier-model golden caught.
        let widen_h = SHORT_TERM_WIDEN_HOURS.max(24.0);
        let mut recent = widen_until(*SHORT_TERM_HOURS, widen_h, 5, collect);
        if recent.len() >= 5 {
            (median_of(&mut recent) - lt) / lt
        } else {
            0.0
        }
    }

    fn better_star_cap(&self, a: &ItemAttributes) -> Option<f64> {
        let ul = a.upgrade_level?;
        if a.pet.is_some() || a.id == "ENCHANTED_BOOK" {
            return None;
        }
        let mut cap: Option<f64> = None;
        let mut s = ul + 1.0;
        while s <= 10.0 {
            let mut m = a.clone();
            m.upgrade_level = Some(s);
            if let Some(t) = self.quick_target(&self.final_key(&m)) {
                if cap.is_none() || t < cap.unwrap() {
                    cap = Some(t);
                }
            }
            s += 1.0;
        }
        cap
    }

    pub fn zero_star_baseline(&self, a: &ItemAttributes) -> Option<f64> {
        a.upgrade_level?;
        if a.pet.is_some() || a.id == "ENCHANTED_BOOK" {
            return None;
        }
        let mut zero = a.clone();
        zero.upgrade_level = None;
        if let Some(t) = self.quick_target(&self.final_key(&zero)) {
            return Some(t);
        }
        if let Some(cv) = self.clean_value.get(&base_key(&zero)) {
            if cv.count > 0 {
                return Some(cv.median);
            }
        }
        let mut bare = zero.clone();
        bare.variant = String::new();
        if let Some(cv) = self.clean_value.get(&base_key(&bare)) {
            if cv.count > 0 {
                return Some(cv.median);
            }
        }
        None
    }

    pub fn feature_value(&self, feat: &str) -> Option<f64> {
        if let Some(bz) = self.bazaar.feature_bazaar_value(feat, self.now_ms) {
            return Some(bz);
        }
        if let Some(rest) = feat.strip_prefix("part:") {
            return self.base_value.get(rest).copied();
        }
        if let Some(rest) = feat.strip_prefix("pethled:") {
            return self.base_value.get(rest).copied();
        }
        None
    }

    fn bazaar_significant(&self, feat: &str, base: f64) -> Option<bool> {
        let v = self.feature_value(feat)?;
        Some(v >= (*FEATURE_MIN_VALUE).max(base * *ATTR_MIN_SHARE))
    }

    /// Does `feat` fork the key for this base key? The ONE place the rule lives.
    ///
    /// `sig_features` and `minor_feature_value` are two halves of one decision:
    /// a feature either forks the key or is subtracted off comparables as minor,
    /// never both. They used to spell the rule out separately, which is fine
    /// while it is a single expression and a latent double-count the moment it
    /// is not — as [`SIG_LEARN_OVER_BAZAAR`] makes it.
    fn feature_forks_key(&self, f: &str, bk: &str, base: f64) -> bool {
        self.feature_forks_key_with(f, bk, self.bazaar_significant(f, base))
    }

    /// [`PriceIndex::feature_forks_key`] with the bazaar verdict already in hand,
    /// for callers that had to compute it anyway. `feature_value` is far too dear
    /// on the rebuild path to look up twice for one decision.
    fn feature_forks_key_with(&self, f: &str, bk: &str, bazaar: Option<bool>) -> bool {
        if always_splits(f) {
            return true;
        }
        let learned = || self.significant.contains(&format!("{bk}|{f}"));
        match bazaar {
            // Bazaar-mapped. Historically its verdict was final, so a feature
            // whose market premium far exceeds its material cost could never
            // fork. As a FLOOR instead, the sales still get a say.
            //
            // `sig_learn_applies` is deliberately tested BEFORE `learned()`, so
            // the overwhelmingly common case (an enchant, which mode 1 never
            // promotes) short-circuits without building the `{bk}|{f}` string.
            Some(by_cost) => by_cost || (sig_learn_applies(f) && learned()),
            None => learned(),
        }
    }

    pub fn sig_features(&self, a: &ItemAttributes) -> Vec<String> {
        let bk = self.star_gated_bk(a, *STAR_SIG);
        let base = self.base_value.get(&bk).copied().unwrap_or(0.0);
        candidate_features(a)
            .into_iter()
            .filter(|f| self.feature_forks_key(f, &bk, base))
            .collect()
    }

    /// `base_key(a)` under the [`STAR_SIG`] gate: when the star level did not
    /// pass Pass 2b's premium bars, the emitted key is the bare one, so the
    /// pool bucketing (`key_and_minor`), the sig-feature context (this calls
    /// them mutually consistent), and every auction-time lookup (`final_key`)
    /// all agree on the family key. `star_sig = false` (flag off) or a pet /
    /// book / unstarred item returns the historical key verbatim.
    fn star_gated_bk(&self, a: &ItemAttributes, star_sig: bool) -> String {
        let bk = base_key(a);
        if !star_sig || a.pet.is_some() || a.id == "ENCHANTED_BOOK" {
            return bk;
        }
        let Some(l) = a.upgrade_level else { return bk };
        let Some(bare) = strip_star_suffix(&bk) else {
            return bk;
        };
        let gk = format!("{bare}|star:{}", jn(l));
        if self.star_significant.contains(&gk) {
            bk
        } else {
            bare
        }
    }

    /// Lowest median among the item's own features that the pool under-represents.
    ///
    /// `None` when the guard is off, nothing is known, or nothing binds. See
    /// [`POOL_REPR_GUARD`]. Only features that did NOT fork the key are consulted:
    /// a feature already in the key is shared by the whole pool by construction,
    /// so it cannot be the thing the pool disagrees about.
    ///
    /// Call this from the CALLER of `price_for_key`, never inside
    /// `compute_price_for` — that is memoised by key alone.
    pub fn feature_floor(&self, a: &ItemAttributes, target: f64) -> Option<f64> {
        if !*POOL_REPR_GUARD || target <= 0.0 {
            return None;
        }
        let bk = base_key(a);
        let in_key = self.sig_features(a);
        let medians: Vec<f64> = candidate_features(a)
            .into_iter()
            .filter(|f| !in_key.contains(f))
            .filter_map(|f| self.feature_median.get(&format!("{bk}|{f}")).copied())
            .collect();
        binding_feature_floor(&medians, target, *POOL_REPR_MIN_GAP)
    }

    /// `final_key` and `minor_feature_value` in ONE walk of the features.
    ///
    /// Pass 3 wants both for every reference and used to compute them
    /// separately, which meant two `candidate_features` walks, two `base_key`
    /// builds and — the expensive half — two `feature_value` lookups per
    /// feature, each of which `to_uppercase()`-allocates and walks the bazaar map
    /// descending by level for an `ench:` token. Over millions of refs that is
    /// the dominant cost of the rebuild.
    ///
    /// Returns exactly what the two functions return separately; the goldens pin
    /// both, so any divergence is a test failure rather than a silent drift.
    pub fn key_and_minor(&self, a: &ItemAttributes) -> (String, f64) {
        let bk = self.star_gated_bk(a, *STAR_SIG);
        let base = self.base_value.get(&bk).copied().unwrap_or(0.0);
        let bar = (*FEATURE_MIN_VALUE).max(base * *ATTR_MIN_SHARE);
        let mut sig: Vec<String> = Vec::new();
        let mut minor = 0.0;
        for f in candidate_features(a) {
            let v = self.feature_value(&f); // the one lookup
            let bazaar = v.map(|v| v >= bar);
            if self.feature_forks_key_with(&f, &bk, bazaar) {
                sig.push(f);
            } else if bazaar == Some(false) {
                // Priced off comparables rather than by the key, so it is
                // subtracted from them. `v` is Some here by construction.
                minor += v.unwrap_or(0.0);
            }
        }
        sig.sort();
        let key = if sig.is_empty() {
            bk
        } else {
            format!("{bk}#{}", sig.join("+"))
        };
        (key, minor)
    }

    pub fn final_key(&self, a: &ItemAttributes) -> String {
        self.final_key_with_star_sig(a, *STAR_SIG)
    }

    /// [`final_key`] with the star gate injected, so tests can pin both
    /// behaviour years on ONE built index without racing the process-global
    /// env `LazyLock`.
    ///
    /// Gate semantics: a star level forks the key only when Pass 2b measured a
    /// premium for `bare|star:N` at the Pass-2 bars. When it folds, the
    /// emitted key is the bare one (qty and `~variant` tails preserved), which
    /// is what lets every unstarred-flavoured consumer — the flip detect, the
    /// listing pricer, `better_star_cap`, `zero_star_baseline` — answer "what
    /// does this SELL as" off the aggregated family pool with no special case.
    /// When OFF (`star_sig = false`), or for pets/books, the key is
    /// byte-identical to the historical star-forking one.
    pub fn final_key_with_star_sig(&self, a: &ItemAttributes, star_sig: bool) -> String {
        let bk = self.star_gated_bk(a, star_sig);
        let base = self.base_value.get(&bk).copied().unwrap_or(0.0);
        let mut sig: Vec<String> = candidate_features(a)
            .into_iter()
            .filter(|f| self.feature_forks_key(f, &bk, base))
            .collect();
        sig.sort();
        if sig.is_empty() {
            bk
        } else {
            format!("{bk}#{}", sig.join("+"))
        }
    }

    pub fn price_for(&self, a: &ItemAttributes) -> Option<KeyStats> {
        self.price_for_key(a, &self.final_key(a))
    }

    /// Progressive key-coarsening for a fragmented exact key. See [`KEY_LADDER`].
    ///
    /// Drops `#`-features one at a time by ascending KNOWN value — a cosmetic
    /// rune or unread extra field goes before a material enchant — and returns
    /// the first pool that prices with ≥ `MIN_REFS` after its own per-seller
    /// dedupe. Dropping a feature can only UNDER-price a valuable variant: the
    /// coarser pool is the cheap majority, so the error direction is a refused
    /// flip rather than an overpay — the same asymmetry as `cheap_flip_rescue`,
    /// but priced off measured medians instead of the bare-base p95.
    ///
    /// Returns `(stats, depth)` for the first priced rung (`depth` ≥ 1 = how
    /// many features the priced key dropped from the exact key; the exact key
    /// itself is the caller's rung and is NOT retried here). Confidence is
    /// degraded by [`KEY_LADDER_CONF_STEP`] per dropped feature — the rung's
    /// own samples/volatility stay measured, only our distance from them is
    /// priced in. `None` when every rung down to the bare base is below
    /// `MIN_REFS` — that IS thin history, and the model lane is the next stop.
    ///
    /// ⛔ NOT a `MIN_REFS` cut: every rung is priced off its own ≥-`MIN_REFS`
    /// pool, never off 2 samples. Pets and skins carry no `#`-features and are
    /// untouched. Off unless [`KEY_LADDER`]; when off this is a dead `None`,
    /// byte-identical to the old behaviour.
    pub fn ladder_price(&self, a: &ItemAttributes, key: &str) -> Option<(KeyStats, usize)> {
        self.ladder_price_inner(a, key, *KEY_LADDER, *KEY_LADDER_CONF_STEP)
    }

    /// [`ladder_price`] with the flag and the degrade factor injected, so tests
    /// exercise every rung without racing on the process env (`LazyLock` reads
    /// once). `enabled = false`, and a key carrying neither `#`-features nor a
    /// star suffix, is a hard `None`. Public so the integration goldens can pin
    /// rung/depth/degrade behaviour.
    ///
    /// The drop-list spans TWO coarsening dimensions, both ordered by one
    /// ascending-known-value rule:
    ///
    /// - the `#`-features (cosmetic before material), and
    /// - the star suffix `*N`, e.g. `BOUQUET_OF_LIES*1` → `BOUQUET_OF_LIES`.
    ///
    /// Stars enter with sort key 0: their per-item coin value is exactly what
    /// we cannot measure here — their own pool is the fragmented one that sent
    /// us down the ladder. What the market DOES guarantee is that a starred
    /// unit weakly dominates the same roll unstarred (strictly better dungeon
    /// stats, positive upgrade cost), so paying the family price for it is not
    /// how you end up holding something worthless — the same one-sided error
    /// direction as everything else on the ladder. Being inserted before the
    /// features, the stable sort keeps stars ahead of the unknown-value ties.
    /// The star rung preserves qty (`X*1x3` → `Xx3`) and a `~variant` tail; it
    /// counts as one depth step for the confidence degrade, like any feature.
    /// Measured motivation 2026-08-15: Bouquet of Lies*1 priced off 13 samples
    /// with the last sale 98h old (`stale` guard, push blocked), while the
    /// unstarred pool traded that day at LBIN 17M with high volume — the item
    /// was liquid; only the STAR VARIANT was stale.
    pub fn ladder_price_inner(
        &self,
        a: &ItemAttributes,
        key: &str,
        enabled: bool,
        conf_step: f64,
    ) -> Option<(KeyStats, usize)> {
        if !enabled {
            return None;
        }
        let (base, rest) = match key.split_once('#') {
            Some((b, r)) => (b, r),
            None => (key, ""),
        };
        let starless = strip_star_suffix(base);
        // Original feature order, for candidate keys: pool keys are emitted by
        // an alphabetic `sig.sort()`, so a rung keeping ≥2 features must rebuild
        // the suffix in KEY order — the value order below only picks WHAT to
        // drop, never the candidate's spelling.
        let feats_orig: Vec<&str> = rest.split('+').filter(|f| !f.is_empty()).collect();
        let mut drops: Vec<Option<&str>> = Vec::new();
        if starless.is_some() {
            drops.push(None); // the star dimension
        }
        drops.extend(feats_orig.iter().copied().map(Some));
        if drops.is_empty() {
            return None;
        }
        drops.sort_by_key(|d| match d {
            None => 0i64,
            Some(f) => self
                .feature_value(f)
                .map(|v| (v * 100.0).clamp(i64::MIN as f64, i64::MAX as f64) as i64)
                .unwrap_or(0),
        });
        for depth in 1..=drops.len() {
            let stars_dropped = drops[..depth].iter().any(Option::is_none);
            let rung_base = if stars_dropped {
                starless.as_deref().unwrap_or(base)
            } else {
                base
            };
            let dropped: std::collections::HashSet<&str> =
                drops[..depth].iter().filter_map(|d| *d).collect();
            let kept: Vec<&str> = feats_orig
                .iter()
                .copied()
                .filter(|f| !dropped.contains(f))
                .collect();
            let candidate = if kept.is_empty() {
                rung_base.to_string()
            } else {
                format!("{rung_base}#{}", kept.join("+"))
            };
            if let Some(mut s) = self.price_for_key(a, &candidate) {
                s.confidence *= conf_step.powi(depth as i32);
                return Some((s, depth));
            }
        }
        None
    }

    /// `priceFor(a, precomputedKey)` — prices against an already-computed key (memoized).
    pub fn price_for_key(&self, a: &ItemAttributes, key: &str) -> Option<KeyStats> {
        if let Some(cached) = self.pf_memo.borrow().get(key).cloned() {
            return cached;
        }
        let computed = self.compute_price_for(a, key);
        self.pf_memo
            .borrow_mut()
            .insert(key.to_string(), computed.clone());
        computed
    }

    fn compute_price_for(&self, a: &ItemAttributes, key: &str) -> Option<KeyStats> {
        let raw = self.price_pool.get(key)?;
        if raw.len() < *MIN_REFS {
            return None;
        }
        let capped: &[PoolEntry] = if raw.len() > *REF_CAP {
            &raw[..*REF_CAP]
        } else {
            &raw[..]
        };
        // Keep only the CHEAPEST sale per seller. This used to build the map with
        // an OWNED String key per reference -- `e.seller.clone()` (a 32-char
        // uuid) or a `format!("anon:{n}")` -- i.e. up to REF_CAP=80 allocations
        // for every key priced. Borrowing from the pool entry costs nothing, and
        // a blank seller could never dedupe against anything (each got a unique
        // `anon:N`), so those simply bypass the map.
        let mut per_seller: FxHashMap<&str, (f64, f64)> = FxHashMap::default();
        let mut anon: Vec<(f64, f64)> = Vec::new();
        for e in capped {
            if e.seller.is_empty() {
                anon.push((e.price, e.sold_at));
                continue;
            }
            match per_seller.get(e.seller.as_str()) {
                Some(cur) if e.price >= cur.0 => {}
                _ => {
                    per_seller.insert(e.seller.as_str(), (e.price, e.sold_at));
                }
            }
        }
        // Order is irrelevant: every consumer below sorts (prices/recent) or
        // aggregates (len/min_max/stddev).
        let deduped: Vec<(f64, f64)> = per_seller.values().copied().chain(anon).collect();
        if deduped.len() < *MIN_REFS {
            return None;
        }

        // Same again: borrowed keys, no clone per reference.
        let mut seller_counts: FxHashMap<&str, i64> = FxHashMap::default();
        for e in capped {
            if !e.seller.is_empty() {
                *seller_counts.entry(e.seller.as_str()).or_insert(0) += 1;
            }
        }
        let top = seller_counts.values().copied().max().unwrap_or(0).max(0);
        let top_share = top as f64 / capped.len() as f64;
        let manipulated = top_share >= 0.33;

        let mut prices: Vec<f64> = deduped.iter().map(|e| e.0).collect();
        sort_asc(&mut prices);
        let long_term = median(&prices);

        let now_s = self.now_secs();
        let recent = recent_window(&deduped, now_s, *SHORT_TERM_HOURS, *SHORT_TERM_WIDEN_HOURS);
        // A key too thin to put 3 sales in even the widened window still has to
        // come from somewhere. Its BASE key is usually liquid — the per-variant
        // fork is what made it thin — so inherit that trend rather than quote a
        // long-term median the market has left behind. Negative only: this can
        // lower a target, never raise one.
        let thin_base_trend = if recent.len() < 3 && *THIN_KEY_BASE_TREND {
            let t = self.base_trend_pct(&base_key(a));
            if t < 0.0 && t.is_finite() {
                Some(t)
            } else {
                None
            }
        } else {
            None
        };
        let short_term = if recent.len() >= 3 {
            median(&recent)
        } else if let Some(t) = thin_base_trend {
            long_term * (1.0 + t)
        } else {
            long_term
        };
        let mut target = short_term.min(long_term);
        if let Some(sc) = self.better_star_cap(a) {
            if sc < target {
                target = sc;
            }
        }
        // Survivorship haircut: this median is a median over listings that SOLD.
        // On an item where most listings never sell it over-states the price at
        // which the thing actually clears. See [`LIQ_DISCOUNT`] for the
        // measurement. Strictly reduces the target, so the risk is a missed flip,
        // never an overpay.
        if *LIQ_DISCOUNT {
            target *= liquidity_factor(
                self.sell_through_for_item(&a.id),
                *LIQ_DISCOUNT_PIVOT,
                *LIQ_DISCOUNT_K,
                *LIQ_DISCOUNT_FLOOR,
            );
        }
        // The inherited trend must reach `trend_pct` too, or the `<= -0.15`
        // falling reject stays dead on exactly the keys it was written for.
        let trend_pct = if recent.len() >= 3 && long_term > 0.0 {
            (short_term - long_term) / long_term
        } else {
            thin_base_trend.unwrap_or(0.0)
        };
        let spread_pct = if long_term > 0.0 {
            (percentile(&prices, 0.75) - percentile(&prices, 0.25)) / long_term
        } else {
            1.0
        };
        let volatility = price_dispersion(&prices, long_term, *ROBUST_VOLATILITY);
        let (tmin, tmax) = min_max(&deduped.iter().map(|e| e.1).collect::<Vec<_>>());
        let span_days = ((tmax - tmin) / 86400.0).clamp(0.5, *REF_MAX_AGE_DAYS);
        let volume_per_day = deduped.len() as f64 / span_days;
        let last_sold_ago_h = ((now_s - tmax) / 3600.0).max(0.0);

        let ref_factor = (deduped.len() as f64 / 12.0).min(1.0);
        let stability = (1.0 - volatility).max(0.0);
        let volume_factor = (volume_per_day / 3.0).min(1.0);
        let mut confidence = 0.4 * ref_factor + 0.4 * stability + 0.2 * volume_factor;
        if manipulated {
            confidence *= 0.5;
        }

        Some(KeyStats {
            target,
            samples: deduped.len() as i64,
            volume_per_day,
            spread_pct,
            lowest_ref: prices[0],
            highest_ref: prices[prices.len() - 1],
            last_sold_ago_h,
            confidence,
            volatility,
            manipulated,
            trend_pct,
        })
    }
}

#[cfg(test)]
mod liquidity_discount_tests {
    use super::liquidity_factor;

    // Prod defaults.
    const PIVOT: f64 = 0.70;
    const K: f64 = 0.30;
    const FLOOR: f64 = 0.85;

    fn f(st: Option<f64>) -> f64 {
        liquidity_factor(st, PIVOT, K, FLOOR)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn unknown_sell_through_is_not_illiquid() {
        // Cold start has no censored history. Our buys on unknown-sell-through
        // items cleared at 80.1%, the overall average — nothing to correct.
        assert_eq!(f(None), 1.0);
        assert_eq!(f(Some(f64::NAN)), 1.0);
    }

    #[test]
    fn a_liquid_item_is_left_completely_alone() {
        // Measured estimate error is already 1.00 above the pivot, so any
        // discount up here is pure lost flips.
        assert_eq!(f(Some(0.70)), 1.0);
        assert_eq!(f(Some(0.85)), 1.0);
        assert_eq!(f(Some(1.0)), 1.0);
    }

    #[test]
    fn the_curve_matches_the_measured_bands() {
        // 2026-08-07, realised/estimate by sell-through band. We sit slightly
        // under each measured mean because the measurement can only see flips
        // that cleared.
        assert!(close(f(Some(0.50)), 0.94)); // measured mean 0.982
        assert!(close(f(Some(0.35)), 0.895)); // measured mean 0.974
        assert!(close(f(Some(0.25)), 0.865)); // measured mean 0.881
    }

    #[test]
    fn the_floor_binds_before_the_valuation_collapses() {
        // A 2%-sell-through item is marked down, not re-valued to nothing:
        // being wrong downward costs a flip, and we still have to be able to
        // price our own held stock.
        assert_eq!(f(Some(0.02)), FLOOR);
        assert_eq!(f(Some(0.0)), FLOOR);
        // Binds from ~0.20 down.
        assert!(close(f(Some(0.20)), FLOOR));
        assert!(f(Some(0.21)) > FLOOR);
    }

    #[test]
    fn it_can_only_ever_lower_a_target() {
        // The whole safety argument rests on this: a haircut costs flips, a
        // markup costs money. No parameterisation may produce a markup.
        for st in [0.0, 0.1, 0.3, 0.5, 0.7, 0.9, 1.0, 5.0] {
            for k in [0.0, 0.3, 2.0] {
                for pivot in [0.0, 0.7, 1.0] {
                    assert!(liquidity_factor(Some(st), pivot, k, FLOOR) <= 1.0);
                }
            }
        }
    }

    #[test]
    fn a_zero_k_is_an_exact_no_op() {
        // The documented rollback that does not need a rebuild.
        for st in [0.0, 0.25, 0.5, 0.9] {
            assert_eq!(liquidity_factor(Some(st), PIVOT, 0.0, FLOOR), 1.0);
        }
    }

    #[test]
    fn a_nonsense_floor_cannot_invert_the_discount() {
        // floor > 1 would otherwise clamp UPWARD into a markup.
        assert!(liquidity_factor(Some(0.1), PIVOT, K, 1.5) <= 1.0);
        assert!(liquidity_factor(Some(0.1), PIVOT, K, -3.0) >= 0.0);
    }
}

#[cfg(test)]
mod pool_representativeness_tests {
    use super::binding_feature_floor;

    const GAP: f64 = 0.15;

    #[test]
    fn the_midas_sharp_median_binds_against_the_gilded_target() {
        // resale ~350.1M from a pool 10:2 gilded; reforge:sharp's own median is
        // 100M. Binding it kills the 111.2M buy on margin.
        assert_eq!(binding_feature_floor(&[100e6], 350.1e6, GAP), Some(100e6));
    }

    #[test]
    fn a_feature_that_merely_disagrees_a_little_does_not_bind() {
        // 5% under target is inside the gap: pools are noisy, this must be inert.
        assert_eq!(binding_feature_floor(&[350e6 * 0.95], 350e6, GAP), None);
        // Exactly at the bar is not "materially under".
        assert_eq!(binding_feature_floor(&[350e6 * 0.85], 350e6, GAP), None);
    }

    #[test]
    fn it_never_raises_a_target() {
        // A feature worth MORE than the pool is not this guard's business.
        assert_eq!(binding_feature_floor(&[900e6], 350e6, GAP), None);
    }

    #[test]
    fn the_cheapest_binding_feature_wins() {
        // Several under-represented features: the cheapest is the real evidence.
        assert_eq!(
            binding_feature_floor(&[200e6, 90e6, 250e6], 350e6, GAP),
            Some(90e6)
        );
    }

    #[test]
    fn junk_medians_are_ignored() {
        assert_eq!(binding_feature_floor(&[0.0, -5.0], 350e6, GAP), None);
        assert_eq!(binding_feature_floor(&[100e6], 0.0, GAP), None);
        assert_eq!(binding_feature_floor(&[], 350e6, GAP), None);
    }
}

#[cfg(test)]
mod two_sided_significance_tests {
    use super::feature_is_significant;

    // STARRED_MIDAS_SWORD*5~bid=0 as it stood 2026-08-04: 10 gilded ~390M and
    // 2 sharp ~100M sharing one key, base (pool median) 385.5M.
    const BASE: f64 = 385.5e6;
    const GILDED: f64 = 390.0e6;
    const SHARP: f64 = 100.0e6;
    const SHARE: f64 = 0.15;
    const MIN_N: usize = 3;

    /// Default posture: two-sided off, no asymmetry.
    fn old(n: usize, m: f64) -> bool {
        feature_is_significant(n, m, BASE, SHARE, MIN_N, MIN_N, false)
    }
    /// Two-sided on with the cheap bar at 2, which is what prod would run.
    fn new(n: usize, m: f64) -> bool {
        feature_is_significant(n, m, BASE, SHARE, MIN_N, 2, true)
    }

    #[test]
    fn one_sided_lets_the_cheap_reforge_hide() {
        // The bug: -285.5M is 5x the 57.8M bar in magnitude and still refused.
        assert!(!old(2, SHARP));
        assert!(
            !old(9, SHARP),
            "even with plenty of samples, the sign refuses it"
        );
    }

    #[test]
    fn the_real_midas_forks_only_with_the_lower_cheap_bar() {
        // reforge:sharp had exactly n=2 against MIN_FEATURE_SAMPLES=3, so
        // two-sided ALONE is not enough — this is the case the user reported.
        assert!(!feature_is_significant(
            2, SHARP, BASE, SHARE, MIN_N, MIN_N, true
        ));
        assert!(new(2, SHARP));
    }

    #[test]
    fn the_majority_can_never_be_significant_against_itself() {
        // Gilded IS the base, so its delta is ~0 under either rule. This is why
        // forking the cheap side is the only move available.
        assert!(!old(10, GILDED));
        assert!(!new(10, GILDED));
    }

    #[test]
    fn still_ignores_a_feature_that_barely_moves_price() {
        // Must not fork on noise: 5% either way is under the 15% bar.
        assert!(!new(9, BASE * 0.95));
        assert!(!new(9, BASE * 1.05));
    }

    #[test]
    fn a_dearer_feature_keeps_the_STRICTER_bar() {
        // The lower bar is for the cheap direction ONLY. A dear feature with 2
        // samples must still be refused, or we would split pools we price off.
        assert!(!new(2, BASE * 2.0));
        assert!(new(3, BASE * 2.0));
    }

    #[test]
    fn an_expensive_feature_is_unaffected_by_the_change() {
        // HEARTFIRE_DAGGER coldfusion 507M against a 45M base: significant under
        // both rules, so enabling this must not disturb it.
        for two in [false, true] {
            assert!(feature_is_significant(
                205, 507e6, 45e6, SHARE, MIN_N, 2, two
            ));
        }
    }

    #[test]
    fn a_zero_base_is_never_significant() {
        assert!(!feature_is_significant(
            9, 100e6, 0.0, SHARE, MIN_N, 2, true
        ));
    }
}

#[cfg(test)]
mod enrich_pool_tests {
    use super::pooled_extra_inner;

    /// The five types seen on HEGEMONY_ARTIFACT in the 7 days to 2026-08-04.
    const TYPES: [&str; 5] = [
        "enrich_critical_chance",
        "enrich_magic_find",
        "enrich_sea_creature_chance",
        "enrich_critical_damage",
        "enrich_walk_speed",
    ];

    #[test]
    fn on_every_enrichment_type_maps_to_the_same_token() {
        let mapped: Vec<&str> = TYPES.iter().map(|t| pooled_extra_inner(t, true)).collect();
        assert!(
            mapped.windows(2).all(|w| w[0] == w[1]),
            "enrichment types must not split the key: {mapped:?}"
        );
    }

    #[test]
    fn off_by_default_every_type_keeps_its_own_key() {
        let mapped: Vec<&str> = TYPES.iter().map(|t| pooled_extra_inner(t, false)).collect();
        assert_eq!(mapped.as_slice(), TYPES.as_slice());
    }

    #[test]
    fn non_enrichment_extras_are_never_pooled() {
        // The prefix test must not catch unrelated extras. `x:hpb`, `x:fuming`
        // and friends carry real, DIFFERENT value and map to bazaar items.
        // `enriched_soul` is the trap: it starts with "enrich" but not "enrich_".
        for n in [
            "hpb",
            "fuming",
            "art_of_war",
            "shiny",
            "ethermerge",
            "enriched_soul",
        ] {
            assert_eq!(
                pooled_extra_inner(n, true),
                n,
                "{n} must keep its own identity"
            );
        }
    }
}

#[cfg(test)]
mod recent_window_tests {
    use super::{recent_window, sort_asc, widen_until};

    const NOW: f64 = 1_785_664_000.0;
    const H: f64 = 3600.0;

    /// The real HEGEMONY_ARTIFACT recomb+enriched pool as it stood on 07-31,
    /// when we bought two at 519-530M. Prices in millions of coins.
    fn hegemony_0731() -> Vec<(f64, f64)> {
        // (price, sold_at) — newest first, hours before NOW.
        let raw = [
            (532.0, 26.0),
            (519.0, 29.0),
            (519.7, 30.0),
            (560.0, 38.0),
            (580.0, 42.0),
            (650.0, 67.0),
            (670.0, 79.0),
            (677.0, 88.0),
            (680.0, 95.0),
            (699.0, 120.0),
            (690.0, 128.0),
            (700.0, 131.0),
        ];
        raw.iter().map(|(p, h)| (p * 1e6, NOW - h * H)).collect()
    }

    #[test]
    fn widening_off_reproduces_the_fixed_window() {
        let pool = hegemony_0731();
        // 12h holds nothing: the key sells ~4/day, which is the whole problem.
        assert!(recent_window(&pool, NOW, 12.0, 0.0).is_empty());
        assert!(recent_window(&pool, NOW, 12.0, 12.0).is_empty());
        // A widen target at or under the base window must not widen.
        assert!(recent_window(&pool, NOW, 24.0, 24.0).is_empty());
    }

    #[test]
    fn widening_finds_the_falling_price_the_fixed_window_misses() {
        let pool = hegemony_0731();
        let r = recent_window(&pool, NOW, 12.0, 48.0);
        // 24h still holds nothing, so it lands on 48h: 5 sales, median 532M.
        assert_eq!(r.len(), 5);
        let med = r[r.len() / 2];
        assert!((med - 532e6).abs() < 1.0, "median was {med}");
        // That is what arms both mechanisms. long_term over the full 21d pool is
        // ~675M, so target = min(560, 675) = 560M and trend = -17%, past the
        // -15% falling reject. Against the 530M we actually paid, the flip dies
        // on margin instead of promising a 25% discount.
        let long_term = 675e6;
        assert!(med < long_term);
        assert!((med - long_term) / long_term <= -0.15);
    }

    #[test]
    fn stops_widening_as_soon_as_it_has_three() {
        // Dense key: 12h already holds 3, so the ladder must not run at all and
        // the sample must stay the tight one.
        let pool: Vec<(f64, f64)> = vec![
            (100e6, NOW - 1.0 * H),
            (101e6, NOW - 2.0 * H),
            (102e6, NOW - 3.0 * H),
            (900e6, NOW - 30.0 * H),
        ];
        let r = recent_window(&pool, NOW, 12.0, 48.0);
        assert_eq!(r.len(), 3);
        assert!(
            !r.contains(&900e6),
            "widened past a window that already had 3"
        );
    }

    /// RELIC_OF_COINS as it traded on 2026-08-03: ~1.8 sales/day, real prices.
    /// 12h holds 2, 24h holds 3, 48h holds 5 — which is exactly the gap that let
    /// the trend read flat while the item was down 22%.
    fn relic_of_coins() -> Vec<(f64, f64)> {
        let raw = [
            (358.9, 2.0),
            (340.0, 6.0),
            (356.9, 20.0),
            (350.0, 30.0),
            (333.0, 40.0),
        ];
        raw.iter().map(|(p, h)| (p * 1e6, NOW - h * H)).collect()
    }

    fn trend_window(pool: &[(f64, f64)], base_h: f64, widen_h: f64) -> Vec<f64> {
        widen_until(base_h, widen_h, 5, |hours| {
            let mut v: Vec<f64> = pool
                .iter()
                .filter(|e| e.1 >= NOW - hours * 3600.0)
                .map(|e| e.0)
                .collect();
            sort_asc(&mut v);
            v
        })
    }

    #[test]
    fn the_trend_ladder_stopping_at_24h_reports_flat_on_a_falling_item() {
        // The live bug: `compute_base_trend_pct` hardcoded 12h→24h. Both rungs
        // come up short of 5, so it returned 0.0 and `estimate_for` applied
        // neither its refusal nor its discount.
        assert_eq!(trend_window(&relic_of_coins(), 12.0, 24.0).len(), 3);
    }

    #[test]
    fn widening_the_trend_to_48h_trips_the_model_refusal() {
        let r = trend_window(&relic_of_coins(), 12.0, 48.0);
        assert_eq!(r.len(), 5);
        let med = r[r.len() / 2];
        assert!((med - 350e6).abs() < 1.0, "median was {med}");
        // 21d long-term median was 414.5M. -15.6% clears `trend <= -0.15`, so the
        // model refuses the key outright rather than quoting 409.5M.
        let long_term = 414.5e6;
        assert!((med - long_term) / long_term <= -0.15);
    }

    #[test]
    fn the_two_windows_share_a_ladder_but_not_a_threshold() {
        // Same pool, same rungs: the min-3 window is satisfied at 24h and stops,
        // the min-5 window keeps going to 48h. Sharing `widen_until` must not
        // collapse the two sample requirements into one.
        let pool = relic_of_coins();
        assert_eq!(recent_window(&pool, NOW, 12.0, 48.0).len(), 3);
        assert_eq!(trend_window(&pool, 12.0, 48.0).len(), 5);
    }

    #[test]
    fn a_key_too_thin_even_to_widen_stays_empty() {
        // Under 3 sales in 48h: `recent` stays short, the caller falls back to
        // long_term exactly as before. Widening must not invent a sample.
        let pool: Vec<(f64, f64)> = vec![(100e6, NOW - 40.0 * H), (101e6, NOW - 44.0 * H)];
        assert_eq!(recent_window(&pool, NOW, 12.0, 48.0).len(), 2);
    }

    #[test]
    fn result_is_sorted_at_every_rung() {
        let pool: Vec<(f64, f64)> = vec![
            (300e6, NOW - 20.0 * H),
            (100e6, NOW - 25.0 * H),
            (200e6, NOW - 30.0 * H),
        ];
        let r = recent_window(&pool, NOW, 12.0, 48.0);
        assert_eq!(r, vec![100e6, 200e6, 300e6]);
    }
}

#[cfg(test)]
mod price_dispersion_tests {
    use super::price_dispersion;
    use crate::math::median;

    /// The real LOUDMOUTH_BASS pool from prod on 2026-08-02: 34 ordinary sales
    /// clustered around 1.0-1.7M, plus the two coin-transfer sales at 61.4M and
    /// 65.0M that were sitting in the 80-reference window at the time.
    fn bass_pool(contaminated: bool) -> Vec<f64> {
        // Shaped like the real pool: tightly clustered (stddev 0.15M on a 1.36M
        // median), not uniformly spread across the range.
        let mut p: Vec<f64> = Vec::new();
        p.extend((0..24).map(|i| 1_250_000.0 + (i as f64) * 8_000.0));
        p.extend((0..5).map(|i| 1_020_000.0 + (i as f64) * 40_000.0));
        p.extend((0..5).map(|i| 1_500_000.0 + (i as f64) * 40_000.0));
        if contaminated {
            p.push(61_400_000.0);
            p.push(65_000_000.0);
        }
        p.sort_by(|a, b| a.partial_cmp(b).unwrap());
        p
    }

    /// The confidence formula from `compute_price_for`, so the test asserts on
    /// the number that actually gates the flip rather than on volatility alone.
    fn confidence(prices: &[f64], robust: bool) -> f64 {
        let med = median(prices);
        let vol = price_dispersion(prices, med, robust);
        let ref_factor = (prices.len() as f64 / 12.0).min(1.0);
        let stability = (1.0 - vol).max(0.0);
        0.4 * ref_factor + 0.4 * stability + 0.2 * 1.0
    }

    #[test]
    fn two_transfer_sales_destroy_the_stddev_estimate() {
        let dirty = bass_pool(true);
        let med = median(&dirty);
        // stddev is pinned at the clamp by 2 sales out of 36...
        assert_eq!(price_dispersion(&dirty, med, false), 1.0);
        // ...which caps confidence at 0.6, under the 0.85 floor. Wholesale reject.
        assert!(confidence(&dirty, false) < 0.85);
    }

    #[test]
    fn the_iqr_ignores_them_and_the_key_prices_again() {
        let dirty = bass_pool(true);
        let clean = bass_pool(false);
        // THE property: contamination barely moves the robust estimate, where it
        // moved the stddev one from 0.11 to its 1.0 clamp.
        let d = price_dispersion(&dirty, median(&dirty), true);
        let c = price_dispersion(&clean, median(&clean), true);
        assert!((d - c).abs() < 0.02, "robust estimate moved {c} -> {d}");
        assert!(
            confidence(&dirty, true) >= 0.85,
            "still rejected: {}",
            confidence(&dirty, true)
        );
    }

    #[test]
    fn robust_and_stddev_agree_on_a_clean_pool() {
        // The fix must not flatter an ordinary key, only stop one sale from
        // destroying it. On uncontaminated prices the two estimates are close.
        let clean = bass_pool(false);
        let med = median(&clean);
        let a = price_dispersion(&clean, med, false);
        let b = price_dispersion(&clean, med, true);
        assert!((a - b).abs() < 0.05, "stddev {a} vs iqr {b}");
    }

    #[test]
    fn a_genuinely_bimodal_pool_still_reports_high_volatility() {
        // THE case the IQR must not paper over: two real variants sharing a key,
        // each a large share of the pool. The median IS unreliable there and the
        // key SHOULD be distrusted. This is why IQR was chosen over MAD.
        let mut p: Vec<f64> = Vec::new();
        p.extend((0..20).map(|i| 1_000_000.0 + i as f64 * 10_000.0));
        p.extend((0..20).map(|i| 8_000_000.0 + i as f64 * 10_000.0));
        p.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med = median(&p);
        assert!(
            price_dispersion(&p, med, true) > 0.5,
            "bimodal pool was flattered"
        );
    }

    #[test]
    fn degenerate_inputs_stay_maximally_distrusted() {
        assert_eq!(price_dispersion(&[], 1.0, true), 1.0);
        assert_eq!(price_dispersion(&[1.0, 2.0], 0.0, true), 1.0);
        assert_eq!(price_dispersion(&[1.0, 2.0], -5.0, false), 1.0);
    }
}
