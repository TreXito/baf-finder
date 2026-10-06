//! Port of `baf-flip-finder/src/modifierModel.ts` — additive per-item modifier
//! value model. Pinned by `goldens/modifierModel/estimates.json`.
//! All internals are medians/sums/lookups (order-independent), so plain HashMaps
//! are used; behavior is fixed by the golden regardless.

use crate::config::{MIN_REFS, REF_MAX_AGE_DAYS};
use crate::math::{median, min_max, stddev};
use crate::nbt::ItemAttributes;
use crate::price_index::{base_key, PriceIndex, Reference};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

const PRIOR_STRENGTH: i64 = 10;
const MIN_COMBO_SAMPLES: i64 = 3;
const SANITY_CEILING_MULT: f64 = 1.1;

/// Buy price (coins) at or above which the model path reports the ask-band
/// volume instead of the whole base item's volume. Unset or 0 keeps the old
/// behaviour byte-for-byte, which is what the goldens pin; set it to 1 to apply
/// the band to every model-priced flip.
///
/// These are read locally rather than through `config.rs` because they have no
/// TS counterpart, so they need none of that module's `Number()` coercion
/// quirks.
static BAND_MIN_BUY: LazyLock<f64> = LazyLock::new(|| env_num("MODEL_BAND_VOLUME_MIN_BUY", 0.0));

/// Fraction of the model target we assume the bot actually lists at. The
/// finder's own `scale_price` tiers run 0.90-0.97.
static BAND_ASK_MULT: LazyLock<f64> = LazyLock::new(|| env_num("MODEL_BAND_ASK_MULT", 0.95));

/// Build a base model from a reference COMBO when the item has fewer than
/// MIN_REFS genuinely bare sales. 0 or unset keeps the old behaviour exactly,
/// which is what `goldens/modifierModel/estimates.json` pins.
///
/// `rebuild` takes its baseline from `combos.get("")`, the sales whose
/// significant-feature set is empty, and skips the base key outright when there
/// are fewer than MIN_REFS of them. Premium gear is never sold bare, so it never
/// gets a model at all: measured over 7 days of prod references (1.24M sales),
/// 4926 of 8514 base keys are skipped for this reason, and `estimate_for`
/// returns None for 4926/4926 of them. That is 7.0% of sales but **14.9% of coin
/// volume**, and it is the whole of the `notpriceable` reject (1081 of 1732
/// HIGHMISS lines on prod, every one `basis=high`, i.e. `high_for_base` had data
/// and the model still refused). `HYPERION*5` alone: 420 sales, **0** bare,
/// 439B of volume in the window.
///
/// With this on, such a key instead anchors on its cheapest well-sampled combo
/// and prices everything else as a delta from there. Every branch is biased to
/// UNDERestimate, because overestimating buys junk that strands forever
/// (`inventory_pricing.rs` clamps the list price to paid*1.05) while
/// underestimating only skips a flip, which is what happens today anyway:
/// the anchor is the cheapest qualifying combo rather than the modal one,
/// unknown feature values contribute 0 rather than a guess, and an item MISSING
/// one of the anchor's features is refused outright unless that feature can be
/// valued and subtracted.
/// Anchor strategy, so the choice is measurable rather than assumed:
///   0 = off, the original bare-sales-only baseline.
///   1 = CHEAPEST qualifying combo, refuse an item missing an unvaluable anchor
///       feature. Biased hardest toward underestimating.
///   2 = MOST-SAMPLED qualifying combo, same strict refusal. A deeper anchor is
///       a better-measured median but sits higher, so deltas can run negative.
///   3 = cheapest anchor, but an unvaluable missing feature contributes 0
///       instead of refusing. Highest coverage, and the only mode that can
///       overestimate, so judge it on overshoot rather than coverage.
static MODEL_REF_COMBO: LazyLock<f64> = LazyLock::new(|| env_num("MODEL_REF_COMBO", 0.0));

fn env_num(name: &str, def: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(def)
}

#[derive(Debug, Clone)]
struct FeatVal {
    value: f64,
    count: i64,
}

#[derive(Debug, Clone)]
struct ComboCorr {
    value: f64,
    #[allow(dead_code)]
    count: i64,
}

