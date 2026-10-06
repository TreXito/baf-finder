//! Port of `baf-flip-finder/src/petLevels.ts` — pet exp → level + coarse band.
//! Constant tables extracted verbatim from the TS source; behavior pinned by
//! `goldens/petLevels/matrix.json`.

/// Per-level-up exp costs, indexed by rarity offset (119 entries).
const PET_LEVELS: [i64; 119] = [
    100, 110, 120, 130, 145, 160, 175, 190, 210, 230, 250, 275, 300, 330, 360, 400, 440, 490, 540,
    600, 660, 730, 800, 880, 960, 1050, 1150, 1260, 1380, 1510, 1650, 1800, 1960, 2130, 2310, 2500,
    2700, 2920, 3160, 3420, 3700, 4000, 4350, 4750, 5200, 5700, 6300, 7000, 7800, 8700, 9700,
    10800, 12000, 13300, 14700, 16200, 17800, 19500, 21300, 23200, 25200, 27400, 29800, 32400,
    35200, 38200, 41400, 44800, 48400, 52200, 56200, 60400, 64800, 69400, 74200, 79200, 84700,
    90700, 97200, 104200, 111700, 119700, 128200, 137200, 146700, 156700, 167700, 179700, 192700,
    206700, 221700, 237700, 254700, 272700, 291700, 311700, 333700, 357700, 383700, 411700, 441700,
    476700, 516700, 561700, 611700, 666700, 726700, 791700, 861700, 936700, 1016700, 1101700,
    1191700, 1286700, 1386700, 1496700, 1616700, 1746700, 1886700,
];

/// Pets that level past 100 (Golden/Jade/Rose Dragon): extra per-level costs
/// (all three share this table and a max of 200).
const DRAGON_EXTRA: [i64; 100] = [
    0, 5555, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700,
    1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700,
    1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700,
    1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700,
    1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700,
    1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700,
    1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700,
    1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700,
    1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700,
    1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700, 1886700,
];

fn rarity_offset(tier: &str) -> usize {
    match tier {
        "COMMON" => 0,
        "UNCOMMON" => 6,
        "RARE" => 11,
        "EPIC" => 16,
        // LEGENDARY and MYTHIC both 20; unknown tiers fall back to LEGENDARY (20).
        _ => 20,
    }
}

/// `OVER_LEVEL_PETS[type]` → (max, extra costs) for the 200-cap dragons.
fn over_level(pet_type: &str) -> Option<(i64, &'static [i64])> {
    match pet_type {
        "GOLDEN_DRAGON" | "JADE_DRAGON" | "ROSE_DRAGON" => Some((200, &DRAGON_EXTRA)),
        _ => None,
    }
}

/// Exact pet level from cumulative exp, honouring rarity and over-level pets.
pub fn pet_level(pet_type: &str, tier: &str, exp: f64) -> i64 {
    let off = rarity_offset(tier);
    // PET_LEVELS.slice(off, off + 99)
    let base = &PET_LEVELS[off..off + 99];
    let over = over_level(pet_type);
    let max_level = over.map(|(m, _)| m).unwrap_or(100);
    let mut level: i64 = 1;
    let mut rest = exp;
    // iterate base costs, then the over-level extras (concat)
    let extra: &[i64] = over.map(|(_, e)| e).unwrap_or(&[]);
    for &c in base.iter().chain(extra.iter()) {
        if rest < c as f64 {
            break;
        }
        rest -= c as f64;
        level += 1;
        if level >= max_level {
            break;
        }
    }
    level.min(max_level)
}

/// Coarse level band used in price keys.
pub fn pet_level_band(pet_type: &str, tier: &str, exp: f64) -> String {
    let lvl = pet_level(pet_type, tier, exp);
    if over_level(pet_type).is_some() {
        if lvl >= 200 {
            "max".into()
        } else if lvl >= 150 {
            "l150".into()
        } else {
            "l100".into()
        }
    } else if lvl >= 100 {
        "max".into()
    } else if lvl >= 90 {
        "l90".into()
    } else if lvl >= 50 {
        "l50".into()
    } else {
        "l1".into()
    }
}
