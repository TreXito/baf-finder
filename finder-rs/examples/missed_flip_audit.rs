//! Were the flips the margin gate rejected actually good?
//!
//! `HIGHMISS` records the ask, our estimate and the price key for every flip a
//! gate killed. The store then says what that key ACTUALLY fetched afterwards.
//! If the realised median sits well above the ask, the gate cost us a real flip.
//! If it sits at or below, the gate was right and the estimate was optimistic.
//!
//! Only `basis=stats` and `basis=model` rows are audited, because `cheap` and
//! `high` are prescreen CEILINGS (1.3x a cheap median, 1.5x a base high) rather
//! than valuations, so a "miss" measured against them overstates the loss.
//!
//! The realised side is net of the 1.12% AH fee measured on our own round trips,
//! and requires 3+ sales of the key so a single outlier cannot manufacture a
//! winner. Keys are recomputed with `final_key`, the same function the log used,
//! so the join is exact rather than by item name.
//!
//! usage: missed_flip_audit <sqlite> <prod.log> [reason] [days]
use finder_core::bazaar::Bazaar;
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{PriceIndex, Reference};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};

fn median(xs: &mut Vec<f64>) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

fn field<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let pat = format!("{name}=");
    let i = s.find(&pat)? + pat.len();
    let rest = &s[i..];
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(&stripped[..end])
    } else {
        let end = rest.find(' ').unwrap_or(rest.len());
        Some(&rest[..end])
    }
}