#[derive(Debug, Clone)]
struct BaseModel {
    baseline: f64,
    baseline_count: i64,
    /// Significant features already priced into `baseline`, sorted. Empty for a
    /// baseline built the original way, from genuinely bare sales, which makes
    /// every `baseline_feats.is_empty()` branch below the exact old behaviour.
    /// Non-empty only under [`MODEL_REF_COMBO`], where the anchor is a combo.
    baseline_feats: Vec<String>,
    feature_value: HashMap<String, FeatVal>,
    combo_correction: HashMap<String, ComboCorr>,
    volume_per_day: f64,
    volatility: f64,
    /// Every base-key sale price in the model window, ascending. Same population
    /// (so the same count) as the timestamps behind `volume_per_day`, which is
    /// what makes `band_volume_per_day(_, 0.0) == volume_per_day` exact.
    sale_prices: Vec<f64>,
    /// The divisor behind `volume_per_day`, kept so the band rate is expressed
    /// on the same per-day scale.
    span_days: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelEstimate {
    pub target: f64,
    pub samples: i64,
    pub volume_per_day: f64,
    pub confidence: f64,
    pub coverage: f64,
    pub volatility: f64,
}

pub struct ModifierModel {
    models: HashMap<String, BaseModel>,
}

fn sort_asc(xs: &mut [f64]) {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
}

struct ComboStat {
    feats: Vec<String>,
    key: String,
    n: usize,
    median: f64,
}

impl ModifierModel {
    pub fn model_count(&self) -> usize {
        self.models.len()
    }

