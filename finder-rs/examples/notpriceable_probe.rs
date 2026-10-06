//! How much of the catalogue can the modifier model not price, and why?
//!
//! `eval_median_flip` falls back to `ModifierModel::estimate_for` whenever a key
//! has no direct stats, and rejects `notpriceable` when that returns None too.
//! On prod that reject is 1081 of 1732 HIGHMISS lines, every one `basis=high`,
//! i.e. `high_for_base` HAD data but the model still refused.
//!
//! `ModifierModel::rebuild` builds a base model only from `combos.get("")`, the
//! sales whose significant-feature set is EMPTY, and skips the base key entirely
//! when fewer than MIN_REFS of those exist. So an item that never trades bare
//! (every Hyperion has gems, every Wither helmet has enchants) gets no model at
//! all no matter how many hundreds of sales it has.
//!
//! This measures that: per base key, total sales vs bare sales, and the share of
//! sales and of coin volume trapped behind the bare-sample floor.
//!
//! usage: notpriceable_probe <sqlite> [days]
use finder_core::bazaar::Bazaar;
use finder_core::config::MIN_REFS;
use finder_core::modifier_model::ModifierModel;
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{base_key, PriceIndex, Reference};
use std::collections::HashMap;

struct BaseAgg {
    total: usize,
    bare: usize,
    value: f64,
    prices: Vec<f64>,
    sample: Option<ItemAttributes>,
}

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
        .expect("usage: notpriceable_probe <sqlite> [days]");
    let days: i64 = std::env::args()
        .nth(2)
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
    eprintln!("  refs={}  days={days}  MIN_REFS={}", refs.len(), *MIN_REFS);

    let now_ms = max_sold * 1000;
    let idx = PriceIndex::build(
        refs.clone(),
        Bazaar::from_prices(HashMap::new(), now_ms),
        now_ms,
    );
    let model = ModifierModel::rebuild(&refs, &idx, now_ms);
    eprintln!("  models built={}", model.model_count());

    let mut agg: HashMap<String, BaseAgg> = HashMap::new();
    for r in &refs {
        let bk = base_key(&r.attrs);
        let bare = idx.sig_features(&r.attrs).is_empty();
        let e = agg.entry(bk).or_insert_with(|| BaseAgg {
            total: 0,
            bare: 0,
            value: 0.0,
            prices: Vec::new(),
            sample: None,
        });
        e.total += 1;
        e.bare += usize::from(bare);
        e.value += r.price;
        e.prices.push(r.price);
        if e.sample.is_none() || !bare {
            // prefer a featured example, that is the one that gets rejected
            e.sample = Some(r.attrs.clone());
        }
    }

    let (mut n_keys, mut n_blind) = (0usize, 0usize);
    let (mut sales_all, mut sales_blind) = (0usize, 0usize);
    let (mut val_all, mut val_blind) = (0.0f64, 0.0f64);
    let mut blind: Vec<(String, usize, usize, f64, f64)> = Vec::new();

    for (bk, a) in &agg {
        n_keys += 1;
        sales_all += a.total;
        val_all += a.value;
        // exactly the condition in ModifierModel::rebuild
        if a.bare < *MIN_REFS {
            n_blind += 1;
            sales_blind += a.total;
            val_blind += a.value;
            let mut p = a.prices.clone();
            blind.push((bk.clone(), a.total, a.bare, median(&mut p), a.value));
        }
    }

    println!();
    println!("=== bare-sample floor (models.get(\"\") < MIN_REFS => NO model at all) ===");
    println!(
        "  base keys      : {n_blind}/{n_keys} blind ({:.1}%)",
        n_blind as f64 / n_keys as f64 * 100.0
    );
    println!(
        "  sales          : {sales_blind}/{sales_all} ({:.1}%)",
        sales_blind as f64 / sales_all as f64 * 100.0
    );
    println!(
        "  coin volume    : {:.1}B/{:.1}B ({:.1}%)",
        val_blind / 1e9,
        val_all / 1e9,
        val_blind / val_all * 100.0
    );

    // Confirm the model really refuses these, using the production entry point.
    let (mut checked, mut refused) = (0usize, 0usize);
    let mut rescued: Vec<(String, usize, f64, f64)> = Vec::new();
    let mut rescued_value = 0.0f64;
    for (bk, a) in &agg {
        if a.bare >= *MIN_REFS {
            continue;
        }
        let Some(s) = &a.sample else { continue };
        checked += 1;
        match model.estimate_for(s, &idx) {
            None => refused += 1,
            Some(e) => {
                rescued_value += a.value;
                let mut p = a.prices.clone();
                rescued.push((bk.clone(), a.total, median(&mut p), e.target));
            }
        }
    }
    println!("  estimate_for   : {refused}/{checked} of those really return None");
    println!(
        "  rescued        : {}/{checked} keys, {:.1}B of the {:.1}B blind volume ({:.1}%)",
        checked - refused,
        rescued_value / 1e9,
        val_blind / 1e9,
        rescued_value / val_blind * 100.0
    );

    rescued.sort_by(|a, b| b.1.cmp(&a.1));
    if !rescued.is_empty() {
        println!();
        println!("=== top 15 rescued keys by sales (est is for one featured example) ===");
        println!(
            "  {:<44}{:>7}{:>12}{:>12}",
            "base_key", "sales", "median", "est"
        );
        for (bk, total, med, est) in rescued.iter().take(15) {
            let short: String = bk.chars().take(42).collect();
            println!(
                "  {short:<44}{total:>7}{:>11.1}M{:>11.1}M",
                med / 1e6,
                est / 1e6
            );
        }
    }

    blind.sort_by(|a, b| b.4.partial_cmp(&a.4).unwrap());
    println!();
    println!("=== top 25 unmodellable base keys by coin volume ===");
    println!(
        "  {:<44}{:>7}{:>7}{:>12}{:>12}",
        "base_key", "sales", "bare", "median", "volume"
    );
    for (bk, total, bare, med, value) in blind.iter().take(25) {
        let short: String = bk.chars().take(42).collect();
        println!(
            "  {short:<44}{total:>7}{bare:>7}{:>11.1}M{:>11.1}M",
            med / 1e6,
            value / 1e6
        );
    }
}
