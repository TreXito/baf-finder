//! What is left after MODEL_REF_COMBO, and can a base-key median price it?
//!
//! Even with the reference-combo anchor, ~48% of model-path sales still have no
//! model, because their base key has no combo with MIN_REFS samples either. The
//! only anchor left for those is the base key's own median (`base_value_for`),
//! which is what `cheap_flip_rescue` already uses -- except that rescue is
//! gated behind a non-empty variant AND a RESCUE_MIN_MULT of 10x, so it fires
//! only on absurd mispricings.
//!
//! This asks whether that anchor is trustworthy enough to relax, by scoring it
//! against ground truth on exactly the sales that nothing else can price.
//! Reported per minimum-sample-count, because the whole question is where the
//! evidence bar should sit.
//!
//! usage: fallback_probe <sqlite> [holdout_days] [window_days]
use finder_core::bazaar::Bazaar;
use finder_core::config::MIN_REFS;
use finder_core::modifier_model::ModifierModel;
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{base_key, PriceIndex, Reference};
use std::collections::HashMap;

fn q(v: &mut Vec<f64>, p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p) as usize]
}

fn main() {
    let db = std::env::args()
        .nth(1)
        .expect("usage: fallback_probe <sqlite> [holdout] [window]");
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
    let now_ms = cutoff * 1000;
    let idx = PriceIndex::build(
        train.clone(),
        Bazaar::from_prices(HashMap::new(), now_ms),
        now_ms,
    );
    let model = ModifierModel::rebuild(&train, &idx, now_ms);

    // Sales nothing can currently price: no direct pool AND no model.
    let mut orphans: Vec<(&ItemAttributes, f64)> = Vec::new();
    for (attrs, actual) in &test {
        if idx.price_for(attrs).is_some() {
            continue;
        }
        if model
            .estimate_for(attrs, &idx)
            .map(|e| e.samples >= *MIN_REFS as i64)
            .unwrap_or(false)
        {
            continue;
        }
        orphans.push((attrs, *actual));
    }
    let orphan_value: f64 = orphans.iter().map(|(_, a)| *a).sum();
    println!();
    println!(
        "  unpriceable by ANY lane: {} sales, {:.2}B coins realised in the holdout",
        orphans.len(),
        orphan_value / 1e9
    );

    println!();
    println!("  base-key median as the anchor, by evidence bar:");
    println!(
        "    {:<10}{:>9}{:>10}{:>10}{:>10}{:>10}",
        "min n", "covered", "abserr50", "abserr90", "bias50", "over1.3x"
    );
    for min_n in [5i64, 10, 20, 40, 80] {
        let mut errs: Vec<f64> = Vec::new();
        let mut sig: Vec<f64> = Vec::new();
        let mut over = 0usize;
        for (attrs, actual) in &orphans {
            let bk = base_key(attrs);
            let med = idx.base_value_for(&bk);
            let n = idx.sold_count_for_base(&bk);
            if med <= 0.0 || n < min_n || *actual <= 0.0 {
                continue;
            }
            let rel = (med - actual) / actual;
            errs.push(rel.abs());
            sig.push(rel);
            if med > actual * 1.3 {
                over += 1;
            }
        }
        if errs.is_empty() {
            println!("    {min_n:<10}{:>9}", 0);
            continue;
        }
        let cov = errs.len();
        println!(
            "    {min_n:<10}{cov:>9}{:>9.1}%{:>9.1}%{:>9.1}%{:>9.1}%",
            q(&mut errs.clone(), 0.50) * 100.0,
            q(&mut errs.clone(), 0.90) * 100.0,
            q(&mut sig.clone(), 0.50) * 100.0,
            over as f64 / cov as f64 * 100.0
        );
    }

    // The rescue only acts when the ask is a fraction of the anchor, so score
    // the anchor on the subset where it would actually fire, by multiple.
    println!();
    println!("  ...but the rescue only fires when median/ask >= MULT, so score THAT subset.");
    println!("  Simulated on realised sales: treat the realised price as the ask and ask");
    println!("  how often the anchor would have been an OVERestimate at each bar.");
    println!(
        "    {:<10}{:>10}{:>12}{:>12}",
        "mult", "would fire", "median err", "over 1.3x"
    );
    for mult in [2.0f64, 3.0, 5.0, 10.0] {
        let mut errs: Vec<f64> = Vec::new();
        let mut over = 0usize;
        for (attrs, actual) in &orphans {
            let bk = base_key(attrs);
            let med = idx.base_value_for(&bk);
            let n = idx.sold_count_for_base(&bk);
            if med <= 0.0 || n < *MIN_REFS as i64 || *actual <= 0.0 {
                continue;
            }
            if med / *actual < mult {
                continue;
            }
            let rel = (med - actual) / actual;
            errs.push(rel.abs());
            if med > actual * 1.3 {
                over += 1;
            }
        }
        let c = errs.len();
        println!(
            "    {mult:<10}{c:>10}{:>11.1}%{:>11.1}%",
            if c > 0 {
                q(&mut errs.clone(), 0.50) * 100.0
            } else {
                f64::NAN
            },
            if c > 0 {
                over as f64 / c as f64 * 100.0
            } else {
                f64::NAN
            }
        );
    }
}
