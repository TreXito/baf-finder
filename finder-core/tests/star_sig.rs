//! STAR_SIG: a star level forks the key only when the sales say it moves the
//! price. Without the flag, `base_key` unconditionally emits `*N`, so a
//! 1-star Bouquet of Lies gets its own thin pool (measured 2026-08-15:
//! 13 samples, 0.8 vol/day, last sale 98h, push refused on guard `stale`)
//! while the unstarred family pool it actually sells with sits liquid next
//! door. With the flag, Pass 2b measures `bare|star:N` against the strict-
//! unstarred group at the Pass-2 bars (one-sided: a premium of at least
//! +ATTR_MIN_SHARE with at least MIN_FEATURE_SAMPLES sales), Pass 3 buckets
//! refs under the folded key, and every lookup agrees — volume, freshness and
//! sample counts aggregate structurally.
//!
//! These tests pin on ONE built index: an insignificant star folds (and the
//! OFF gate is byte-identical), a measured premium keeps `*N`, per-level
//! independence holds, and an item that never traded unstarred folds (no bar,
//! no proof — one unified pool instead of ten fragments). No env is flipped:
//! `final_key_with_star_sig` injects the gate, and the default `final_key`
//! path stays OFF for every other suite in the workspace.

use finder_core::bazaar::Bazaar;
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{PriceIndex, Reference};
use std::collections::HashMap;

const NOW_MS: i64 = 1_783_941_749_000;

fn sword_star(level: Option<f64>) -> ItemAttributes {
    let ul = match level {
        Some(l) => format!(r#","upgradeLevel":{l}"#),
        None => String::new(),
    };
    serde_json::from_str(&format!(r#"{{"id":"TEST_SWORD"{ul}}}"#)).expect("attrs json")
}

fn reference(price: f64, seller: &str, attrs: ItemAttributes) -> Reference {
    Reference {
        price,
        sold_at: (NOW_MS / 1000) as f64,
        seller: seller.to_string(),
        tts_ms: None,
        attrs,
    }
}

fn build(refs: Vec<Reference>) -> PriceIndex {
    PriceIndex::build(refs, Bazaar::from_prices(HashMap::new(), NOW_MS), NOW_MS)
}

/// The family: 6 unstarred at 100M. star:1 at 105M (+5% < 15% bar, thin
/// premium) must FOLD; star:5 at 120M (+20%, n=4) must keep its fork.
fn family_refs() -> Vec<Reference> {
    let mut out = Vec::new();
    for i in 0..6 {
        out.push(reference(
            100_000_000.0,
            &format!("clean{i}"),
            sword_star(None),
        ));
    }
    for i in 0..4 {
        out.push(reference(
            105_000_000.0,
            &format!("s1_{i}"),
            sword_star(Some(1.0)),
        ));
    }
    for i in 0..4 {
        out.push(reference(
            120_000_000.0,
            &format!("s5_{i}"),
            sword_star(Some(5.0)),
        ));
    }
    out
}

#[test]
fn insignificant_star_folds_and_off_gate_is_byte_identical() {
    let idx = build(family_refs());
    let a1 = sword_star(Some(1.0));
    // OFF: the historical star fork, untouched.
    assert_eq!(idx.final_key_with_star_sig(&a1, false), "TEST_SWORD*1");
    assert_eq!(idx.final_key(&a1), "TEST_SWORD*1", "flag defaults to off");
    // ON: +5% at 4 samples is below the bar everywhere ⇒ fold to the family.
    assert_eq!(idx.final_key_with_star_sig(&a1, true), "TEST_SWORD");
    // The unstarred key is gate-invariant.
    let clean = sword_star(None);
    assert_eq!(idx.final_key_with_star_sig(&clean, true), "TEST_SWORD");
}

#[test]
fn a_measured_premium_keeps_the_star_fork() {
    let idx = build(family_refs());
    let a5 = sword_star(Some(5.0));
    assert_eq!(idx.final_key_with_star_sig(&a5, true), "TEST_SWORD*5");
    // … while star:1 stays folded on the SAME index: the gate is per level.
    assert_eq!(
        idx.final_key_with_star_sig(&sword_star(Some(1.0)), true),
        "TEST_SWORD"
    );
}

#[test]
fn never_traded_unstarred_folds_by_default() {
    // No unstarred group exists, so there is no bar to prove a premium
    // against: the refs unify under the bare key instead of fragmenting one
    // pool per star level.
    let mut refs = Vec::new();
    for i in 0..5 {
        refs.push(reference(
            80_000_000.0,
            &format!("s7_{i}"),
            sword_star(Some(7.0)),
        ));
    }
    let idx = build(refs);
    let a7 = sword_star(Some(7.0));
    assert_eq!(idx.final_key_with_star_sig(&a7, false), "TEST_SWORD*7");
    assert_eq!(idx.final_key_with_star_sig(&a7, true), "TEST_SWORD");
}

#[test]
fn the_bars_are_the_pass_two_ones() {
    // n=2 cannot prove significance even at +40% (MIN_FEATURE_SAMPLES = 3),
    // and n=4 at +10% cannot either (ATTR_MIN_SHARE = 15%): both fold. Same
    // numbers Pass 2 would refuse a feature fork for.
    let mut refs = Vec::new();
    for i in 0..6 {
        refs.push(reference(
            100_000_000.0,
            &format!("clean{i}"),
            sword_star(None),
        ));
    }
    for i in 0..2 {
        refs.push(reference(
            140_000_000.0,
            &format!("spike{i}"),
            sword_star(Some(9.0)),
        ));
    }
    for i in 0..4 {
        refs.push(reference(
            110_000_000.0,
            &format!("mild{i}"),
            sword_star(Some(3.0)),
        ));
    }
    let idx = build(refs);
    assert_eq!(
        idx.final_key_with_star_sig(&sword_star(Some(9.0)), true),
        "TEST_SWORD",
        "2 spike sales do not fork anything"
    );
    assert_eq!(
        idx.final_key_with_star_sig(&sword_star(Some(3.0)), true),
        "TEST_SWORD",
        "+10% is below the 15% share bar"
    );
}
