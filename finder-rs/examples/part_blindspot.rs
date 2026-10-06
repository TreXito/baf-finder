//! Do items that carry drill parts (or pet-held items) get systematically
//! UNDERPRICED by the median lane?
//!
//! The suspicion came from two identical "Magnetic Gemstone Drill LT-522"
//! purchases minutes apart: ours targeted 26.40M, COFL's targeted 37.35M.
//!
//! `part:X` is never a key component. It reaches value by one of three roads,
//! and every road that fails, fails DOWNWARD:
//!
//!   1. `feature_value("part:X")` resolves (X has its own base_value) AND clears
//!      max(FEATURE_MIN_VALUE, base * ATTR_MIN_SHARE) -> the part splits the key,
//!      the item gets its own pool. Correct.
//!   2. It resolves but sits BELOW that bar -> "minor". `adjusted_price` strips
//!      the FULL part value out of every comparable sale when building the pool,
//!      but `eval_median_flip` credits our own item only
//!      `min(minor * 0.5, target * 0.3)` back. Asymmetric by construction.
//!   3. It does not resolve at all (the part has no recent bare sales, so no
//!      base_value) -> `bazaar_significant` returns None, `minor_feature_value`
//!      skips it, the learned pass needs MIN_FEATURE_SAMPLES of that exact
//!      drill+part combo. Miss that and the part is worth EXACTLY ZERO and the
//!      drill is priced off a pool of bare drills.
//!
//! Then `resale = min(resale, live_lbin(key))` clamps to the cheapest live
//! listing sharing that key -- which, whenever road 2 or 3 was taken, includes
//! bare drills.
//!
//! This measures roads 2 and 3 against what the items ACTUALLY sold for.
//!
//! NOTE ON BIAS: the reference being scored is itself in the pool it is scored
//! against, which drags `target` toward `actual` and therefore UNDERSTATES any
//! error. Every number here is a floor, not an estimate.
//!
//! usage: part_blindspot <sqlite> [days] [name-substring]
use finder_core::bazaar::Bazaar;
use finder_core::config::{ATTR_MIN_SHARE, FEATURE_MIN_VALUE, MIN_REFS};
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{base_key, candidate_features, PriceIndex, Reference};
use std::collections::HashMap;

#[derive(Default)]
struct Bucket {
    n: usize,
    /// signed relative error of the resale we WOULD quote, vs the realised price
    err_now: Vec<f64>,
    /// same, but crediting the full minor value instead of half
    err_full: Vec<f64>,
    /// coins of realised value in this bucket
    value: f64,
    /// coins we would have left on the table (actual - resale_now), when positive
    shortfall: f64,
}

impl Bucket {
    fn push(&mut self, actual: f64, now: f64, full: f64) {
        self.n += 1;
        self.value += actual;
        self.err_now.push((now - actual) / actual);
        self.err_full.push((full - actual) / actual);
        if actual > now {
            self.shortfall += actual - now;
        }
    }
}

