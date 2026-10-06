//! Port of the decision half of `baf-flip-finder/src/filter.ts` — BinMaster-
//! compatible `evaluateFlip`. Pinned by `goldens/filter/decisions.json`.
//! (loadFilter/saveFilterText/watch — the file-IO + web-editor half — are Phase 3
//! wsServer concerns and have no domain-core golden.)

use crate::nbt::ItemAttributes;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

const GLOBAL_CATCHALL_SCALE: f64 = 0.97;

#[derive(Debug, Clone, Deserialize)]
pub struct BinMasterFilter {
    pub global_min_profit: Option<f64>,
    pub global_min_profit_percent: Option<f64>,
    pub global_min_volume: Option<f64>,
    pub global_min_confidence: Option<f64>,
    pub item_specific_filters: HashMap<String, Vec<FilterRule>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FilterRule {
    pub filter_type: Value,
    #[serde(default)]
    pub matcher: Value,
    #[serde(default)]
    pub scale_price: Option<f64>,
}

/// The flip fields `evaluateFlip` reads (medianStats.volumePerDay flattened).
#[derive(Debug, Clone, Deserialize)]
pub struct FilterFlip {
    pub attrs: ItemAttributes,
    pub profit: f64,
    #[serde(rename = "roiPct")]
    pub roi_pct: f64,
    pub confidence: f64,
    #[serde(rename = "volumePerDay", default)]
    pub volume_per_day: Option<f64>,
    /// Phase 1: the finder's REAL fair time-to-sell in ms (median hours-to-sell
    /// among sales near this key's median price, ×3.6e6). None on the golden /
    /// one-shot paths and whenever no TTS history exists, so the filter falls back
    /// to the volume proxy exactly as before.
    #[serde(rename = "fairTtsMs", default)]
    pub fair_tts_ms: Option<f64>,
    /// Fair-TTS sample count backing `fair_tts_ms`; the real TTS is only trusted
    /// once this clears `MIN_TTS_SAMPLES`.
    #[serde(rename = "ttsSamples", default)]
    pub tts_samples: Option<i64>,
    /// Item-level P(a listing sells within the sell-through horizon), Kaplan-Meier
    /// over sold and censored listings. None on the golden / one-shot paths and
    /// whenever the item has no censored history, in which case the TTS waiver is
    /// refused rather than granted blind — an unknown sell-through is not a good
    /// one. See [`crate::survival`].
    #[serde(rename = "sellThrough", default)]
    pub sell_through: Option<f64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FilterDecision {
    pub pass: bool,
    pub scale_price: f64,
    pub priority: f64,
    pub reason: Option<String>,
}

pub struct Filter {
    filter: Option<BinMasterFilter>,
}

/// Phase 1 (2026-07-17): when set, a tier's liquidity gate judges on the finder's
/// REAL fair time-to-sell (`fairTtsMs`) rather than the `86.4M/volume` proxy, and
/// a confirmed-fast real TTS satisfies that tier's `min_volume` (volume was only
/// ever a proxy for the same liquidity). Default OFF ⇒ byte-identical to the
/// volume behaviour; the goldens carry no TTS data, so it is a strict no-op until
/// switched on with `TTS_LIQUIDITY=1`. Rationale: over 3h of live flips, real TTS
/// ran a median 0.56× of what volume predicted and 64% of flips sold FASTER than
/// volume implied, so the volume floor was dropping fast-selling low-volume money.
static TTS_LIQUIDITY: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
    matches!(
        std::env::var("TTS_LIQUIDITY").as_deref(),
        Ok("1") | Ok("true")
    )
});
/// Fair-TTS observations required before the real TTS is trusted over volume.
const MIN_TTS_SAMPLES: i64 = 6;

/// JS `${n}` for a number (integer-valued → no decimal).
fn jn(x: f64) -> String {
    format!("{x}")
}

/// filter.ts `fmt`: ≥1e6 → "N.NM"; else "Nk" (rounded).
fn fmt(n: f64) -> String {
    if n.abs() >= 1e6 {
        format!("{:.1}M", n / 1e6)
    } else {
        format!("{}k", (n / 1000.0).round() as i64)
    }
}

