//! Not a correctness test: a timing probe for the per-auction key path.
//! `cargo test -p finder-core --test keypath_bench -- --nocapture`
use finder_core::nbt::decode_item_bytes;
use finder_core::price_index::{base_key, candidate_features};
use serde_json::Value;
use std::time::Instant;

fn corpus() -> Vec<finder_core::nbt::ItemAttributes> {
    let raw = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../finder-rs/fixture-page0.json"
    ))
    .expect("fixture-page0.json");
    let v: Value = serde_json::from_slice(&raw).unwrap();
    v["auctions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["item_bytes"].as_str())
        .filter_map(decode_item_bytes)
        .collect()
}

#[test]
fn keypath_timing() {
    let items = corpus();
    assert!(!items.is_empty(), "decoded nothing from fixture");
    let books = items.iter().filter(|a| a.id == "ENCHANTED_BOOK").count();
    println!("corpus: {} items ({} enchanted books)", items.len(), books);

    // warm
    for a in &items {
        std::hint::black_box(base_key(a));
        std::hint::black_box(candidate_features(a));
    }

    const REPS: u32 = 50;
    let t = Instant::now();
    for _ in 0..REPS {
        for a in &items {
            std::hint::black_box(base_key(a));
        }
    }
    let bk = t.elapsed().as_secs_f64() * 1e9 / (REPS as f64 * items.len() as f64);

    let t = Instant::now();
    for _ in 0..REPS {
        for a in &items {
            std::hint::black_box(candidate_features(a));
        }
    }
    let cf = t.elapsed().as_secs_f64() * 1e9 / (REPS as f64 * items.len() as f64);

    println!("base_key           {:>8.0} ns/item", bk);
    println!("candidate_features {:>8.0} ns/item", cf);
    println!("combined           {:>8.0} ns/item", bk + cf);
    println!(
        "=> per 1000-auction page: {:.2} ms",
        (bk + cf) * 1000.0 / 1e6
    );

    // Books only, to isolate the allocating sort comparator.
    let only_books: Vec<_> = items.iter().filter(|a| a.id == "ENCHANTED_BOOK").collect();
    if !only_books.is_empty() {
        let t = Instant::now();
        for _ in 0..REPS {
            for a in &only_books {
                std::hint::black_box(base_key(a));
            }
        }
        let b = t.elapsed().as_secs_f64() * 1e9 / (REPS as f64 * only_books.len() as f64);
        println!("base_key (books only) {:>8.0} ns/item", b);
    }
}
