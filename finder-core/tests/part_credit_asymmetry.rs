//! Drill parts can only ever move our resale DOWN, never up.
//!
//! Prompted by two identical "Magnetic Gemstone Drill LT-522" buys minutes
//! apart: our finder targeted 26.40M, COFL targeted 37.35M.
//!
//! `part:X` is not a key component. It reaches value on three roads:
//!
//!   1. it splits the key  -> own pool, priced correctly
//!   2. "minor" (value known, below max(FEATURE_MIN_VALUE, base*ATTR_MIN_SHARE))
//!      -> `adjusted_price` strips the FULL value from every comparable sale
//!      when building the pool, but the sniper credits our own item only
//!      `min(minor*0.5, target*0.3)` back
//!   3. unvalued (the part has no recent bare sales, so no `base_value`) and
//!      not learned -> contributes literally nothing, and the item is priced
//!      off a pool of BARE items
//!
//! Roads 2 and 3 are both one-directional losses. These tests pin the exact
//! arithmetic so a fix has something to move.
//!
//! The 0.5 and the 0.3 cap came in with the original TS behavioural clone
//! (`934c8f5`) and have never been justified against realised prices.

use finder_core::bazaar::Bazaar;
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{base_key, PriceIndex, Reference};
use std::collections::HashMap;

const DAY: f64 = 86_400_000.0;
const NOW: i64 = 1_800_000_000_000;

fn item(id: &str, part: Option<&str>) -> ItemAttributes {
    let parts: Vec<&str> = part.into_iter().collect();
    serde_json::from_value(serde_json::json!({ "id": id, "parts": parts }))
        .expect("ItemAttributes from json")
}

/// `seller` must be unique per sale: `compute_price_for` keeps only the CHEAPEST
/// sale per seller and then requires MIN_REFS survivors, so reusing a name
/// silently collapses the pool and the key prices as None.
fn sale(a: &ItemAttributes, price: f64, days_ago: f64, seller: &str) -> Reference {
    Reference {
        price,
        sold_at: NOW as f64 - days_ago * DAY,
        seller: seller.to_string(),
        tts_ms: None,
        attrs: a.clone(),
    }
}

/// n sales of `a` at `price`, distinct sellers, spread over recent days.
fn many(a: &ItemAttributes, price: f64, n: usize, tag: &str) -> Vec<Reference> {
    (0..n)
        .map(|i| sale(a, price, (i % 5) as f64 * 0.5, &format!("{tag}{i}")))
        .collect()
}

fn build(refs: Vec<Reference>) -> PriceIndex {
    PriceIndex::build(refs, Bazaar::from_prices(HashMap::new(), NOW), NOW)
}

/// The sniper's own resale arithmetic (`sniper.rs`, median lane).
fn resale_now(idx: &PriceIndex, a: &ItemAttributes) -> Option<f64> {
    let key = idx.final_key(a);
    let stats = idx.price_for_key(a, &key)?;
    let minor = idx.minor_feature_value(a);
    Some(stats.target + (minor * 0.5).min(stats.target * 0.3))
}

