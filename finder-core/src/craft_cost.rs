//! Port of `baf-flip-finder/src/craftCost.ts` — craft-cost ceiling for STARRED
//! items. Behavior pinned by `goldens/craftCost/matrix.json`.

use crate::config::{CRAFT_CEILING_MULT, ESSENCE_PER_STAR_COST};
use crate::nbt::ItemAttributes;
use crate::price_index::{candidate_features, PriceIndex};

/// Absolute star number (6..10) → the master star consumed at that step.
fn master_star(s: i64) -> Option<&'static str> {
    match s {
        6 => Some("FIRST_MASTER_STAR"),
        7 => Some("SECOND_MASTER_STAR"),
        8 => Some("THIRD_MASTER_STAR"),
        9 => Some("FOURTH_MASTER_STAR"),
        10 => Some("FIFTH_MASTER_STAR"),
        _ => None,
    }
}

fn master_star_fallback(s: i64) -> f64 {
    match s {
        6 => 16_000_000.0,
        7 => 23_000_000.0,
        8 => 37_000_000.0,
        9 => 66_000_000.0,
        10 => 170_000_000.0,
        _ => 0.0,
    }
}

const NECRON_SWORDS: [&str; 4] = ["HYPERION", "ASTRAEA", "SCYLLA", "VALKYRIE"];
const NECRON_HANDLE_FALLBACK: f64 = 25_000_000.0;

/// Coin cost of the star materials to bring an item to `level` stars.
pub fn star_material_cost(level: f64, index: &PriceIndex) -> f64 {
    // `!(level > 0)` (not `level <= 0`) to match TS's NaN handling verbatim.
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    if !(level > 0.0) {
        return 0.0;
    }
    let mut sum = level * *ESSENCE_PER_STAR_COST;
    let hi = level.min(10.0) as i64;
    for s in 6..=hi {
        let bz = if index.bazaar_ready() {
            index.bazaar_price(master_star(s).unwrap())
        } else {
            None
        };
        sum += match bz {
            Some(v) if v > 0.0 => v,
            _ => master_star_fallback(s),
        };
    }
    sum
}

/// Positive bazaar value or None.
fn bz(id: &str, index: &PriceIndex) -> Option<f64> {
    let v = if index.bazaar_ready() {
        index.bazaar_price(id)
    } else {
        None
    };
    match v {
        Some(x) if x > 0.0 => Some(x),
        _ => None,
    }
}

fn recipe_base_floor(id: &str, index: &PriceIndex) -> Option<f64> {
    let laser = bz("GIANT_FRAGMENT_LASER", index)?;
    let catalyst = bz("WITHER_CATALYST", index)?;
    let handle = {
        let h = index.base_value_for("NECRON_HANDLE");
        if h != 0.0 {
            h
        } else {
            NECRON_HANDLE_FALLBACK
        }
    };
    let necron_blade = 24.0 * catalyst + handle;
    if id == "NECRON_BLADE" {
        return Some(necron_blade);
    }
    if NECRON_SWORDS.contains(&id) {
        return Some(8.0 * laser + necron_blade);
    }
    None
}

fn component_value(a: &ItemAttributes, index: &PriceIndex) -> f64 {
    let mut sum = 0.0;
    for f in candidate_features(a) {
        if let Some(v) = index.feature_value(&f) {
            if v > 0.0 {
                sum += v;
            }
        }
    }
    sum
}

/// Upper-bound craft cost of a starred item, or None when it can't/shouldn't cap.
pub fn craft_ceiling(a: &ItemAttributes, index: &PriceIndex) -> Option<f64> {
    if *CRAFT_CEILING_MULT <= 0.0 {
        return None;
    }
    let ul = a.upgrade_level?;
    if ul <= 0.0 {
        return None;
    }
    if a.pet.is_some() || a.id == "ENCHANTED_BOOK" {
        return None;
    }
    let stars = star_material_cost(ul, index);
    if stars <= 0.0 {
        return None;
    }

    let mut bases: Vec<f64> = Vec::new();
    let bv = index.base_value_for(&a.id);
    if bv > 0.0 {
        bases.push(bv);
    }
    if let Some(z) = index.zero_star_baseline(a) {
        bases.push(z);
    }
    if let Some(floor) = recipe_base_floor(&a.id, index) {
        bases.push(floor + component_value(a, index));
    }
    if bases.is_empty() {
        return None;
    }
    let mut base0 = 0.0;
    for b in bases {
        if b > base0 {
            base0 = b;
        }
    }
    Some((base0 + stars) * *CRAFT_CEILING_MULT)
}
