//! `NBT_COUNT` / `NBT_EXTRA_FIELDS`: the fields `decode_item_bytes` never read.
//!
//! The parser jumps to `root.i[0].tag.ExtraAttributes` and reads a fixed key
//! list, so two things were structurally invisible:
//!
//!   - `Count`, which sits on `i[0]` -- every stack size of an item shared one
//!     price pool. A live dump has KAT_FLOWER asking 650k at 1x and 40.9M at
//!     64x, both priced off the same median.
//!   - `model`, which COFL uses to tell Abicases apart. All 2,886 Abicase sales
//!     in our store looked identical to us, pooled at a 19.49M median, so a
//!     `sumsung_2` worth 25.67M read as below-median and got gated. That miss
//!     was 8.66M of profit.
//!
//! Both flags default OFF and both halves are asserted: OFF must be byte-
//! identical (the 929-case nbt golden and the inventory-slot golden both pin the
//! serialized shape), ON must key the items apart.
//!
//! Run the ON halves with `NBT_COUNT=1 NBT_EXTRA_FIELDS=1 cargo test`.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use finder_core::nbt::{decode_item_bytes, ItemAttributes};
use finder_core::price_index::{base_key, candidate_features};
use simdnbt::owned;

/// Real item_bytes in the shape decode_item_bytes expects, with `Count` on i[0]
/// where Hypixel actually puts it (NOT inside ExtraAttributes).
fn item_bytes(id: &str, count: i8, extra_pairs: &[(&str, &str)]) -> String {
    let mut extra = owned::NbtCompound::new();
    extra.insert("id", id);
    for (k, v) in extra_pairs {
        extra.insert(*k, *v);
    }
    let mut tag = owned::NbtCompound::new();
    tag.insert("ExtraAttributes", extra);
    let mut item = owned::NbtCompound::new();
    item.insert("tag", tag);
    item.insert("Count", owned::NbtTag::Byte(count));
    let mut root = owned::NbtCompound::new();
    root.insert("i", owned::NbtList::Compound(vec![item]));
    let mut buf = Vec::new();
    owned::BaseNbt::new("", root).write(&mut buf);
    STANDARD.encode(&buf)
}

fn decode(id: &str, count: i8, pairs: &[(&str, &str)]) -> ItemAttributes {
    decode_item_bytes(&item_bytes(id, count, pairs)).expect("decodes")
}

// ----------------------------- NBT_COUNT -----------------------------

#[test]
fn stack_size_is_invisible_when_the_flag_is_off() {
    if *finder_core::config::NBT_COUNT {
        return; // env has it on; the other test covers that
    }
    // This is the bug, pinned: a 64-stack and a single are the same key, so a
    // 64x listing is judged against single-item sales and vice versa.
    let single = decode("KAT_FLOWER", 1, &[]);
    let stack = decode("KAT_FLOWER", 64, &[]);
    assert_eq!(single.count, 1);
    assert_eq!(stack.count, 1, "Count is not read when the flag is off");
    assert_eq!(base_key(&single), base_key(&stack));
    assert_eq!(base_key(&stack), "KAT_FLOWER");
}

#[test]
fn stack_size_splits_the_key_when_the_flag_is_on() {
    if !*finder_core::config::NBT_COUNT {
        return; // run with NBT_COUNT=1
    }
    let single = decode("KAT_FLOWER", 1, &[]);
    let stack = decode("KAT_FLOWER", 64, &[]);
    assert_eq!(single.count, 1);
    assert_eq!(stack.count, 64);
    assert_eq!(
        base_key(&single),
        "KAT_FLOWER",
        "a single must keep its old key"
    );
    assert_eq!(base_key(&stack), "KAT_FLOWERx64");
    assert_ne!(base_key(&single), base_key(&stack));

    // Every distinct stack size is its own good, not a multiple of the single:
    // a 10x does not sell for 10x, so they must not be normalised together.
    let sizes: Vec<String> = [2i8, 4, 8, 16, 32, 64]
        .iter()
        .map(|c| base_key(&decode("GOBLIN_OMELETTE", *c, &[])))
        .collect();
    let uniq: std::collections::HashSet<_> = sizes.iter().collect();
    assert_eq!(
        uniq.len(),
        sizes.len(),
        "each stack size needs its own key: {sizes:?}"
    );

    // The suffix must not collide with the star suffix or the variant marker.
    let starred: ItemAttributes = serde_json::from_value(serde_json::json!({
        "id": "KAT_FLOWER", "upgradeLevel": 5, "count": 64
    }))
    .unwrap();
    assert_eq!(base_key(&starred), "KAT_FLOWER*5x64");
}

