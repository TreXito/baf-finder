//! Replays captured prod FLIP log lines through the real filter with the real
//! prod binmaster-filter.json, and reports pass/reject with the binding reason.
//!
//! The FUNNEL-FILTER counters cannot answer "which gate actually blocked this
//! flip", because `last_reason` is overwritten by whichever tier is evaluated
//! last. This replays each flip and reports the decision the finder truly makes.
//!
//! usage: cargo run -p finder-core --example replay_filter -- <filter.json> <flips.jsonl>

use finder_core::filter::{BinMasterFilter, Filter, FilterFlip};
use std::collections::BTreeMap;

fn main() {
    let mut args = std::env::args().skip(1);
    let fpath = args.next().expect("filter.json path");
    let lpath = args.next().expect("flips.jsonl path");

    let bm: BinMasterFilter =
        serde_json::from_str(&std::fs::read_to_string(&fpath).unwrap()).expect("parse filter");
    let filter = Filter::new(Some(bm));

    let mut pass_lo = 0usize; // sub-3M passes: the flips the new tiers unlock
    let mut pass_hi = 0usize;
    let mut rej_lo = 0usize;
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    let mut unlocked: Vec<(f64, String, f64)> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for line in std::fs::read_to_string(&lpath).unwrap().lines() {
        let start = match line.find('{') {
            Some(i) => i,
            None => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&line[start..]) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("msg").and_then(|m| m.as_str()) != Some("FLIP") {
            continue;
        }
        let profit = v.get("profit").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let key = v
            .get("key")
            .and_then(|x| x.as_str())
            .unwrap_or("?")
            .to_string();
        let price = v.get("price").and_then(|x| x.as_f64()).unwrap_or(0.0);
        if !seen.insert((key.clone(), price.to_bits(), profit.to_bits())) {
            continue;
        }
        let roi = v
            .get("roi")
            .and_then(|x| x.as_str())
            .and_then(|s| s.trim_end_matches('%').parse::<f64>().ok())
            .unwrap_or(0.0);
        let tts_h = v.get("fairTtsH").and_then(|x| x.as_f64());
        let flip = FilterFlip {
            attrs: serde_json::from_str(&format!(r#"{{"id":"{key}"}}"#)).unwrap(),
            profit,
            roi_pct: roi,
            confidence: v.get("confidence").and_then(|x| x.as_f64()).unwrap_or(0.0),
            volume_per_day: v.get("volPerDay").and_then(|x| x.as_f64()),
            fair_tts_ms: tts_h.map(|h| h * 3_600_000.0),
            tts_samples: v.get("ttsNFair").and_then(|x| x.as_i64()),
            sell_through: v.get("sellThrough").and_then(|x| x.as_f64()),
        };
        let d = filter.evaluate_flip(&flip);
        if d.pass {
            if profit < 3e6 {
                pass_lo += 1;
                unlocked.push((
                    profit,
                    v.get("item")
                        .and_then(|x| x.as_str())
                        .unwrap_or("?")
                        .to_string(),
                    roi,
                ));
            } else {
                pass_hi += 1;
            }
        } else if profit < 3e6 {
            rej_lo += 1;
            *reasons.entry(d.reason.unwrap_or_default()).or_default() += 1;
        }
    }

    println!("PASS >=3M (pre-existing): {pass_hi}");
    println!("PASS  <3M (NEW TIERS):    {pass_lo}");
    println!("REJECT <3M:               {rej_lo}");
    println!("\nsub-3M reject reasons (the BINDING gate):");
    let mut rs: Vec<_> = reasons.into_iter().collect();
    rs.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    for (r, c) in rs.iter().take(10) {
        println!("  {c:5}  {r}");
    }
    unlocked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    println!("\nunlocked sample:");
    for (p, item, roi) in unlocked.iter().take(8) {
        println!("  {:6.2}M  roi {:3.0}%  {}", p / 1e6, roi, item);
    }
}
