//! How much value does a pet level band hide?
//!
//! `pet_level_band` collapses an over-level pet (Golden/Jade/Rose Dragon) into
//! just three buckets, and the bottom one is enormous:
//!
//! ```ignore
//! if lvl >= 200 { "max" } else if lvl >= 150 { "l150" } else { "l100" }
//! ```
//!
//! So levels **1 through 149** share one price key and therefore one median. A
//! Lvl 1 egg and a Lvl 149 dragon are priced identically. Prod shows exactly
//! that: `[Lvl 50] Golden Dragon Egg`, `[Lvl 111]`, `[Lvl 115]` and `[Lvl 145]`
//! all keyed `PET:GOLDEN_DRAGON:LEGENDARY:l100` and all estimated at 855.4M.
//!
//! This groups real sales by their TRUE level and prints what each decile
//! actually fetched, which is the same measurement that proved the PULSE_RING
//! thunder_charge ladder.
//!
//! usage: pet_band_probe <sqlite> [pet_type] [days]
use finder_core::nbt::ItemAttributes;
use finder_core::pet_levels::{pet_level, pet_level_band};
use std::collections::BTreeMap;

fn median(xs: &mut Vec<f64>) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

fn main() {
    let db = std::env::args()
        .nth(1)
        .expect("usage: pet_band_probe <sqlite> [pet_type] [days]");
    let want = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "GOLDEN_DRAGON".into());
    let days: i64 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(14);

    let conn = rusqlite::Connection::open_with_flags(
        db.as_str(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open db");
    let max_sold: i64 = conn
        .query_row("SELECT MAX(sold_at) FROM sold", [], |r| r.get(0))
        .unwrap();
    let cutoff = max_sold - days * 86_400;

    let mut stmt = conn
        .prepare(
            "SELECT price, attrs FROM sold \
             WHERE attrs IS NOT NULL AND attrs != '' AND price > 0 AND sold_at >= ?1",
        )
        .unwrap();
    let rows = stmt
        .query_map([cutoff], |r| {
            Ok((r.get::<_, f64>(0)?, r.get::<_, String>(1)?))
        })
        .unwrap();

    // band -> level bucket -> prices
    let mut by_band: BTreeMap<String, BTreeMap<i64, Vec<f64>>> = BTreeMap::new();
    let mut band_all: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut n = 0usize;
    for row in rows.flatten() {
        let (price, attrs_json) = row;
        let Ok(attrs) = serde_json::from_str::<ItemAttributes>(&attrs_json) else {
            continue;
        };
        let Some(pet) = &attrs.pet else { continue };
        if pet.pet_type != want {
            continue;
        }
        n += 1;
        let lvl = pet_level(&pet.pet_type, &pet.tier, pet.exp);
        let band = pet_level_band(&pet.pet_type, &pet.tier, pet.exp);
        // 10-level buckets so the shape inside a band is visible
        let bucket = (lvl / 10) * 10;
        by_band
            .entry(band.clone())
            .or_default()
            .entry(bucket)
            .or_default()
            .push(price);
        band_all.entry(band).or_default().push(price);
    }

    println!();
    println!("=== {want}: {n} sales in {days}d ===");
    for (band, buckets) in &by_band {
        let mut all = band_all.get(band).cloned().unwrap_or_default();
        let bn = all.len();
        let bmed = median(&mut all);
        println!();
        println!("  BAND {band}  n={bn}  pooled median = {:.1}M   <-- every item below is priced off THIS", bmed / 1e6);
        println!(
            "    {:<14}{:>7}{:>14}{:>12}",
            "true level", "n", "median", "vs pooled"
        );
        for (bucket, prices) in buckets {
            let mut p = prices.clone();
            let m = median(&mut p);
            if p.len() < 3 {
                continue;
            }
            let ratio = if bmed > 0.0 { m / bmed } else { 0.0 };
            let flag = if ratio < 0.6 || ratio > 1.6 {
                "  <== MISPRICED"
            } else {
                ""
            };
            println!(
                "    lvl {:<10}{:>7}{:>13.1}M{:>11.2}x{flag}",
                format!("{}-{}", bucket, bucket + 9),
                p.len(),
                m / 1e6,
                ratio
            );
        }
    }
}