    pub fn rebuild(refs: &[Reference], index: &PriceIndex, now_ms: i64) -> ModifierModel {
        let cutoff = now_ms as f64 / 1000.0 - *REF_MAX_AGE_DAYS * 86400.0;
        // bk -> comboKey -> seller -> (price, soldAt) cheapest per seller.
        type ByItem = HashMap<String, HashMap<String, HashMap<String, (f64, f64)>>>;
        let mut by_item: ByItem = HashMap::new();
        let mut anon = 0i64;
        for r in refs {
            if r.sold_at < cutoff || r.price <= 0.0 {
                continue;
            }
            let bk = base_key(&r.attrs);
            let mut sig = index.sig_features(&r.attrs);
            sig.sort();
            let combo_key = sig.join("+");
            let sellers = by_item.entry(bk).or_default().entry(combo_key).or_default();
            let seller = if r.seller.is_empty() {
                let s = format!("anon:{anon}");
                anon += 1;
                s
            } else {
                r.seller.clone()
            };
            match sellers.get(&seller) {
                Some(p) if r.price >= p.0 => {}
                _ => {
                    sellers.insert(seller, (r.price, r.sold_at));
                }
            }
        }

        let mut models = HashMap::new();
        for (bk, combos) in &by_item {
            let mut bare_prices: Vec<f64> = combos
                .get("")
                .map(|m| m.values().map(|s| s.0).collect())
                .unwrap_or_default();
            sort_asc(&mut bare_prices);
            // Old behaviour when the flag is off: no bare sales, no model. Kept
            // as an early skip so the disabled path does no extra work either.
            if bare_prices.len() < *MIN_REFS && *MODEL_REF_COMBO == 0.0 {
                continue;
            }

            let combo_stats: Vec<ComboStat> = combos
                .iter()
                .map(|(key, sales)| {
                    let mut prices: Vec<f64> = sales.values().map(|s| s.0).collect();
                    sort_asc(&mut prices);
                    ComboStat {
                        feats: if key.is_empty() {
                            Vec::new()
                        } else {
                            key.split('+').map(String::from).collect()
                        },
                        key: key.clone(),
                        n: prices.len(),
                        median: median(&prices),
                    }
                })
                .collect();

            // Modifier values from combo pairs differing by exactly one modifier.
            let mut feature_value: HashMap<String, FeatVal> = HashMap::new();
            let mut all_feats: HashSet<String> = HashSet::new();
            for c in &combo_stats {
                for f in &c.feats {
                    all_feats.insert(f.clone());
                }
            }
            for f in &all_feats {
                let mut diffs: Vec<f64> = Vec::new();
                let mut count: i64 = 0;
                for a in &combo_stats {
                    if a.feats.iter().any(|x| x == f) {
                        continue;
                    }
                    let mut wf = a.feats.clone();
                    wf.push(f.clone());
                    wf.sort();
                    let with_f = wf.join("+");
                    if let Some(b) = combo_stats.iter().find(|c| c.key == with_f) {
                        diffs.push(b.median - a.median);
                        count += a.n.min(b.n) as i64;
                    }
                }
                if diffs.is_empty() {
                    continue;
                }
                sort_asc(&mut diffs);
                let med = median(&diffs);
                feature_value.insert(
                    f.clone(),
                    FeatVal {
                        value: (count as f64 * med) / (count as f64 + PRIOR_STRENGTH as f64),
                        count,
                    },
                );
            }

            // Anchor the model. Genuinely bare sales are always preferred; a
            // reference combo is only ever consulted when there are too few.
            let (baseline, baseline_count, baseline_feats, base_prices) = if bare_prices.len()
                >= *MIN_REFS
            {
                let n = bare_prices.len() as i64;
                (median(&bare_prices), n, Vec::new(), bare_prices)
            } else {
                let eligible = combo_stats
                    .iter()
                    .filter(|c| !c.feats.is_empty() && c.n >= *MIN_REFS && c.median > 0.0);
                let anchor = if *MODEL_REF_COMBO == 2.0 {
                    // Deepest combo: the best-measured median, but it sits
                    // higher so more items need a negative delta.
                    eligible
                        .max_by(|a, b| a.n.cmp(&b.n).then(b.median.partial_cmp(&a.median).unwrap()))
                } else {
                    // Cheapest well-sampled combo, so the anchor sits as close
                    // to bare as the data allows and the deltas run upward.
                    eligible
                        .min_by(|a, b| a.median.partial_cmp(&b.median).unwrap().then(b.n.cmp(&a.n)))
                };
                let Some(a) = anchor else { continue };
                let mut prices: Vec<f64> = combos
                    .get(&a.key)
                    .map(|m| m.values().map(|s| s.0).collect())
                    .unwrap_or_default();
                sort_asc(&mut prices);
                let mut feats = a.feats.clone();
                feats.sort();
                (a.median, a.n as i64, feats, prices)
            };

            // Synergy corrections.
            let mut combo_correction: HashMap<String, ComboCorr> = HashMap::new();
            for c in &combo_stats {
                if c.feats.len() < 2 || (c.n as i64) < MIN_COMBO_SAMPLES {
                    continue;
                }
                // Deltas are measured from the anchor, so features already priced
                // into `baseline` must not be added again, and ones the anchor
                // has but this combo lacks come back off. Both loops are no-ops
                // when `baseline_feats` is empty, i.e. on the original path.
                let mut predicted = baseline;
                for f in &c.feats {
                    if baseline_feats.contains(f) {
                        continue;
                    }
                    predicted += feature_value
                        .get(f)
                        .map(|fv| fv.value.max(0.0))
                        .unwrap_or(0.0);
                }
                for f in &baseline_feats {
                    if c.feats.contains(f) {
                        continue;
                    }
                    predicted -= feature_value
                        .get(f)
                        .map(|fv| fv.value.max(0.0))
                        .unwrap_or(0.0);
                }
                let mut corr =
                    ((c.median - predicted) * c.n as f64) / (c.n as f64 + PRIOR_STRENGTH as f64);
                let cap = c.median * 0.5;
                corr = corr.clamp(-cap, cap);
                combo_correction.insert(
                    c.key.clone(),
                    ComboCorr {
                        value: corr,
                        count: c.n as i64,
                    },
                );
            }

            let mut times: Vec<f64> = Vec::new();
            let mut sale_prices: Vec<f64> = Vec::new();
            for sales in combos.values() {
                for s in sales.values() {
                    times.push(s.1);
                    sale_prices.push(s.0);
                }
            }
            sort_asc(&mut sale_prices);
            let (tmin, tmax) = min_max(&times);
            let span_days = ((tmax - tmin) / 86400.0).clamp(0.5, *REF_MAX_AGE_DAYS);
            let volatility = if baseline > 0.0 {
                (stddev(&base_prices) / baseline).min(1.0)
            } else {
                1.0
            };
            models.insert(
                bk.clone(),
                BaseModel {
                    baseline,
                    baseline_count,
                    baseline_feats,
                    feature_value,
                    combo_correction,
                    volume_per_day: times.len() as f64 / span_days,
                    volatility,
                    sale_prices,
                    span_days,
                },
            );
        }
        ModifierModel { models }
    }

