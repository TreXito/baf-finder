//! Partial-scroll blades must be flippable, and priced off their OWN scroll tier.
//! The Rust mirror of TS `npm run test:blade` (baf-flip-finder/src/partialScrollBlade.test.ts).
//! Keep the two in step.
//!
//! A guard (`is_incomplete_meta`) used to reject any Hyperion/Astraea/Scylla/
//! Valkyrie carrying SOME but not all of its three ability scrolls as "scam bait".
//! Prod sold history shows they are a liquid, consistently-priced tier, so it was
//! removed 2026-07-15 (s21). No golden fixture contains a partial-scroll blade, so
//! all 16 suites pass with or without the guard: this test is the only thing that
//! catches TS/Rust drift here.
//!
//! It pins BOTH halves of the decision:
//!   1. a partial-scroll blade is no longer screened out, and
//!   2. removing the guard did NOT make it get priced off full-scroll sales —
//!      that safety comes from the key/dominance layers, not the guard.

use finder_core::bazaar::Bazaar;
use finder_core::modifier_model::ModifierModel;
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{PriceIndex, Reference};
use finder_core::sniper::{screen_auction, ActiveAuction, DecodedAuction};
use std::collections::HashMap;

const NOW_MS: i64 = 1_783_941_749_000;

fn blade(scrolls: &[&str]) -> ItemAttributes {
    serde_json::from_value(serde_json::json!({
        "id": "HYPERION",
        "scrolls": scrolls,
        "upgradeLevel": 5,
    }))
    .expect("blade attrs")
}

const ALL3: [&str; 3] = [
    "IMPLOSION_SCROLL",
    "WITHER_SHIELD_SCROLL",
    "SHADOW_WARP_SCROLL",
];
const ONE: [&str; 1] = ["IMPLOSION_SCROLL"];

/// Mirrors the TS fixture: clean ~590M, 1-scroll ~850M, full 3-scroll ~1.29B,
/// with enough distinct sellers to survive seller-dedup (D3).
fn refs() -> Vec<Reference> {
    let mut out = Vec::new();
    for (scrolls, price, tag) in [
        (&[][..], 590_000_000.0, "clean"),
        (&ONE[..], 850_000_000.0, "one"),
        (&ALL3[..], 1_290_000_000.0, "full"),
    ] {
        for i in 0..12 {
            out.push(Reference {
                price,
                sold_at: (NOW_MS / 1000 - i * 60) as f64,
                seller: format!("{tag}{i}"),
                tts_ms: None,
                attrs: blade(scrolls),
            });
        }
    }
    out
}

fn setup() -> (PriceIndex, ModifierModel) {
    let r = refs();
    // Cold bazaar, matching the TS test's module-level (unfetched) bazaar.
    let idx = PriceIndex::build(
        r.clone(),
        Bazaar::from_prices(HashMap::new(), NOW_MS),
        NOW_MS,
    );
    let model = ModifierModel::rebuild(&r, &idx, NOW_MS);
    (idx, model)
}

fn decoded(idx: &PriceIndex, attrs: ItemAttributes, price: f64) -> DecodedAuction {
    let key = idx.final_key(&attrs);
    DecodedAuction {
        a: ActiveAuction {
            uuid: "test-uuid-1".into(),
            starting_bid: price,
            auctioneer: Some("someseller".into()),
            item_name: "Hyperion".into(),
        },
        attrs,
        key,
    }
}

#[test]
fn partial_scroll_blade_reaches_evaluation() {
    // The guard is gone: a cheap 1-scroll blade must not be screened out.
    let (idx, model) = setup();
    let v = screen_auction(&decoded(&idx, blade(&ONE), 1_500_000.0), &idx, &model);
    assert_ne!(
        v, 'r',
        "a cheap partial-scroll Hyperion must NOT be screened out"
    );
}

#[test]
fn full_scroll_blade_reaches_evaluation() {
    // Sanity: this was never blocked and still isn't.
    let (idx, model) = setup();
    let v = screen_auction(&decoded(&idx, blade(&ALL3), 1_500_000.0), &idx, &model);
    assert_ne!(v, 'r', "a cheap full-scroll Hyperion must reach evaluation");
}

#[test]
fn each_scroll_tier_keys_separately() {
    // The load-bearing safety: this, not the guard, is what stops a 1-scroll blade
    // being valued off 3-scroll sales. If this fails, removing the guard DID become
    // unsafe.
    let (idx, _) = setup();
    let k_one = idx.final_key(&blade(&ONE));
    let k_all = idx.final_key(&blade(&ALL3));
    let k_clean = idx.final_key(&blade(&[]));
    assert_ne!(
        k_one, k_all,
        "1-scroll and 3-scroll must not share a finalKey"
    );
    assert_ne!(
        k_one, k_clean,
        "1-scroll and clean must not share a finalKey"
    );
    assert_ne!(
        k_all, k_clean,
        "3-scroll and clean must not share a finalKey"
    );
    // Same keys the TS test prints, so a drift shows up as a readable diff.
    assert_eq!(k_clean, "HYPERION*5");
    assert!(k_one.starts_with("HYPERION*5#scroll:"), "got {k_one}");
}
