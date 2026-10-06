//! The lbin lane relists against `list[1]` (the 2nd-cheapest LIVE listing). That
//! listing can be an overpriced wall with no relation to what the item actually
//! sells for. Prod, 2026-07-23: a "Placeable Fairy Soul" that sells for ~36M had
//! a lone 98M live wall, so the lane emitted a phantom flip (buy 38M, "resell"
//! 98M, +58M, 153% ROI, confidence 0.79) against an exit price that does not
//! exist. The sold-median cap (LBIN_REF_SOLD_MULT) must kill that flip while
//! leaving a normally priced book untouched.

use finder_core::bazaar::Bazaar;
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{PriceIndex, Reference};
use finder_core::sniper::{eval_lbin_flips, ActiveAuction, Bin, DecodedAuction, RelistTracker};
use std::collections::{HashMap, HashSet};

const NOW_MS: i64 = 1_783_941_749_000;

fn soul() -> ItemAttributes {
    serde_json::from_value(serde_json::json!({ "id": "PLACEABLE_FAIRY_SOUL_RIFT" }))
        .expect("soul attrs")
}

/// ~36M sold median from a dozen distinct sellers, plus one 100M outlier print
/// (the sold twin of the live wall). Median is robust to it: ~36M either way.
fn refs() -> Vec<Reference> {
    let mut out = Vec::new();
    for i in 0..12 {
        out.push(Reference {
            price: 36_000_000.0,
            sold_at: (NOW_MS / 1000 - i * 3600) as f64,
            seller: format!("seller{i}"),
            tts_ms: None,
            attrs: soul(),
        });
    }
    out.push(Reference {
        price: 100_000_000.0,
        sold_at: (NOW_MS / 1000 - 36000) as f64,
        seller: "__ur".into(),
        attrs: soul(),
        tts_ms: None,
    });
    out
}

fn setup() -> PriceIndex {
    let r = refs();
    PriceIndex::build(r, Bazaar::from_prices(HashMap::new(), NOW_MS), NOW_MS)
}

fn decoded(idx: &PriceIndex, uuid: &str, price: f64) -> DecodedAuction {
    let attrs = soul();
    let key = idx.final_key(&attrs);
    DecodedAuction {
        a: ActiveAuction {
            uuid: uuid.into(),
            starting_bid: price,
            auctioneer: Some("someseller".into()),
            item_name: "Placeable Fairy Soul".into(),
        },
        attrs,
        key,
    }
}

/// Build a live BIN book for the soul key: (uuid, price) pairs, sorted ascending
/// like the real pipeline does.
fn book(idx: &PriceIndex, listings: &[(&str, f64)]) -> HashMap<String, Vec<Bin>> {
    let key = idx.final_key(&soul());
    let mut list: Vec<Bin> = listings
        .iter()
        .map(|(u, p)| Bin {
            uuid: (*u).into(),
            price: *p,
        })
        .collect();
    list.sort_by(|x, y| x.price.partial_cmp(&y.price).unwrap());
    let mut m = HashMap::new();
    m.insert(key, list);
    m
}

#[test]
fn lone_wall_does_not_manufacture_a_flip() {
    let idx = setup();
    // Cheapest listing (38M, the snipe) then a 98M wall. Exactly the prod book.
    let bykey = book(
        &idx,
        &[
            ("snipe", 38_000_000.0),
            ("wall", 98_000_000.0),
            ("w2", 99_000_000.0),
            ("w3", 100_000_000.0),
        ],
    );
    let cands = vec![decoded(&idx, "snipe", 38_000_000.0)];
    let mut seen = HashSet::new();
    let mut relist = RelistTracker::default();
    let flips = eval_lbin_flips(
        &cands,
        &bykey,
        &mut seen,
        &mut relist,
        NOW_MS as f64,
        &idx,
        NOW_MS as f64,
    );
    assert!(
        flips.is_empty(),
        "a 38M snipe on a soul that sells for 36M must NOT flip against a 98M wall; got {:?}",
        flips
            .iter()
            .map(|f| (f.reference, f.roi_pct, &f.guard))
            .collect::<Vec<_>>()
    );
}

#[test]
fn normally_priced_book_still_flips_untouched() {
    let idx = setup();
    // Whole book sits near the 36M sold value: a genuine undercut. The cap must
    // not bind, and the reference must stay the real 2nd listing (38M).
    let bykey = book(
        &idx,
        &[
            ("snipe", 30_000_000.0),
            ("l1", 38_000_000.0),
            ("l2", 39_000_000.0),
            ("l3", 40_000_000.0),
        ],
    );
    let cands = vec![decoded(&idx, "snipe", 30_000_000.0)];
    let mut seen = HashSet::new();
    let mut relist = RelistTracker::default();
    let flips = eval_lbin_flips(
        &cands,
        &bykey,
        &mut seen,
        &mut relist,
        NOW_MS as f64,
        &idx,
        NOW_MS as f64,
    );
    assert_eq!(
        flips.len(),
        1,
        "a real undercut on a normally priced book must still flip"
    );
    assert_eq!(
        flips[0].reference, 38_000_000.0,
        "a normal book's reference must be untouched"
    );
    assert!(
        !flips[0].guard.contains("wall_capped"),
        "normal book must not be wall_capped"
    );
}
