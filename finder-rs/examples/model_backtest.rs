//! Does MODEL_REF_COMBO price the unmodellable half of the catalogue, and does
//! it price it ACCURATELY?
//!
//! Honest holdout, no lookahead: build the index and the model from sales before
//! a cutoff, then predict what items sold for after it. Scored only on the sales
//! that actually take the model path in `eval_median_flip`, i.e. the ones where
//! `price_for` finds no direct pool, because those are the ones that come back
//! `notpriceable` today.
//!
//! Run twice, MODEL_REF_COMBO=0 then =1, and compare. Three numbers decide it:
//!
//!   COVERAGE  - share of those sales we can price at all. This is the recall
//!               win and it should jump.
//!   ABS ERR   - |pred - actual| / actual. A wider tail means the new prices are
//!               noise.
//!   OVERSHOOT - share priced at more than 1.3x what the item actually fetched.
//!               THE number that matters: overestimating is what buys junk and
//!               strands it, underestimating only costs a flip we already miss.
//!
//! usage: model_backtest <sqlite> [holdout_days] [window_days]
use finder_core::bazaar::Bazaar;
use finder_core::config::MIN_REFS;
use finder_core::modifier_model::ModifierModel;
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{base_key, PriceIndex, Reference};
use std::collections::HashMap;

fn pct(v: &mut Vec<f64>, q: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * q) as usize]
}

fn main() {
    let db = std::env::args()
        .nth(1)
        .expect("usage: model_backtest <sqlite> [holdout_days] [window_days]");
    let holdout: i64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let window: i64 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(7);

    let conn = rusqlite::Connection::open_with_flags(
        db.as_str(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open db");

    let max_sold: i64 = conn
        .query_row("SELECT MAX(sold_at) FROM sold", [], |r| r.get(0))
        .unwrap();
    let cutoff = max_sold - holdout * 86_400;
    let start = cutoff - window * 86_400;

    let mut stmt = conn
        .prepare(
            "SELECT price, sold_at, seller, attrs, tts_ms FROM sold \
             WHERE attrs IS NOT NULL AND attrs != '' AND price > 0 AND sold_at >= ?1",
        )
        .unwrap();
    let mut train: Vec<Reference> = Vec::new();
    let mut test: Vec<(ItemAttributes, f64)> = Vec::new();
    let rows = stmt
        .query_map([start], |r| {
            Ok((
                r.get::<_, f64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, String>(3)?,
                r.get::<_, Option<f64>>(4)?,
            ))
        })
        .unwrap();
    for row in rows.flatten() {
        let (price, sold_at, seller, attrs_json, tts_ms) = row;
        let Ok(attrs) = serde_json::from_str::<ItemAttributes>(&attrs_json) else {
            continue;
        };
        if sold_at < cutoff {
            train.push(Reference {
                price,
                sold_at: sold_at as f64 * 1000.0,
                seller,
                tts_ms,
                attrs,
            });
        } else {
            test.push((attrs, price));
        }
    }
    eprintln!(
        "  train={}  test={}  holdout={holdout}d  window={window}d",
        train.len(),
        test.len()
    );

    let now_ms = cutoff * 1000;
    let idx = PriceIndex::build(
        train.clone(),
        Bazaar::from_prices(HashMap::new(), now_ms),
        now_ms,
    );
    let model = ModifierModel::rebuild(&train, &idx, now_ms);
    eprintln!("  models built={}", model.model_count());

    // Only the sales that reach the model path: no direct pool for the key.
    let mut model_path = 0usize;
    let mut priced = 0usize;
    let mut errs: Vec<f64> = Vec::new();
    let mut signed: Vec<f64> = Vec::new();
    let mut overshoot = 0usize;
    let mut over_by_value = 0.0f64;
    let mut worst: Vec<(f64, String, f64, f64)> = Vec::new();

    for (attrs, actual) in &test {
        if idx.price_for(attrs).is_some() {
            continue;
        }
        model_path += 1;
        let Some(est) = model.estimate_for(attrs, &idx) else {
            continue;
        };
        // eval_median_flip requires this before it will use the estimate.
        if est.samples < *MIN_REFS as i64 {
            continue;
        }
        let pred = est.target;
        // ⚠️ `actual > 0` is not enough. Real sales land at 1-17 coins (junk
        // listings, mispriced dumps), and scoring a 19.3M prediction against a
        // 17-coin sale reports +113,309,892% and dominates the overshoot metric.
        // Score only on sales large enough that a misprice costs real money —
        // `hardMinProfit` is 3M, so nothing below MIN_ACTUAL can drive a buy.
        let min_actual: f64 = std::env::var("MIN_ACTUAL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_000_000.0);
        if pred <= 0.0 || *actual < min_actual {
            continue;
        }
        priced += 1;
        let rel = (pred - actual) / actual;
        errs.push(rel.abs());
        signed.push(rel);
        if pred > actual * 1.3 {
            overshoot += 1;
            over_by_value += pred - actual;
            worst.push((rel, base_key(attrs), pred, *actual));
        }
    }

    // Per-sale predictions, so the two runs can be diffed down to the sales that
    // ONLY the new path can price. The aggregate above would hide those if they
    // priced badly but were diluted by the ones both runs share.
    if let Ok(path) = std::env::var("DUMP_PREDS") {
        use std::io::Write;
        let mut f = std::fs::File::create(&path).expect("create DUMP_PREDS");
        for (i, (attrs, actual)) in test.iter().enumerate() {
            if idx.price_for(attrs).is_some() {
                continue;
            }
            let pred = model
                .estimate_for(attrs, &idx)
                .filter(|e| e.samples >= *MIN_REFS as i64)
                .map(|e| e.target)
                .unwrap_or(0.0);
            writeln!(f, "{i}\t{pred}\t{actual}\t{}", base_key(attrs)).unwrap();
        }
    }

    let on = std::env::var("MODEL_REF_COMBO").unwrap_or_default();
    println!();
    println!(
        "  MODEL_REF_COMBO = {}",
        if on == "1" { "ON" } else { "off" }
    );
    println!("  model-path sales : {model_path}");
    println!(
        "  coverage         : {priced}/{model_path} = {:.1}%",
        priced as f64 / model_path.max(1) as f64 * 100.0
    );
    if !errs.is_empty() {
        let mut e = errs.clone();
        let mut s = signed.clone();
        println!(
            "  abs err          : p50={:.1}%  p75={:.1}%  p90={:.1}%",
            pct(&mut e, 0.50) * 100.0,
            pct(&mut e, 0.75) * 100.0,
            pct(&mut e, 0.90) * 100.0
        );
        println!(
            "  bias             : p50={:+.1}%  (negative = priced below realised = safe)",
            pct(&mut s, 0.50) * 100.0
        );
        println!(
            "  overshoot >1.3x  : {overshoot}/{priced} = {:.1}%   ({:.1}M coins of imagined value)",
            overshoot as f64 / priced as f64 * 100.0,
            over_by_value / 1e6
        );
    }
    worst.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    if !worst.is_empty() {
        println!("  worst overshoots :");
        for (rel, bk, pred, actual) in worst.iter().take(8) {
            let short: String = bk.chars().take(38).collect();
            println!(
                "    {short:<40} pred={:>9.1}M actual={:>9.1}M  {:+.0}%",
                pred / 1e6,
                actual / 1e6,
                rel * 100.0
            );
        }
    }
}