#[test]
fn count_is_omitted_from_the_wire_unless_it_is_a_stack() {
    // Millions of refs are already persisted without this field, and the goldens
    // pin the serialized shape, so a single must serialize exactly as before.
    let single = decode("KAT_FLOWER", 1, &[]);
    let json = serde_json::to_value(&single).unwrap();
    assert!(
        json.get("count").is_none(),
        "count must not appear for a single: {json}"
    );

    // ...and a legacy ref with no count field must read back as a single.
    let legacy: ItemAttributes =
        serde_json::from_value(serde_json::json!({"id": "KAT_FLOWER"})).unwrap();
    assert_eq!(legacy.count, 1);

    if *finder_core::config::NBT_COUNT {
        let stack = decode("KAT_FLOWER", 64, &[]);
        let json = serde_json::to_value(&stack).unwrap();
        assert_eq!(json.get("count").and_then(|v| v.as_i64()), Some(64));
        // Round-trips, so a stack sale stored today keys the same way tomorrow.
        let back: ItemAttributes = serde_json::from_value(json).unwrap();
        assert_eq!(base_key(&back), base_key(&stack));
    }
}

// -------------------------- NBT_EXTRA_FIELDS --------------------------

#[test]
fn identity_fields_are_invisible_when_the_flag_is_off() {
    if *finder_core::config::NBT_EXTRA_FIELDS {
        return;
    }
    // The Abicase miss, pinned: two visibly different phones, one key.
    let a = decode("ABICASE", 1, &[("model", "sumsung_2")]);
    let b = decode("ABICASE", 1, &[("model", "blue_aqua")]);
    assert!(a.extras.is_empty(), "got {:?}", a.extras);
    assert_eq!(base_key(&a), base_key(&b));
    assert!(!candidate_features(&a).iter().any(|f| f.contains("model")));
}

#[test]
fn identity_fields_become_candidate_features_when_the_flag_is_on() {
    if !*finder_core::config::NBT_EXTRA_FIELDS {
        return; // run with NBT_EXTRA_FIELDS=1
    }
    let a = decode("ABICASE", 1, &[("model", "sumsung_2")]);
    assert_eq!(
        a.extras.get("model_sumsung_2"),
        Some(&1.0),
        "got {:?}",
        a.extras
    );
    assert!(
        candidate_features(&a).contains(&"x:model_sumsung_2".to_string()),
        "got {:?}",
        candidate_features(&a)
    );

    // Deliberately NOT in the base key: every Abicase must stay in one base group
    // so pass-2 significance has something to compare a model against. Splitting
    // the base key would strand them all with no refs at all.
    let b = decode("ABICASE", 1, &[("model", "blue_aqua")]);
    assert_eq!(base_key(&a), "ABICASE");
    assert_eq!(base_key(&a), base_key(&b));

    // Same treatment for the Bucket of Dye field.
    let d = decode("BUCKET_OF_DYE", 1, &[("dye_donated", "DYE_CHOCOLATE")]);
    assert_eq!(
        d.extras.get("dye_donated_dye_chocolate"),
        Some(&1.0),
        "got {:?}",
        d.extras
    );

    // Case is normalised, so SUMSUNG_2 and sumsung_2 are one feature and not two
    // half-populated pools.
    let upper = decode("ABICASE", 1, &[("model", "SUMSUNG_2")]);
    assert_eq!(upper.extras.get("model_sumsung_2"), Some(&1.0));
}

#[test]
fn an_item_without_the_new_fields_is_untouched_either_way() {
    // The overwhelming majority of items carry neither field; they must decode
    // identically no matter how the flags are set.
    let plain = decode("HYPERION", 1, &[("modifier", "heroic")]);
    assert_eq!(plain.count, 1);
    assert_eq!(base_key(&plain), "HYPERION");
    assert!(!plain
        .extras
        .keys()
        .any(|k| k.starts_with("model_") || k.starts_with("dye_donated_")));
}
