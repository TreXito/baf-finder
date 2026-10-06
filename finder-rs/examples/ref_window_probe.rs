//! What would a WIDER sales-history window actually buy us?
//!
//! `REF_MAX_RAM` is not the constraint. `load_references` selects
//! `sold WHERE sold_at >= now - REF_MAX_AGE_DAYS*86400` (7 days) which yields
//! ~1.22M refs, and `cap_ram` only sheds down to `REF_MAX_RAM` = 1.8M — so the
//! RAM cap never fires and raising it changes nothing. The binding limit is the
//! **7-day age window**.
//!
//! This measures, per window, the thing that actually matters: how many more
//! distinct `base_key`s reach `MIN_REFS` and therefore become **priceable**.
//! Extra refs on a key that is already priceable are near-worthless because
//! `REF_CAP` (80) truncates per-key history anyway.
//!
//! Read-only. Run it on the box; it holds the refs in memory, so watch RSS.
//!
//! usage: ref_window_probe <auctions.sqlite> [days,days,...]

use finder_core::config::{MIN_REFS, REF_CAP};
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::base_key;
use std::collections::HashMap;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let db = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("/root/finder/data/auctions.sqlite");
    let windows: Vec<i64> = args
        .get(2)
        .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![7, 14, 21, 30, 60]);

    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open db read-only");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    println!("MIN_REFS={} REF_CAP={}\n", *MIN_REFS, *REF_CAP);
    println!(
        "{:>6} {:>12} {:>10} {:>12} {:>12} {:>10}",
        "days", "refs", "refs/day", "keys>=MIN", "new keys", "est RAM"
    );

    let mut base_keys: Option<usize> = None;
    let max = *windows.iter().max().unwrap();

    // Load once at the widest window, then filter down per window: one pass over
    // sqlite instead of N, and the JSON decode (the expensive part) happens once.
    let cutoff_max = now - max * 86400;
    let mut stmt = conn
        .prepare("SELECT sold_at, attrs FROM sold WHERE sold_at >= ?1")
        .expect("prepare");
    let mut all: Vec<(i64, String)> = Vec::new();
    let rows = stmt
        .query_map([cutoff_max], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .expect("query");
    for r in rows.flatten() {
        all.push(r);
    }
    eprintln!(
        "loaded {} raw sold rows for the widest window ({max}d)",
        all.len()
    );

    // Decode once; keep (sold_at, base_key) which is all the counting needs.
    let mut decoded: Vec<(i64, String)> = Vec::with_capacity(all.len());
    let mut bad = 0usize;
    for (sold_at, attrs_s) in &all {
        match serde_json::from_str::<ItemAttributes>(attrs_s) {
            Ok(a) => decoded.push((*sold_at, base_key(&a))),
            Err(_) => bad += 1,
        }
    }
    drop(all);
    eprintln!("decoded {} refs ({bad} undecodable)\n", decoded.len());

    for w in &windows {
        let cutoff = now - w * 86400;
        let mut counts: HashMap<&str, u32> = HashMap::new();
        let mut refs = 0usize;
        for (sold_at, key) in &decoded {
            if *sold_at >= cutoff {
                refs += 1;
                *counts.entry(key.as_str()).or_insert(0) += 1;
            }
        }
        let priceable = counts
            .values()
            .filter(|c| **c as usize >= *MIN_REFS)
            .count();
        let new = base_keys.map(|b| priceable as i64 - b as i64).unwrap_or(0);
        if base_keys.is_none() {
            base_keys = Some(priceable);
        }
        // Refs actually retained: REF_CAP per key bounds what the index keeps.
        let retained: usize = counts.values().map(|c| (*c as usize).min(*REF_CAP)).sum();
        println!(
            "{w:>6} {refs:>12} {:>10} {priceable:>12} {:>12} {:>9.1}GB",
            refs / *w as usize,
            if new > 0 {
                format!("+{new}")
            } else {
                "-".into()
            },
            // ~1.2KB/ref measured against the live process: 1.22M refs inside a
            // 2.20GB RSS that also holds the model, sqlite cache and the live BIN set.
            retained as f64 * 1200.0 / 1e9
        );
    }
    println!(
        "\n  'new keys' = distinct base_keys that cross MIN_REFS and become priceable\n  \
         est RAM counts REF_CAP-truncated refs only, which is what the index keeps"
    );
}
