//! Is the `cheap_median * 1.3` pre-gate calibrated, or does it discard flips
//! before anything is ever priced?
//!
//! `eval_median_flip` opens with a COARSE pre-filter:
//!
//! ```ignore
//! if let Some(qt) = index.cheap_median(key) {
//!     if price > qt * 1.3 * (1.0 - MIN_MARGIN) * PRESCREEN_SLACK { reject Margin }
//! }
//! ```
//!
//! `PRESCREEN_SLACK` defaults to 1.0 and prod does not set it, so the live
//! ceiling is `cheap_median * 1.3 * 0.88` = **1.144x** the cheap median. Any ask
//! above that dies without the stats or model lane ever seeing it.
//!
//! This measures the ceiling against ground truth: for every sale in a holdout,
//! what did the item ACTUALLY fetch relative to its own key's cheap median? Sales
//! that realised above the ceiling are proof the pre-gate is cutting into real
//! prices rather than filtering absurd ones, because an ask at that level would
//! have been rejected unpriced even though the item genuinely sells there.
//!
//! usage: prescreen_probe <sqlite> [holdout_days] [window_days]
use finder_core::bazaar::Bazaar;
use finder_core::config::{MIN_MARGIN, PRESCREEN_SLACK};
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{PriceIndex, Reference};
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
        .expect("usage: prescreen_probe <sqlite> [holdout] [window]");
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
    let idx = PriceIndex::build(train, Bazaar::from_prices(HashMap::new(), now_ms), now_ms);
    eprintln!(
        "  test={}  MIN_MARGIN={}  PRESCREEN_SLACK={}",
        test.len(),
        *MIN_MARGIN,
        *PRESCREEN_SLACK
    );

    let mut ratios: Vec<f64> = Vec::new();
    let mut over = 0usize;
    let mut over_value = 0.0f64;
    let mut n = 0usize;
    // The live ceiling as a multiple of the cheap median.
    for slack in [1.0_f64, 1.14, 1.25, 1.4] {
        let ceil_mult = 1.3 * (1.0 - *MIN_MARGIN) * slack;
        let (mut blocked, mut blocked_val) = (0usize, 0.0f64);
        let mut total = 0usize;
        for (attrs, actual) in &test {
            let key = idx.final_key(attrs);
            let Some(qt) = idx.cheap_median(&key) else {
                continue;
            };
            if qt <= 0.0 {
                continue;
            }
            total += 1;
            if *actual > qt * ceil_mult {
                blocked += 1;
                blocked_val += *actual;
            }
            if slack == 1.0 {
                ratios.push(*actual / qt);
                n += 1;
                if *actual > qt * ceil_mult {
                    over += 1;
                    over_value += *actual;
                }
            }
        }
        println!(
            "  slack {slack:<5} ceiling {ceil_mult:.3}x cheap_median -> {blocked}/{total} realised sales sit ABOVE it ({:.1}%), {:.1}B coins",
            blocked as f64 / total.max(1) as f64 * 100.0,
            blocked_val / 1e9
        );
    }
    println!();
    println!("  realised price / cheap_median, {n} sales with a cheap median:");
    println!(
        "    p10={:.2}x  p25={:.2}x  p50={:.2}x  p75={:.2}x  p90={:.2}x  p99={:.2}x",
        q(&mut ratios, 0.10),
        q(&mut ratios, 0.25),
        q(&mut ratios, 0.50),
        q(&mut ratios, 0.75),
        q(&mut ratios, 0.90),
        q(&mut ratios, 0.99)
    );
    println!(
        "  at the LIVE ceiling: {over} sales ({:.1}%) worth {:.1}B realised above it",
        over as f64 / n.max(1) as f64 * 100.0,
        over_value / 1e9
    );
}
