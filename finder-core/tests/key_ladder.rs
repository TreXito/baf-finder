//! KEY_LADDER: a fragmented exact key falls back to progressively coarser keys.
//!
//! The 2026-08-15 measured motivation: `notpriceable` is 62% of HIGHMISS reject
//! events and 91% of missed profit on prod (~46.5B/era), all `basis=high` — and
//! 7 of the top 10 misses have ≥5 BASE sales in 21 days. The item is abundant;
//! the exact FEATURE COMBO is not. Price it off the coarser pool rather than
//! refuse. Dropping a feature can only under-price a valuable variant (the
//! coarser pool is the cheap majority), which is the same safe direction
//! `cheap_flip_rescue` already ships on.
//!
//! These tests pin: the cheapest features drop first, rungs deepen only as far
//! as they must, confidence degrades per dropped feature, OFF is a dead None,
//! and a `#`-less key never ladders.

use finder_core::bazaar::Bazaar;
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{PriceIndex, Reference};
use std::collections::HashMap;

const NOW_MS: i64 = 1_783_941_749_000;

fn sword(skin: Option<&str>, compact: Option<i64>) -> ItemAttributes {
    let skin_json = match skin {
        Some(s) => format!(r#""skin":"{s}""#),
        None => String::new(),
    };
    let ench_json = match compact {
        Some(l) => format!(r#""enchantments":{{"compact":{l}}}"#),
        None => String::new(),
    };
    let mut parts = vec![r#""id":"TEST_SWORD""#.to_string()];
    if !skin_json.is_empty() {
        parts.push(skin_json);
    }
    if !ench_json.is_empty() {
        parts.push(ench_json);
    }
    serde_json::from_str(&format!("{{{}}}", parts.join(","))).expect("attrs json")
}

/// Clean pool has 6 sellers at 100M; the exact skinned key has 2 sellers at
/// 140M — both below MIN_REFS, so the exact key cannot price.
fn refs_skin() -> Vec<Reference> {
    let mut out = Vec::new();
    for i in 0..6 {
        out.push(Reference {
            price: 100_000_000.0,
            sold_at: (NOW_MS / 1000 - i * 60) as f64,
            seller: format!("clean{i}"),
            tts_ms: None,
            attrs: sword(None, None),
        });
    }
    for i in 0..2 {
        out.push(Reference {
            price: 140_000_000.0,
            sold_at: (NOW_MS / 1000 - i * 60) as f64,
            seller: format!("skinned{i}"),
            tts_ms: None,
            attrs: sword(Some("DRAGON"), None),
        });
    }
    out
}

/// Exact combo (skin + compact) 2 refs; skin-only 4 refs (both thin); clean 6.
/// `skin:` always splits (and has no bazaar value), `ench:compact6` forks on
/// material cost (20M book vs a 100M item), so the exact key is
/// `TEST_SWORD#ench:compact6+skin:DRAGON`.
fn refs_two_feature() -> Vec<Reference> {
    let mut out = Vec::new();
    for i in 0..6 {
        out.push(Reference {
            price: 100_000_000.0,
            sold_at: (NOW_MS / 1000 - i * 60) as f64,
            seller: format!("clean{i}"),
            tts_ms: None,
            attrs: sword(None, None),
        });
    }
    for i in 0..4 {
        out.push(Reference {
            price: 120_000_000.0,
            sold_at: (NOW_MS / 1000 - i * 60) as f64,
            seller: format!("ench{i}"),
            tts_ms: None,
            attrs: sword(None, Some(6)),
        });
    }
    for i in 0..2 {
        out.push(Reference {
            price: 140_000_000.0,
            sold_at: (NOW_MS / 1000 - i * 60) as f64,
            seller: format!("combo{i}"),
            tts_ms: None,
            attrs: sword(Some("DRAGON"), Some(6)),
        });
    }
    out
}

fn idx(refs: Vec<Reference>) -> PriceIndex {
    PriceIndex::build(refs, Bazaar::from_prices(HashMap::new(), NOW_MS), NOW_MS)
}

/// Starred variant of [`sword`]: `{"id":"TEST_SWORD","upgradeLevel":N}` plus
/// the same optional skin/compact feature carriers, so pools bucket into the
/// starred keys (`TEST_SWORD*2#...`) and the star-dropping rungs get exercised.
fn sword_star(level: f64, compact: Option<i64>, skin: Option<&str>) -> ItemAttributes {
    let mut parts = vec![
        r#""id":"TEST_SWORD""#.to_string(),
        format!(r#""upgradeLevel":{level}"#),
    ];
    if let Some(l) = compact {
        parts.push(format!(r#""enchantments":{{"compact":{l}}}"#));
    }
    if let Some(s) = skin {
        parts.push(format!(r#""skin":"{s}""#));
    }
    serde_json::from_str(&format!("{{{}}}", parts.join(","))).expect("attrs json")
}

fn refs_starred() -> Vec<Reference> {
    let mut out = Vec::new();
    for i in 0..6 {
        out.push(Reference {
            price: 100_000_000.0,
            sold_at: (NOW_MS / 1000 - i * 60) as f64,
            seller: format!("clean{i}"),
            tts_ms: None,
            attrs: sword(None, None),
        });
    }
    // The STARRED variant pool: 2 sellers at 105M — thin, so the exact key
    // cannot price, and 5% above the family pool.
    for i in 0..2 {
        out.push(Reference {
            price: 105_000_000.0,
            sold_at: (NOW_MS / 1000 - i * 60) as f64,
            seller: format!("starred{i}"),
            tts_ms: None,
            attrs: sword_star(1.0, None, None),
        });
    }
    out
}

#[test]
fn exact_key_thin_but_the_coarser_pool_prices_the_item() {
    let idx = idx(refs_skin());
    let a = sword(Some("DRAGON"), None);
    let key = idx.final_key(&a);
    assert_eq!(key, "TEST_SWORD#skin:DRAGON");
    // Below MIN_REFS at the exact key: hard notpriceable, as before.
    assert!(idx.price_for_key(&a, &key).is_none());
    let (stats, depth) = idx
        .ladder_price_inner(&a, &key, true, 0.85)
        .expect("coarser pool must price the fragmented key");
    assert_eq!(depth, 1, "one cosmetic feature dropped");
    assert!(
        (stats.target - 100_000_000.0).abs() < 1.0,
        "priced off the clean pool median, got {}",
        stats.target
    );
    assert!(stats.samples >= 5, "rung keeps its own measured samples");
}

#[test]
fn confidence_is_degraded_per_dropped_feature() {
    let idx = idx(refs_skin());
    let a = sword(Some("DRAGON"), None);
    let key = idx.final_key(&a);
    let coarser = idx
        .price_for_key(&a, "TEST_SWORD")
        .expect("clean pool prices directly");
    let (laddered, depth) = idx
        .ladder_price_inner(&a, &key, true, 0.85)
        .expect("ladder prices");
    assert_eq!(depth, 1);
    let expected = coarser.confidence * 0.85;
    assert!(
        (laddered.confidence - expected).abs() < 1e-9,
        "conf {} must equal coarser {:e} x 0.85^1",
        laddered.confidence,
        coarser.confidence
    );
}

#[test]
fn cheapest_feature_drops_first_and_only_as_deep_as_needed() {
    // Bazaar makes ench:compact6 a material (thus key-forking) feature.
    let mut prices = HashMap::new();
    prices.insert("ENCHANTMENT_COMPACT_6".to_string(), 20_000_000.0);
    let idx = PriceIndex::build(
        refs_two_feature(),
        Bazaar::from_prices(prices, NOW_MS),
        NOW_MS,
    );
    let a = sword(Some("DRAGON"), Some(6));
    let key = idx.final_key(&a);
    assert!(
        key.contains("ench:compact6") && key.contains("skin:DRAGON"),
        "exact key carries both forks, got {key}"
    );
    // Rung 1 drops the zero-value skin first → ench-only pool has 4 (<5).
    // Rung 2 drops both → clean pool has 6. It must land DEEP, not shallow.
    let (stats, depth) = idx
        .ladder_price_inner(&a, &key, true, 0.85)
        .expect("ladder prices at depth 2");
    assert_eq!(depth, 2, "ench-only pool is thin, so it deepens");
    assert!(
        (stats.target - 100_000_000.0).abs() < 1.0,
        "deep rung prices off the clean pool, got {}",
        stats.target
    );
    // Now make the ench-only pool deep enough: order must stop at depth 1.
    let mut refs = refs_two_feature();
    for i in 0..2 {
        refs.push(Reference {
            price: 120_000_000.0,
            sold_at: (NOW_MS / 1000 - (i as i64) * 60) as f64,
            seller: format!("ench_extra{i}"),
            tts_ms: None,
            attrs: sword(None, Some(6)),
        });
    }
    let mut prices = HashMap::new();
    prices.insert("ENCHANTMENT_COMPACT_6".to_string(), 20_000_000.0);
    let idx = PriceIndex::build(refs, Bazaar::from_prices(prices, NOW_MS), NOW_MS);
    let key = idx.final_key(&a);
    let (stats, depth) = idx
        .ladder_price_inner(&a, &key, true, 0.85)
        .expect("ladder prices at depth 1");
    assert_eq!(depth, 1, "skin dropped first, ench pool is deep enough");
    assert!(
        (stats.target - 120_000_000.0).abs() < 1.0,
        "shallow rung keeps the ench pool's median, got {}",
        stats.target
    );
}

#[test]
fn off_is_a_dead_none_and_keyless_keys_never_ladder() {
    let idx = idx(refs_skin());
    let a = sword(Some("DRAGON"), None);
    let key = idx.final_key(&a);
    assert!(idx.ladder_price_inner(&a, &key, false, 0.85).is_none());
    // A key already at its bare base has no features to drop.
    assert!(idx
        .ladder_price_inner(&sword(None, None), "TEST_SWORD", true, 0.85)
        .is_none());
    let no_key = idx.ladder_price_inner(&a, "PET_SKIN_SOMETHING", true, 0.85);
    assert!(
        no_key.is_none(),
        "no '#' → no ladder (pets/skins untouched)"
    );
    // The star rung obeys the same flag: OFF is a dead None even with the
    // family pool liquid, and a bare key has no dimension to drop.
    assert!(idx
        .ladder_price_inner(&sword_star(1.0, None, None), "TEST_SWORD*1", false, 0.85)
        .is_none());
}

#[test]
fn starred_variant_prices_off_the_liquid_family_pool() {
    let idx = idx(refs_starred());
    let a = sword_star(1.0, None, None);
    let key = idx.final_key(&a);
    assert_eq!(key, "TEST_SWORD*1", "star suffix sits between id and qty");
    // Thin at the exact key: 2 refs < MIN_REFS, as before.
    assert!(idx.price_for_key(&a, &key).is_none());
    let (stats, depth) = idx
        .ladder_price_inner(&a, &key, true, 0.85)
        .expect("star rung must reach the liquid family pool");
    assert_eq!(depth, 1, "the star suffix is one coarsening step");
    assert!(
        (stats.target - 100_000_000.0).abs() < 1.0,
        "priced off the unstarred family pool, got {}",
        stats.target
    );
    let bare = idx
        .price_for_key(&a, "TEST_SWORD")
        .expect("family pool prices directly");
    let expected = bare.confidence * 0.85;
    assert!(
        (stats.confidence - expected).abs() < 1e-9,
        "conf degrades by one rung like any feature: {} vs {:e}",
        stats.confidence,
        expected
    );
}

#[test]
fn stars_drop_first_then_cheapest_unknown_feature() {
    // ench:compact6 forks on material cost (20M book); the skin has no known
    // value (sort key 0, inserted after stars — ties keep stars first).
    // Order must be:  *2  →  skin  →  ench.
    let mut prices = HashMap::new();
    prices.insert("ENCHANTMENT_COMPACT_6".to_string(), 20_000_000.0);
    let mut refs = refs_starred();
    // The ench fork pool (6 refs at 120M) and the starless+both-features pool
    // (6 refs at 112M) — the latter is what rung 1 targets IF, and only if,
    // the star suffix drops before the skin.
    for i in 0..6 {
        refs.push(Reference {
            price: 120_000_000.0,
            sold_at: (NOW_MS / 1000 - i * 60) as f64,
            seller: format!("ench{i}"),
            tts_ms: None,
            attrs: sword(None, Some(6)),
        });
    }
    for i in 0..6 {
        refs.push(Reference {
            price: 112_000_000.0,
            sold_at: (NOW_MS / 1000 - i * 60) as f64,
            seller: format!("starless_combo{i}"),
            tts_ms: None,
            attrs: sword(Some("DRAGON"), Some(6)),
        });
    }
    let idx = PriceIndex::build(refs, Bazaar::from_prices(prices, NOW_MS), NOW_MS);
    let a = sword_star(2.0, Some(6), Some("DRAGON"));
    let key = idx.final_key(&a);
    assert!(
        key.starts_with("TEST_SWORD*2#")
            && key.contains("ench:compact6")
            && key.contains("skin:DRAGON"),
        "exact key carries stars and both forks, got {key}"
    );
    let (stats, depth) = idx
        .ladder_price_inner(&a, &key, true, 0.85)
        .expect("star-dropped rung prices");
    assert_eq!(
        depth, 1,
        "stars drop before any feature: rung 1 keeps both features starless"
    );
    assert!(
        (stats.target - 112_000_000.0).abs() < 1.0,
        "rung 1 must be the starless combo pool, got {}",
        stats.target
    );
}

#[test]
fn starred_and_family_thin_is_honest_notpriceable() {
    // Only the 2 starred refs exist anywhere: the family pool has nothing.
    let mut refs = Vec::new();
    for i in 0..2 {
        refs.push(Reference {
            price: 105_000_000.0,
            sold_at: (NOW_MS / 1000 - i * 60) as f64,
            seller: format!("starred{i}"),
            tts_ms: None,
            attrs: sword_star(1.0, None, None),
        });
    }
    let idx = idx(refs);
    let a = sword_star(1.0, None, None);
    let key = idx.final_key(&a);
    assert!(
        idx.ladder_price_inner(&a, &key, true, 0.85).is_none(),
        "no pool has MIN_REFS anywhere ⇒ honest notpriceable"
    );
}

#[test]
fn every_rung_thin_means_genuinely_thin_history() {
    // Nothing but the 2 skinned refs at any pool.
    let mut refs = Vec::new();
    for i in 0..2 {
        refs.push(Reference {
            price: 140_000_000.0,
            sold_at: (NOW_MS / 1000 - i * 60) as f64,
            seller: format!("skinned{i}"),
            tts_ms: None,
            attrs: sword(Some("DRAGON"), None),
        });
    }
    let idx = idx(refs);
    let a = sword(Some("DRAGON"), None);
    let key = idx.final_key(&a);
    assert!(
        idx.ladder_price_inner(&a, &key, true, 0.85).is_none(),
        "no pool has MIN_REFS anywhere ⇒ honest notpriceable"
    );
}