fn pct(v: &mut Vec<f64>, p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p) as usize]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let db = args
        .next()
        .expect("usage: part_blindspot <sqlite> [days] [name-substring]");
    let days: i64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(7);
    let filter = args.next().unwrap_or_default().to_lowercase();

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
            "SELECT price, sold_at, seller, attrs, tts_ms FROM sold \
             WHERE attrs IS NOT NULL AND attrs != '' AND price > 0 AND sold_at >= ?1",
        )
        .unwrap();
    let mut refs: Vec<Reference> = Vec::new();
    let rows = stmt
        .query_map([cutoff], |r| {
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
        refs.push(Reference {
            price,
            sold_at: sold_at as f64 * 1000.0,
            seller,
            tts_ms,
            attrs,
        });
    }
    eprintln!(
        "  refs={}  days={days}  FEATURE_MIN_VALUE={}  ATTR_MIN_SHARE={}",
        refs.len(),
        *FEATURE_MIN_VALUE,
        *ATTR_MIN_SHARE
    );

    let now_ms = max_sold * 1000;
    let idx = PriceIndex::build(
        refs.clone(),
        Bazaar::from_prices(HashMap::new(), now_ms),
        now_ms,
    );

    // Which part tokens exist at all, and can we even value them?
    let mut part_seen: HashMap<String, (usize, Option<f64>, usize)> = HashMap::new();
    for r in &refs {
        for f in candidate_features(&r.attrs) {
            if !f.starts_with("part:") && !f.starts_with("pethled:") {
                continue;
            }
            let bk = base_key(&r.attrs);
            let learned = idx.sig_features(&r.attrs).contains(&f);
            let e = part_seen
                .entry(f.clone())
                .or_insert((0, idx.feature_value(&f), 0));
            e.0 += 1;
            e.2 += usize::from(learned);
            let _ = bk;
        }
    }

    let mut b_split = Bucket::default(); // road 1: part splits the key
    let mut b_minor = Bucket::default(); // road 2: known but below the bar
    let mut b_blind = Bucket::default(); // road 3: unvalued and unlearned
    let mut b_none = Bucket::default(); // control: no part features at all

    for r in &refs {
        if !filter.is_empty() && !r.attrs.id.to_lowercase().contains(&filter) {
            continue;
        }
        let feats: Vec<String> = candidate_features(&r.attrs)
            .into_iter()
            .filter(|f| f.starts_with("part:") || f.starts_with("pethled:"))
            .collect();

        let fk = idx.final_key(&r.attrs);
        let Some(stats) = idx.price_for_key(&r.attrs, &fk) else {
            continue;
        };
        if stats.samples < *MIN_REFS as i64 {
            continue;
        }
        let minor = idx.minor_feature_value(&r.attrs);
        let now = stats.target + (minor * 0.5).min(stats.target * 0.3);
        let full = stats.target + minor;

        if feats.is_empty() {
            b_none.push(r.price, now, full);
            continue;
        }
        let sig = idx.sig_features(&r.attrs);
        let any_split = feats.iter().any(|f| sig.contains(f));
        let any_blind = feats
            .iter()
            .any(|f| idx.feature_value(f).is_none() && !sig.contains(f));

        if any_blind {
            b_blind.push(r.price, now, full);
        } else if any_split {
            b_split.push(r.price, now, full);
        } else {
            b_minor.push(r.price, now, full);
        }
    }

    println!();
    println!("=== part / held-item tokens in {days}d of sales ===");
    let mut ps: Vec<_> = part_seen.into_iter().collect();
    ps.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    println!(
        "  {:<44}{:>7}{:>14}{:>10}",
        "token", "sales", "own value", "learned"
    );
    for (tok, (n, val, learned)) in ps.iter().take(25) {
        let v = match val {
            Some(v) => format!("{:.2}M", v / 1e6),
            None => "UNVALUED".to_string(),
        };
        println!("  {tok:<44}{n:>7}{v:>14}{learned:>10}");
    }
    let unvalued = ps.iter().filter(|(_, (_, v, _))| v.is_none()).count();
    println!(
        "  {} of {} tokens have NO own base_value",
        unvalued,
        ps.len()
    );

    println!();
    println!("=== resale we would quote vs what the item actually sold for ===");
    println!("  (negative = we quote BELOW the realised price, i.e. we undersell)");
    println!();
    println!(
        "  {:<26}{:>7}{:>11}{:>11}{:>11}{:>13}",
        "road", "n", "p25", "p50 err", "p75", "shortfall"
    );
    for (name, b) in [
        ("1 part splits the key", &mut b_split),
        ("2 minor, half-credited", &mut b_minor),
        ("3 blind, zero-credited", &mut b_blind),
        ("control: no parts", &mut b_none),
    ] {
        if b.n == 0 {
            println!("  {name:<26}{:>7}", 0);
            continue;
        }
        let (p25, p50, p75) = (
            pct(&mut b.err_now, 0.25),
            pct(&mut b.err_now, 0.5),
            pct(&mut b.err_now, 0.75),
        );
        println!(
            "  {name:<26}{:>7}{:>10.1}%{:>10.1}%{:>10.1}%{:>11.2}B",
            b.n,
            p25 * 100.0,
            p50 * 100.0,
            p75 * 100.0,
            b.shortfall / 1e9
        );
    }

    println!();
    println!("=== what full credit instead of half would do (road 2 only) ===");
    if b_minor.n > 0 {
        let now = pct(&mut b_minor.err_now, 0.5) * 100.0;
        let full = pct(&mut b_minor.err_full, 0.5) * 100.0;
        println!("  median error, half credit : {now:>7.1}%");
        println!("  median error, full credit : {full:>7.1}%");
        println!(
            "  -> {}",
            if full.abs() < now.abs() {
                "full credit is CLOSER to realised value"
            } else {
                "half credit is closer; the asymmetry is doing real work, leave it"
            }
        );
    } else {
        println!("  no road-2 samples in this window");
    }

    println!();
    println!("  Bias note: each reference sits inside the pool it is scored against,");
    println!("  so every error above is a FLOOR. The real gap is wider.");
}