    pub fn estimate_for(
        &self,
        attrs: &ItemAttributes,
        index: &PriceIndex,
    ) -> Option<ModelEstimate> {
        let bk = base_key(attrs);
        let m = self.models.get(&bk)?;
        let trend = index.base_trend_pct(&bk);
        if trend <= -0.15 {
            return None;
        }
        let feats = index.sig_features(attrs);

        let value_of = |f: &String| -> Option<f64> {
            let learned = m.feature_value.get(f);
            let det = if f.starts_with("ench:") {
                None
            } else {
                index.feature_value(f)
            };
            if learned.map(|l| l.count >= PRIOR_STRENGTH).unwrap_or(false) {
                let l = learned.unwrap();
                Some(0.0_f64.max(l.value).max(det.unwrap_or(0.0)))
            } else if det.is_some() {
                det
            } else if learned.map(|l| l.count > 0).unwrap_or(false) {
                Some(0.0_f64.max(learned.unwrap().value))
            } else {
                None
            }
        };

        let mut est = m.baseline;
        let mut covered = 0i64;
        for f in &feats {
            // Already inside the anchor's price, so it is covered for free and
            // must not be added twice. Never true on the original path.
            if m.baseline_feats.contains(f) {
                covered += 1;
                continue;
            }
            if let Some(c) = value_of(f) {
                est += c;
                covered += 1;
            }
        }
        // This item is missing something the anchor has. Refuse rather than guess
        // when that feature cannot be valued: pricing a bare Hyperion off a
        // gemmed one is exactly the overestimate that strands inventory.
        for f in &m.baseline_feats {
            if feats.contains(f) {
                continue;
            }
            match value_of(f) {
                Some(c) => est -= c,
                // Mode 3 keeps the estimate instead of refusing, which is the
                // only way this path can OVERestimate. Measured, not assumed.
                None if *MODEL_REF_COMBO == 3.0 => {}
                None => return None,
            }
        }
        let mut sig_sorted = feats.clone();
        sig_sorted.sort();
        if let Some(corr) = m.combo_correction.get(&sig_sorted.join("+")) {
            est += corr.value;
        }
        // A bare-sale baseline is a genuine floor: nothing is worth less than the
        // item with no modifiers. An anchor combo is NOT, since subtracting its
        // features is the whole point, so there the floor is only non-negative.
        est = est.max(if m.baseline_feats.is_empty() {
            m.baseline
        } else {
            0.0
        });
        if m.baseline > 0.0 && est > m.baseline * 3.0 {
            est = m.baseline * 3.0;
        }
        let high = index.high_for_base(&bk);
        if high > 0.0 {
            est = est.min(high * SANITY_CEILING_MULT);
        }
        if trend < 0.0 {
            est *= 1.0 + trend;
        }

        let coverage = if !feats.is_empty() {
            covered as f64 / feats.len() as f64
        } else {
            1.0
        };
        let base_factor = (m.baseline_count as f64 / 12.0).min(1.0);
        let stability = (1.0 - m.volatility).max(0.0);
        let confidence = (0.45 * base_factor + 0.25 * coverage + 0.3 * stability).min(0.88);

        Some(ModelEstimate {
            target: est,
            samples: m.baseline_count,
            volume_per_day: m.volume_per_day,
            confidence,
            coverage,
            volatility: m.volatility,
        })
    }

    /// Daily rate of base-key sales that cleared at or above `ask`.
    ///
    /// `volume_per_day` counts every sale of the base item, so for a variant
    /// priced off modifier deltas it answers "how often does ANY Fermento
    /// Chestplate sell" (57/day) rather than "how often does one sell at what I
    /// am asking" (2.5/day). Measured over 5010 of our own round-trips the two
    /// differ by 2.5x at the median and 12x at the 10th percentile, which is how
    /// a spec that trades once a fortnight cleared a gate that believed it
    /// turned over every 25 minutes. Same population and same span as
    /// `volume_per_day`, so the result is always <= it.
    pub fn band_volume_per_day(&self, attrs: &ItemAttributes, ask: f64) -> Option<f64> {
        let m = self.models.get(&base_key(attrs))?;
        let below = m.sale_prices.partition_point(|p| *p < ask);
        Some((m.sale_prices.len() - below) as f64 / m.span_days)
    }

    /// The volume a model-priced flip should hand to the filter's liquidity
    /// gate, plus whether the band actually replaced the base rate.
    ///
    /// Deliberately narrow: it only swaps the number the gate reads. The
    /// sniper's own `MIN_VOLUME_PER_DAY` floor and `refine_confidence` keep
    /// seeing the base rate, so enabling this cannot reject a flip through a
    /// path that was never measured.
    pub fn reported_volume(
        &self,
        attrs: &ItemAttributes,
        price: f64,
        resale: f64,
        base_volume: f64,
    ) -> (f64, bool) {
        self.reported_volume_inner(
            attrs,
            price,
            resale,
            base_volume,
            *BAND_MIN_BUY,
            *BAND_ASK_MULT,
        )
    }