/// Road 2: the part IS valued, but below the significance bar.
///
/// Bare drill 10M, engine 1M, so the bar is max(500k, 10M*0.15) = 1.5M and the
/// engine misses it. A drill+engine really trades at 11M. We quote 10.5M.
#[test]
fn minor_part_is_stripped_in_full_but_credited_by_half() {
    let bare = item("TEST_DRILL", None);
    let parted = item("TEST_DRILL", Some("TEST_ENGINE"));
    let engine = item("TEST_ENGINE", None);

    let mut refs = many(&bare, 10_000_000.0, 12, "bare");
    refs.extend(many(&parted, 11_000_000.0, 12, "part"));
    refs.extend(many(&engine, 1_000_000.0, 12, "eng"));
    let idx = build(refs);

    // The engine has its own value, and it is below the bar, so it stays minor.
    assert_eq!(idx.feature_value("part:TEST_ENGINE"), Some(1_000_000.0));
    assert!(
        idx.sig_features(&parted).is_empty(),
        "engine must NOT split the key here, got {:?}",
        idx.sig_features(&parted)
    );
    assert_eq!(base_key(&parted), base_key(&bare), "same base key");
    assert_eq!(idx.final_key(&parted), idx.final_key(&bare), "same pool");
    assert_eq!(idx.minor_feature_value(&parted), 1_000_000.0);

    // Pool side: the 11M sales were normalised DOWN by the full 1M, so they sit
    // on top of the bare 10M sales and the pooled target is 10M.
    let stats = idx
        .price_for_key(&parted, &idx.final_key(&parted))
        .expect("stats");
    assert_eq!(stats.target, 10_000_000.0, "full strip on the way in");

    // Estimate side: only half the part comes back.
    let quoted = resale_now(&idx, &parted).expect("resale");
    assert_eq!(quoted, 10_500_000.0);

    // The market says 11M. We are 500k light, which is exactly the half we
    // refused to credit -- not a modelling error, an accounting one.
    assert_eq!(11_000_000.0 - quoted, 500_000.0);
}

/// Road 3: the part has no bare sales, so it has no value and no key of its own.
///
/// This is the brutal one. The parted item is pooled WITH bare items and the
/// part is worth zero, so we quote the bare price for a parted drill.
#[test]
fn unvalued_part_contributes_nothing_and_pools_with_bare_items() {
    let bare = item("TEST_DRILL", None);
    let parted = item("TEST_DRILL", Some("RARE_ENGINE"));

    // Note what is absent: no bare RARE_ENGINE sales anywhere in the index.
    let mut refs = many(&bare, 10_000_000.0, 12, "bare");
    // Only two parted sales, under MIN_FEATURE_SAMPLES, so the learned pass
    // cannot rescue it either.
    refs.push(sale(&parted, 18_000_000.0, 1.0, "rare_a"));
    refs.push(sale(&parted, 18_000_000.0, 2.0, "rare_b"));
    let idx = build(refs);

    assert_eq!(idx.feature_value("part:RARE_ENGINE"), None, "no own value");
    assert!(idx.sig_features(&parted).is_empty(), "not learned either");
    assert_eq!(
        idx.minor_feature_value(&parted),
        0.0,
        "unvalued parts are skipped entirely: bazaar_significant is None, not Some(false)"
    );
    assert_eq!(
        idx.final_key(&parted),
        idx.final_key(&bare),
        "pooled with bare"
    );

    // Two 18M sales against twelve 10M sales: the median is unmoved.
    let quoted = resale_now(&idx, &parted).expect("resale");
    assert_eq!(quoted, 10_000_000.0);

    // An item that trades at 18M is quoted at 10M. 8M, 44%, gone.
    assert!(
        18_000_000.0 - quoted == 8_000_000.0,
        "expected the full part value to vanish, quoted {quoted}"
    );
}

/// Road 1, the control: a part worth more than the bar splits the key and is
/// priced correctly. Whatever a fix does, it must not disturb this.
#[test]
fn significant_part_splits_the_key_and_prices_correctly() {
    let bare = item("TEST_DRILL", None);
    let parted = item("TEST_DRILL", Some("BIG_ENGINE"));
    let engine = item("BIG_ENGINE", None);

    let mut refs = many(&bare, 10_000_000.0, 12, "bare");
    refs.extend(many(&parted, 28_000_000.0, 12, "part"));
    refs.extend(many(&engine, 18_000_000.0, 12, "eng"));
    let idx = build(refs);

    // 18M clears max(500k, 10M*0.15), so it becomes a key component.
    assert_eq!(idx.feature_value("part:BIG_ENGINE"), Some(18_000_000.0));
    assert_eq!(
        idx.sig_features(&parted),
        vec!["part:BIG_ENGINE".to_string()]
    );
    assert_ne!(idx.final_key(&parted), idx.final_key(&bare), "own pool");

    // Own pool, own median, no credit arithmetic involved.
    assert_eq!(idx.minor_feature_value(&parted), 0.0);
    assert_eq!(resale_now(&idx, &parted), Some(28_000_000.0));
}