fn main() {
    let db = std::env::args()
        .nth(1)
        .expect("usage: missed_flip_audit <sqlite> <log> [reason] [days]");
    let log = std::env::args().nth(2).expect("log path");
    let want_reason = std::env::args().nth(3).unwrap_or_else(|| "margin".into());
    let days: i64 = std::env::args()
        .nth(4)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);

    let conn = rusqlite::Connection::open_with_flags(
        db.as_str(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open db");
    let max_sold: i64 = conn
        .query_row("SELECT MAX(sold_at) FROM sold", [], |r| r.get(0))
        .unwrap();
    let start = max_sold - days * 86_400;

    let mut stmt = conn
        .prepare(
            "SELECT price, sold_at, seller, attrs, tts_ms FROM sold \
             WHERE attrs IS NOT NULL AND attrs != '' AND price > 0 AND sold_at >= ?1",
        )
        .unwrap();
    let mut refs: Vec<Reference> = Vec::new();
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
        refs.push(Reference {
            price,
            sold_at: sold_at as f64 * 1000.0,
            seller,
            tts_ms,
            attrs,
        });
    }
    let now_ms = max_sold * 1000;
    let idx = PriceIndex::build(
        refs.clone(),
        Bazaar::from_prices(HashMap::new(), now_ms),
        now_ms,
    );

    // final_key -> realised prices, and how long those sales took to clear.
    // TTS matters as much as price here: a flip that only pays on a 30h hold is
    // not the same product as one that clears in an hour, and 44% of our
    // inventory has historically never sold at all.
    let mut realised: HashMap<String, Vec<f64>> = HashMap::new();
    let mut tts_by_key: HashMap<String, Vec<f64>> = HashMap::new();
    for r in &refs {
        let k = idx.final_key(&r.attrs);
        realised.entry(k.clone()).or_default().push(r.price);
        if let Some(t) = r.tts_ms {
            if t > 0.0 {
                tts_by_key.entry(k).or_default().push(t);
            }
        }
    }
    eprintln!(
        "  {} sales over {days}d, {} distinct keys",
        refs.len(),
        realised.len()
    );

    let f = std::fs::File::open(&log).expect("open log");
    let (mut wins, mut losses, mut unknown) = (0usize, 0usize, 0usize);
    let (mut win_profit, mut loss_size) = (0.0f64, 0.0f64);
    let mut details: Vec<(f64, String, f64, f64, f64)> = Vec::new();
    let mut seen = 0usize;
    // (predicted margin, ask, key) for the band analysis below.
    let mut audited: Vec<(f64, f64, String)> = Vec::new();

    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if !line.contains("HIGHMISS") {
            continue;
        }
        let Some(reason) = field(&line, "reason") else {
            continue;
        };
        if reason != want_reason {
            continue;
        }
        let Some(basis) = field(&line, "basis") else {
            continue;
        };
        if basis != "stats" && basis != "model" {
            continue;
        }
        let Some(key) = field(&line, "key") else {
            continue;
        };
        let ask: f64 = field(&line, "price")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0);
        let est: f64 = field(&line, "est")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0);
        if ask <= 0.0 {
            continue;
        }
        seen += 1;
        if est > 0.0 {
            audited.push(((est - ask) / est, ask, key.to_string()));
        }
        let Some(prices) = realised.get(key) else {
            unknown += 1;
            continue;
        };
        if prices.len() < 3 {
            unknown += 1;
            continue;
        }
        let mut p = prices.clone();
        let med = median(&mut p);
        let net = med * 0.9888; // 1.12% AH fee, measured
        if net > ask * 1.05 {
            wins += 1;
            win_profit += net - ask;
            details.push((net - ask, key.to_string(), ask, med, est));
        } else {
            losses += 1;
            loss_size += ask - net;
        }
    }

    println!();
    println!("=== {want_reason} misses with a REAL valuation: {seen} ===");
    println!("  would have PROFITED : {wins}");
    println!("  would have LOST     : {losses}");
    println!("  no evidence         : {unknown}");
    if wins + losses > 0 {
        println!(
            "  hit rate            : {:.1}%",
            wins as f64 / (wins + losses) as f64 * 100.0
        );
        println!(
            "  net if we took them all: {:+.2}B   (gross win {:.2}B, gross loss {:.2}B)",
            (win_profit - loss_size) / 1e9,
            win_profit / 1e9,
            loss_size / 1e9
        );
    }
    // Where does EV actually turn negative? Bucketing by the margin we PREDICTED
    // is the only way to pick a threshold from evidence instead of taste. Each
    // row is "if MIN_MARGIN let this band through, what would it have earned".
    println!();
    println!("=== EV by predicted margin band (realised, net of the 1.12% fee) ===");
    println!(
        "  {:<14}{:>6}{:>9}{:>12}{:>12}{:>11}",
        "margin band", "n", "hit%", "net", "avg/flip", "med TTS"
    );
    let bands = [
        (0.0, 0.03),
        (0.03, 0.06),
        (0.06, 0.09),
        (0.09, 0.12),
        (0.12, 1.0),
    ];
    for (lo, hi) in bands {
        let (mut n, mut w) = (0usize, 0usize);
        let mut net_total = 0.0f64;
        let mut ttss: Vec<f64> = Vec::new();
        for (margin, ask, key) in &audited {
            if *margin < lo || *margin >= hi {
                continue;
            }
            let Some(prices) = realised.get(key) else {
                continue;
            };
            if prices.len() < 3 {
                continue;
            }
            let mut p = prices.clone();
            let net = median(&mut p) * 0.9888;
            n += 1;
            if net > *ask {
                w += 1;
            }
            net_total += net - *ask;
            if let Some(t) = tts_by_key.get(key) {
                let mut tv = t.clone();
                ttss.push(median(&mut tv));
            }
        }
        if n == 0 {
            continue;
        }
        let med_tts = if ttss.is_empty() {
            f64::NAN
        } else {
            median(&mut ttss) / 3_600_000.0
        };
        println!(
            "  {:<14}{n:>6}{:>8.0}%{:>11.2}B{:>11.1}M{:>10.1}h",
            format!("{:.0}-{:.0}%", lo * 100.0, hi * 100.0),
            w as f64 / n as f64 * 100.0,
            net_total / 1e9,
            net_total / n as f64 / 1e6,
            med_tts
        );
    }

    details.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    println!();
    println!("  biggest missed winners:");
    for (p, k, ask, med, est) in details.iter().take(10) {
        let short: String = k.chars().take(44).collect();
        println!(
            "    {short:<46} ask={:>8.1}M realised={:>8.1}M est={:>8.1}M  +{:.1}M",
            ask / 1e6,
            med / 1e6,
            est / 1e6,
            p / 1e6
        );
    }
}
