//! VARIANT_SIG: a cheap variant must not fork the price key.
//!
//! Real case, 2026-07-28: an Aspect of the Void listed at 56,000 with a ~24M
//! resale was rejected `notpriceable`, key `ASPECT_OF_THE_VOID~rune=SNOW1`.
//! `base_key` appends the variant unconditionally, so the rune forked both the
//! price pool AND the base group -- meaning pass-2 significance could never
//! compare "with rune" against "without rune" and learn the rune is worthless.
//!
//! These assert the two halves of the fix, and that OFF is unchanged.

use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{base_key, candidate_features};

// Built through serde like the other golden tests, since ItemAttributes has no
// Default and most fields are #[serde(default)].
fn aotv(variant: &str) -> ItemAttributes {
    serde_json::from_value(serde_json::json!({
        "id": "ASPECT_OF_THE_VOID",
        "variant": variant,
    }))
    .expect("ItemAttributes from json")
}

#[test]
fn variant_forks_the_base_key_when_the_flag_is_off() {
    // Default behaviour, and why the miss happened.
    if *finder_core::config::VARIANT_SIG {
        return; // env has it on; the other test covers that
    }
    assert_eq!(base_key(&aotv("")), "ASPECT_OF_THE_VOID");
    assert_eq!(
        base_key(&aotv("rune=SNOW1")),
        "ASPECT_OF_THE_VOID~rune=SNOW1"
    );
    let feats = candidate_features(&aotv("rune=SNOW1"));
    assert!(
        !feats.iter().any(|f| f.starts_with("var:")),
        "variant must not be emitted as a feature when the flag is off"
    );
}

#[test]
fn variant_becomes_a_candidate_feature_when_the_flag_is_on() {
    if !*finder_core::config::VARIANT_SIG {
        return; // run with VARIANT_SIG=1
    }
    // Base key collapses, so the item shares a pool with plain AOTV and pass-2
    // finally has a comparison group.
    assert_eq!(base_key(&aotv("rune=SNOW1")), "ASPECT_OF_THE_VOID");
    assert_eq!(base_key(&aotv("")), "ASPECT_OF_THE_VOID");

    // ...and the variant is offered up for significance instead of vanishing.
    let feats = candidate_features(&aotv("rune=SNOW1"));
    assert!(
        feats.contains(&"var:rune=SNOW1".to_string()),
        "got {feats:?}"
    );

    // build_variant joins multiple parts with '|'; each must be judged separately
    // so an expensive dye can keep its key while a cheap rune does not.
    let feats = candidate_features(&aotv("rune=SNOW1|dye=DYE_NECRON"));
    assert!(
        feats.contains(&"var:rune=SNOW1".to_string()),
        "got {feats:?}"
    );
    assert!(
        feats.contains(&"var:dye=DYE_NECRON".to_string()),
        "got {feats:?}"
    );
}