    /// `min_buy`/`ask_mult` are injected so both paths are unit-testable without
    /// touching process env, the same shape as `Filter::evaluate_flip_inner`.
    fn reported_volume_inner(
        &self,
        attrs: &ItemAttributes,
        price: f64,
        resale: f64,
        base_volume: f64,
        min_buy: f64,
        ask_mult: f64,
    ) -> (f64, bool) {
        if min_buy <= 0.0 || price < min_buy {
            return (base_volume, false);
        }
        match self.band_volume_per_day(attrs, resale * ask_mult) {
            Some(b) if b < base_volume => (b, true),
            _ => (base_volume, false),
        }
    }
}

#[cfg(test)]
mod band_volume_tests {
    use super::*;

    // One base item, 10 sales over 10 days: 6 cheap (10M) and 4 dear (60M).
    // volume_per_day = 10/10 = 1.0, and only the dear four clear a 60M ask.
    fn model() -> ModifierModel {
        let mut sale_prices = vec![10e6, 10e6, 10e6, 10e6, 10e6, 10e6, 60e6, 60e6, 60e6, 60e6];
        sort_asc(&mut sale_prices);
        let mut models = HashMap::new();
        models.insert(
            "TEST_ITEM".to_string(),
            BaseModel {
                baseline: 10e6,
                baseline_count: 10,
                baseline_feats: Vec::new(),
                feature_value: HashMap::new(),
                combo_correction: HashMap::new(),
                volume_per_day: 1.0,
                volatility: 0.1,
                sale_prices,
                span_days: 10.0,
            },
        );
        ModifierModel { models }
    }

    fn attrs() -> ItemAttributes {
        serde_json::from_str(r#"{"id":"TEST_ITEM"}"#).unwrap()
    }

    #[test]
    fn band_at_zero_ask_equals_base_volume() {
        // The band is a filter over the same population, so an ask nothing can
        // fall below must reproduce volume_per_day exactly.
        assert_eq!(model().band_volume_per_day(&attrs(), 0.0), Some(1.0));
    }

    #[test]
    fn band_counts_only_sales_at_or_above_the_ask() {
        let m = model();
        assert_eq!(m.band_volume_per_day(&attrs(), 60e6), Some(0.4)); // 4 of 10 over 10 days
        assert_eq!(m.band_volume_per_day(&attrs(), 60e6 + 1.0), Some(0.0));
        assert_eq!(m.band_volume_per_day(&attrs(), 10e6), Some(1.0)); // inclusive at the ask
    }

    #[test]
    fn unknown_base_key_has_no_band() {
        let a: ItemAttributes = serde_json::from_str(r#"{"id":"NOT_MODELLED"}"#).unwrap();
        assert_eq!(model().band_volume_per_day(&a, 1.0), None);
    }

    #[test]
    fn off_by_default_reports_the_base_volume() {
        // min_buy 0 = flag unset = the pre-existing behaviour the goldens pin.
        let (v, applied) = model().reported_volume_inner(&attrs(), 50e6, 63e6, 1.0, 0.0, 0.95);
        assert_eq!((v, applied), (1.0, false));
    }

    #[test]
    fn below_the_capital_threshold_reports_the_base_volume() {
        let (v, applied) = model().reported_volume_inner(&attrs(), 20e6, 63e6, 1.0, 50e6, 0.95);
        assert_eq!((v, applied), (1.0, false));
    }

    #[test]
    fn tail_priced_buy_reports_the_thinner_band() {
        // 50M buy, model target 63M ⇒ ask 59.85M ⇒ only the four 60M sales
        // qualify ⇒ 0.4/day, not the base item's 1.0/day.
        let (v, applied) = model().reported_volume_inner(&attrs(), 50e6, 63e6, 1.0, 50e6, 0.95);
        assert_eq!((v, applied), (0.4, true));
    }

    #[test]
    fn band_never_raises_the_reported_volume() {
        // An ask below every sale gives band == base; it must not be flagged as
        // applied, so a cheap-relative-to-market flip keeps today's routing.
        let (v, applied) = model().reported_volume_inner(&attrs(), 50e6, 1e6, 1.0, 50e6, 0.95);
        assert_eq!((v, applied), (1.0, false));
    }
}
