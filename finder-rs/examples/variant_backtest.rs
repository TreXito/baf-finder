//! Does VARIANT_SIG price more accurately, or just differently?
//!
//! Honest backtest, no lookahead: build the index from sales BEFORE a cutoff,
//! then predict the price of items that sold AFTER it and compare to what they
//! actually fetched. Run once with VARIANT_SIG off and once on.
//!
//! Two things matter and they trade off:
//!   COVERAGE  - share of sales we can price at all. Collapsing worthless
//!               variants should raise this (that is the notpriceable fix).
//!   ACCURACY  - |predicted - actual| / actual. Collapsing a variant that DOES
//!               carry value would show up here as a worse tail.
//!
//! Bias is reported separately because underselling and overselling are both
//! bad but are different failures: negative bias = we target too low = we
//! undersell.
//!
//! usage: variant_backtest <sqlite> [holdout_days]
use finder_core::bazaar::Bazaar;
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{PriceIndex, Reference};
use std::collections::HashMap;

fn main() {
    let db = std::env::args()
        .nth(1)
        .expect("usage: variant_backtest <sqlite> [holdout_days]");
    let holdout_days: i64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);

    let conn = rusqlite::Connection::open_with_flags(
        db.as_str(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open db");

    let max_sold: i64 = conn
        .query_row("SELECT MAX(sold_at) FROM sold", [], |r| r.get(0))
        .expect("max sold_at");
    let cutoff = max_sold - holdout_days * 86_400;
    eprintln!("  max sold_at={max_sold}  cutoff={cutoff}  holdout={holdout_days}d");

    let mut stmt = conn
        .prepare("SELECT price, sold_at, seller, attrs, tts_ms FROM sold WHERE attrs IS NOT NULL AND attrs != '' AND price > 0")
        .unwrap();
    let mut train: Vec<Reference> = Vec::new();
    let mut test: Vec<(ItemAttributes, f64)> = Vec::new();

    let rows = stmt
        .query_map([], |r| {
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
    eprintln!("  train={}  test={}", train.len(), test.len());

    let now_ms = max_sold * 1000;
    let idx = PriceIndex::build(train, Bazaar::from_prices(HashMap::new(), now_ms), now_ms);

    let mut errs: Vec<f64> = Vec::new();
    let mut signed: Vec<f64> = Vec::new();
    let mut priced = 0usize;
    for (attrs, actual) in &test {
        if let Some(stats) = idx.price_for(attrs) {
            let pred = stats.target;
            if pred > 0.0 && *actual > 0.0 {
                priced += 1;
                errs.push(((pred - actual) / actual).abs());
                signed.push((pred - actual) / actual);
            }
        }
    }

    let pct = |v: &mut Vec<f64>, q: f64| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[((v.len() as f64 - 1.0) * q) as usize]
    };

    println!();
    println!(
        "  VARIANT_SIG = {}",
        if *finder_core::config::VARIANT_SIG {
            "ON"
        } else {
            "off"
        }
    );
    println!(
        "  coverage : {priced}/{} = {:.1}%",
        test.len(),
        priced as f64 / test.len() as f64 * 100.0
    );
    if !errs.is_empty() {
        let mut e = errs.clone();
        let mut s = signed.clone();
        println!(
            "  abs err  : p50={:.1}%  p75={:.1}%  p90={:.1}%",
            pct(&mut e, 0.50) * 100.0,
            pct(&mut e, 0.75) * 100.0,
            pct(&mut e, 0.90) * 100.0
        );
        println!(
            "  bias     : p50={:+.1}%   (negative = target below realised = underselling)",
            pct(&mut s, 0.50) * 100.0
        );
    }
}