fn filter_item_id(attrs: &ItemAttributes) -> String {
    if let Some(pet) = &attrs.pet {
        if !pet.pet_type.is_empty() {
            return format!("PET_{}", pet.pet_type.to_uppercase());
        }
    }
    attrs.id.to_uppercase()
}

/// Resolve a matcher field to a numeric value, or None.
fn field_value(attrs: &ItemAttributes, field: &str) -> Option<f64> {
    if let Some(v) = attrs.enchantments.get(field) {
        return Some(*v);
    }
    if let Some(v) = attrs.attributes.get(field) {
        return Some(*v);
    }
    if let Some(v) = attrs.extras.get(field) {
        return Some(*v);
    }
    match field {
        "gem_slots" => Some(attrs.gem_slots as f64),
        "stars" | "upgrade_level" => Some(attrs.upgrade_level.unwrap_or(0.0)),
        "recombobulated" => Some(if attrs.recombobulated { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn matcher_matches(attrs: &ItemAttributes, matcher: &Value) -> bool {
    let obj = match matcher.as_object() {
        Some(m) => m,
        None => return true, // matcher ?? {} → no fields → matches all
    };
    if obj.is_empty() {
        return true;
    }
    for (field, cond) in obj {
        let num = match cond.get("Number") {
            Some(v) if !v.is_null() => v,
            _ => return false,
        };
        let val = match field_value(attrs, field) {
            Some(v) => v,
            None => return false,
        };
        let arr = match num.get("Equal").and_then(|e| e.as_array()) {
            Some(a) => a,
            None => return false,
        };
        let ok = if arr.len() == 2 {
            let a = arr[0].as_f64().unwrap_or(f64::NAN);
            let b = arr[1].as_f64().unwrap_or(f64::NAN);
            val >= a && val <= b
        } else {
            arr.iter().any(|x| x.as_f64() == Some(val))
        };
        if !ok {
            return false;
        }
    }
    true
}

fn tier_num(c: &Value, key: &str) -> Option<f64> {
    c.get(key).and_then(|v| v.as_f64())
}

impl Filter {
    pub fn new(filter: Option<BinMasterFilter>) -> Self {
        Filter { filter }
    }

    pub fn evaluate_flip(&self, f: &FilterFlip) -> FilterDecision {
        self.evaluate_flip_inner(
            f,
            *TTS_LIQUIDITY,
            *crate::config::TTS_MIN_SELL_THROUGH,
            (
                *crate::config::EXTREME_FLIP_MIN_PROFIT,
                *crate::config::EXTREME_FLIP_MIN_ROI_PCT,
            ),
        )
    }

    /// `tts_liquidity`, `min_sell_through` and the `extreme` waiver pair
    /// (min profit, min ROI pct) are injected so tests can exercise every path
    /// without the process-global env `LazyLock`s; production reads them from
    /// `TTS_LIQUIDITY`, `TTS_MIN_SELL_THROUGH` and `EXTREME_FLIP_MIN_*`.
    fn evaluate_flip_inner(
        &self,
        f: &FilterFlip,
        tts_liquidity: bool,
        min_sell_through: f64,
        extreme: (f64, f64),
    ) -> FilterDecision {
        let miss = |reason: String| FilterDecision {
            pass: false,
            scale_price: 1.0,
            priority: 0.0,
            reason: Some(reason),
        };
        let filter = match &self.filter {
            None => {
                return FilterDecision {
                    pass: true,
                    scale_price: 1.0,
                    priority: 0.0,
                    reason: None,
                }
            }
            Some(f) => f,
        };

        let volume = f.volume_per_day;
        let id = filter_item_id(&f.attrs);
        let mut rules: Vec<&FilterRule> = Vec::new();
        if let Some(g) = filter.item_specific_filters.get("GLOBAL") {
            rules.extend(g.iter());
        }
        if let Some(r) = filter.item_specific_filters.get(&id) {
            rules.extend(r.iter());
        }

        // Blacklist wins.
        for r in &rules {
            if r.filter_type.get("Blacklist").is_some()
                && r.filter_type.get("Consider").is_none()
                && matcher_matches(&f.attrs, &r.matcher)
            {
                return miss(format!("blacklisted ({id})"));
            }
        }

        let g_profit = filter.global_min_profit.unwrap_or(0.0);
        let g_roi = filter.global_min_profit_percent.unwrap_or(0.0);
        let g_conf = filter.global_min_confidence.unwrap_or(0.0);
        let g_vol = filter.global_min_volume.unwrap_or(0.0);
        let passes_global = f.profit >= g_profit
            && f.roi_pct >= g_roi
            && f.confidence >= g_conf
            && (volume.is_none() || volume.unwrap() >= g_vol);

        // Extreme-margin waiver (`EXTREME_FLIP_MIN_PROFIT`): when the buy price
        // is a trivial fraction of the reference, a reference error costs the
        // buy price, not the margin — so the tier `min_confidence` and
        // `min_volume` quality gates stop being evidence about the trade.
        // Measured 2026-08-15: a drill at buy 253,100 vs a 22.95M reference
        // (8214% ROI, 20.79M profit) was declined `conf<88%`; even a 99%
        // estimate error still clears profit there. Only those two tier checks
        // are skipped — profit, roi, `max_time_to_sell`, matchers and the
        // blacklist all apply exactly — and both thresholds must clear, so a
        // merely LARGE or merely HIGH-ROI flip is untouched. Default 0 = off,
        // byte-identical to the old behaviour.
        let extreme = extreme.0 > 0.0 && f.profit >= extreme.0 && f.roi_pct >= extreme.1;

        let mut best: Option<(f64, f64)> = None; // (priority, scale)
        let mut last_reason = format!(
            "below global (profit {}, roi {}%, conf {}%) and no tier matched",
            fmt(f.profit),
            (f.roi_pct.round() as i64),
            (f.confidence * 100.0).round() as i64
        );
        // Real fair-TTS (ms) when Phase-1 liquidity is on AND enough samples back
        // it; else None so `est_sell_ms` falls back to the volume proxy — the exact
        // pre-Phase-1 behaviour the goldens pin.
        //
        // The sell-through floor is the de-biasing half (2026-08-05). `fair_tts_ms`
        // is a median over listings that SOLD, so an item whose listings almost
        // never sell still reads fast on the few that did; 455 of 2,383 items look
        // faster than 6h on under 50% sell-through. Refusing the waiver when
        // sell-through is unknown is deliberate: `censored` covers 2,725 items, so
        // a missing value means a genuinely unseen item, and the volume floor is
        // the right fallback for those.
        let liquidity_confirmed = match f.sell_through {
            Some(st) => st >= min_sell_through,
            None => min_sell_through <= 0.0,
        };
        let real_tts_ms: Option<f64> = if tts_liquidity && liquidity_confirmed {
            match (f.fair_tts_ms, f.tts_samples) {
                (Some(t), Some(n)) if n >= MIN_TTS_SAMPLES && t > 0.0 => Some(t),
                _ => None,
            }
        } else {
            None
        };
        let est_sell_ms = real_tts_ms.or(match volume {
            Some(v) if v > 0.0 => Some(86_400_000.0 / v),
            _ => None,
        });
        for r in &rules {
            let c = match r.filter_type.get("Consider") {
                Some(c) if !c.is_null() => c,
                _ => continue,
            };
            if !matcher_matches(&f.attrs, &r.matcher) {
                continue;
            }
            if let Some(mp) = tier_num(c, "min_profit") {
                if f.profit < mp {
                    last_reason = format!("profit<{}", fmt(mp));
                    continue;
                }
            }
            if let Some(mpp) = tier_num(c, "min_profit_percent") {
                if f.roi_pct < mpp {
                    last_reason = format!("roi<{}%", jn(mpp));
                    continue;
                }
            }
            if let Some(mc) = tier_num(c, "min_confidence") {
                if f.confidence < mc && !extreme {
                    last_reason = format!("conf<{}%", (mc * 100.0).round() as i64);
                    continue;
                }
            }
            // A tier's liquidity = "vol>=mv AND sells within mtts"; volume was only
            // a proxy for that sell-time. A confirmed-fast real TTS (<= this tier's
            // window) makes the volume floor redundant, so it is waived. When
            // real_tts_ms is None (flag off / no history) tts_proves_liquid is
            // false and the volume floor applies exactly as before.
            let mtts = tier_num(c, "max_time_to_sell");
            let tts_proves_liquid = matches!((real_tts_ms, mtts), (Some(rt), Some(m)) if rt <= m);
            if let Some(mv) = tier_num(c, "min_volume") {
                if !extreme && !tts_proves_liquid && (volume.is_none() || volume.unwrap() < mv) {
                    last_reason = format!("vol<{}", jn(mv));
                    continue;
                }
            }
            if let Some(m) = mtts {
                if let Some(est) = est_sell_ms {
                    if est > m {
                        last_reason = "too slow to sell".to_string();
                        continue;
                    }
                }
            }
            let priority = tier_num(c, "priority").unwrap_or(0.0);
            let scale = r.scale_price.unwrap_or(1.0);
            if best.is_none() || priority > best.unwrap().0 {
                best = Some((priority, scale));
            }
        }
        if let Some((priority, scale)) = best {
            return FilterDecision {
                pass: true,
                scale_price: scale,
                priority,
                reason: None,
            };
        }
        if passes_global {
            return FilterDecision {
                pass: true,
                scale_price: GLOBAL_CATCHALL_SCALE,
                priority: 0.0,
                reason: None,
            };
        }
        miss(last_reason)
    }
}

#[cfg(test)]
mod tts_liquidity_tests {
    use super::*;

    // One GLOBAL tier: needs vol>=3 AND sells within 8h; profit 5M / roi 12% / conf 0.78.
    // Global catch-all is 25M/15% so a 10M flip can only pass via the tier.
    fn filter() -> Filter {
        let j = r#"{
            "global_min_profit": 25000000, "global_min_profit_percent": 15,
            "global_min_volume": 0.5, "global_min_confidence": 0.75,
            "item_specific_filters": { "GLOBAL": [ { "filter_type": { "Consider": {
                "min_profit": 5000000, "min_profit_percent": 12, "min_confidence": 0.78,
                "min_volume": 3, "max_time_to_sell": 28800000, "priority": 9 } },
                "matcher": {}, "scale_price": 0.93 } ] }
        }"#;
        Filter::new(Some(serde_json::from_str(j).unwrap()))
    }

    // vol 2.0 (below the tier's 3), profit 10M @ 50% roi, conf 0.8. fair_tts/samples
    // vary; sell-through healthy at 0.9 unless a test is about sell-through.
    fn flip(fair_tts_ms: Option<f64>, tts_samples: Option<i64>) -> FilterFlip {
        flip_st(fair_tts_ms, tts_samples, Some(0.9))
    }

    fn flip_st(
        fair_tts_ms: Option<f64>,
        tts_samples: Option<i64>,
        sell_through: Option<f64>,
    ) -> FilterFlip {
        FilterFlip {
            attrs: serde_json::from_str(r#"{"id":"TEST_ITEM"}"#).unwrap(),
            profit: 10_000_000.0,
            roi_pct: 50.0,
            confidence: 0.8,
            volume_per_day: Some(2.0),
            fair_tts_ms,
            tts_samples,
            sell_through,
        }
    }

    // The sell-through floor used by every test below that is not about it.
    const ST: f64 = 0.5;

    /// Prod-shaped GLOBAL tier ladder (2026-08-15 binmaster-filter.json). The
    /// drill case: profit 20.79M clears the 15M tier but volume 0.44/day and
    /// confidence 0.53 clear NONE of them; only the `roi 100%` tier has no
    /// `max_time_to_sell`, so it is the sole tier the waiver can pass through.
    fn prodlike_filter() -> Filter {
        let j = r#"{
            "global_min_profit": 25000000, "global_min_profit_percent": 15,
            "global_min_volume": 0.5, "global_min_confidence": 0.65,
            "item_specific_filters": { "GLOBAL": [
              { "filter_type": { "Consider": { "min_profit": 3000000, "min_profit_percent": 10,
                  "min_volume": 10, "min_confidence": 0.8, "max_time_to_sell": 10800000, "priority": 10 } },
                "matcher": {}, "scale_price": 0.91 },
              { "filter_type": { "Consider": { "min_profit": 15000000, "min_profit_percent": 12,
                  "min_volume": 1, "min_confidence": 0.78, "max_time_to_sell": 86400000, "priority": 7 } },
                "matcher": {}, "scale_price": 0.95 },
              { "filter_type": { "Consider": { "min_profit": 3000000, "min_profit_percent": 100,
                  "min_confidence": 0.7, "priority": 8 } },
                "matcher": {}, "scale_price": 0.93 },
              { "filter_type": { "Consider": { "min_profit": 400000, "min_profit_percent": 25,
                  "min_volume": 20, "min_confidence": 0.88, "max_time_to_sell": 3600000, "priority": 4 } },
                "matcher": {}, "scale_price": 0.9 }
            ] }
        }"#;
        Filter::new(Some(serde_json::from_str(j).unwrap()))
    }

    /// The 2026-08-15 drill: buy 253,100, reference 22.95M, conf 0.53, vol 0.44/day.
    fn drill_flip() -> FilterFlip {
        FilterFlip {
            attrs: serde_json::from_str(r#"{"id":"GEMSTONE_DRILL_3"}"#).unwrap(),
            profit: 20_785_207.0,
            roi_pct: 8214.0,
            confidence: 0.53,
            volume_per_day: Some(0.44),
            fair_tts_ms: None,
            tts_samples: None,
            sell_through: Some(0.63),
        }
    }

    #[test]
    fn waiver_off_still_rejects_the_drill_exactly_as_before() {
        let d = prodlike_filter().evaluate_flip_inner(&drill_flip(), false, ST, (0.0, 0.0));
        assert!(
            !d.pass,
            "waiver off must preserve the old reject byte-for-byte"
        );
        assert!(d.reason.unwrap().contains("conf<88%"));
    }

    #[test]
    fn waiver_on_passes_the_drill_via_the_no_mtts_tier() {
        let d =
            prodlike_filter().evaluate_flip_inner(&drill_flip(), false, ST, (5_000_000.0, 500.0));
        assert!(d.pass, "an extreme-margin flip clears conf+volume tiers");
        assert_eq!(
            d.priority, 8.0,
            "lands on the roi-100 tier, not a higher bar"
        );
        assert!((d.scale_price - 0.93).abs() < 1e-9);
    }

    #[test]
    fn waiver_does_not_apply_below_both_thresholds() {
        let mut f = drill_flip();
        f.profit = 4_000_000.0; // profit clears a 5M waiver bar? no
        let d = prodlike_filter().evaluate_flip_inner(&f, false, ST, (5_000_000.0, 500.0));
        assert!(!d.pass, "profit below the waiver bar keeps the old reject");
        let mut f = drill_flip();
        f.roi_pct = 300.0; // roi below the 500% waiver bar
        let d = prodlike_filter().evaluate_flip_inner(&f, false, ST, (5_000_000.0, 500.0));
        assert!(!d.pass, "roi below the waiver bar keeps the old reject");
    }

    #[test]
    fn waiver_never_skips_mtts() {
        // Every Consider tier here is mtts-gated and the item is a slow trader,
        // so even an extreme flip must still be refused: sell-time is not a
        // quality-metric artifact, it is slot time the bot actually pays.
        let j = r#"{
            "global_min_profit": 25000000, "global_min_profit_percent": 15,
            "global_min_volume": 0.5, "global_min_confidence": 0.65,
            "item_specific_filters": { "GLOBAL": [
              { "filter_type": { "Consider": { "min_profit": 3000000, "min_profit_percent": 10,
                  "min_volume": 10, "min_confidence": 0.8, "max_time_to_sell": 3600000, "priority": 9 } },
                "matcher": {}, "scale_price": 0.91 }
            ] }
        }"#;
        let f = Filter::new(Some(serde_json::from_str(j).unwrap()));
        let d = f.evaluate_flip_inner(&drill_flip(), false, ST, (5_000_000.0, 500.0));
        assert!(!d.pass, "max_time_to_sell is never waived");
        assert!(d.reason.unwrap().contains("too slow to sell"));
    }

    #[test]
    fn waiver_loses_to_blacklist() {
        let j = r#"{
            "global_min_profit": 25000000, "global_min_profit_percent": 15,
            "global_min_volume": 0.5, "global_min_confidence": 0.65,
            "item_specific_filters": { "GEMSTONE_DRILL_3": [
              { "filter_type": { "Blacklist": null }, "matcher": {}, "scale_price": 1.0 }
            ] }
        }"#;
        let f = Filter::new(Some(serde_json::from_str(j).unwrap()));
        let d = f.evaluate_flip_inner(&drill_flip(), false, ST, (5_000_000.0, 500.0));
        assert!(!d.pass, "blacklist wins over the waiver");
        assert!(d.reason.unwrap().contains("blacklisted"));
    }

    #[test]
    fn off_path_rejects_low_volume_even_with_fast_tts() {
        // Flag OFF ⇒ volume proxy only ⇒ vol 2 < 3 ⇒ dropped, TTS ignored (parity).
        let d =
            filter().evaluate_flip_inner(&flip(Some(5_400_000.0), Some(20)), false, ST, (0.0, 0.0));
        assert!(!d.pass);
        assert!(d.reason.unwrap().contains("vol<3"));
    }

    #[test]
    fn on_path_fast_real_tts_waives_the_volume_floor() {
        // Flag ON, real TTS 1.5h <= tier 8h, 20 samples, sell-through 90% ⇒ passes.
        let d =
            filter().evaluate_flip_inner(&flip(Some(5_400_000.0), Some(20)), true, ST, (0.0, 0.0));
        assert!(
            d.pass,
            "fast-selling low-volume flip should pass on the TTS path"
        );
    }

    #[test]
    fn on_path_slow_real_tts_still_rejected() {
        // Flag ON but real TTS 10h > tier 8h ⇒ genuinely illiquid ⇒ still dropped.
        let d =
            filter().evaluate_flip_inner(&flip(Some(36_000_000.0), Some(20)), true, ST, (0.0, 0.0));
        assert!(
            !d.pass,
            "slow low-volume flip must NOT pass just because TTS is known"
        );
    }

    #[test]
    fn on_path_thin_samples_fall_back_to_volume() {
        // Flag ON, fast TTS but only 2 samples (< MIN_TTS_SAMPLES) ⇒ untrusted ⇒
        // volume proxy applies ⇒ vol 2 < 3 ⇒ dropped.
        let d =
            filter().evaluate_flip_inner(&flip(Some(5_400_000.0), Some(2)), true, ST, (0.0, 0.0));
        assert!(!d.pass);
        assert!(d.reason.unwrap().contains("vol<3"));
    }

    #[test]
    fn the_redstone_ore_trap_no_longer_waives_the_volume_floor() {
        // The measured shape: 2 sales at ~0.22h against 78 listings that never sold.
        // The naive median says 0.22h — comfortably inside the tier's 8h — so the
        // pre-2026-08-05 waiver let it through on a 2.5% sell-through.
        let d = filter().evaluate_flip_inner(
            &flip_st(Some(792_000.0), Some(20), Some(0.025)),
            true,
            ST,
            (0.0, 0.0),
        );
        assert!(!d.pass, "a 2.5% sell-through must not buy a volume waiver");
        assert!(d.reason.unwrap().contains("vol<3"));
    }

    #[test]
    fn unknown_sell_through_refuses_the_waiver_rather_than_assuming_the_best() {
        let d = filter().evaluate_flip_inner(
            &flip_st(Some(5_400_000.0), Some(20), None),
            true,
            ST,
            (0.0, 0.0),
        );
        assert!(
            !d.pass,
            "an unmeasured item must fall back to the volume floor"
        );
        assert!(d.reason.unwrap().contains("vol<3"));
    }

    #[test]
    fn a_zero_floor_restores_the_unguarded_waiver() {
        // TTS_MIN_SELL_THROUGH=0 is the documented rollback; it must behave exactly
        // like the pre-de-biasing code, unknown sell-through included.
        let d = filter().evaluate_flip_inner(
            &flip_st(Some(5_400_000.0), Some(20), None),
            true,
            0.0,
            (0.0, 0.0),
        );
        assert!(d.pass, "floor 0 must reproduce the old unguarded waiver");
        let d = filter().evaluate_flip_inner(
            &flip_st(Some(792_000.0), Some(20), Some(0.025)),
            true,
            0.0,
            (0.0, 0.0),
        );
        assert!(d.pass, "floor 0 must not apply any sell-through condition");
    }

    #[test]
    fn sell_through_exactly_at_the_floor_passes() {
        let d = filter().evaluate_flip_inner(
            &flip_st(Some(5_400_000.0), Some(20), Some(0.5)),
            true,
            ST,
            (0.0, 0.0),
        );
        assert!(d.pass, "the floor is inclusive");
    }
}
