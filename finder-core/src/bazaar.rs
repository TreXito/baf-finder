//! Port of `baf-flip-finder/src/bazaar.ts` (the pricing-relevant half):
//! `bazaarPrice`, `bazaarReady`, and `featureBazaarValue`. The live-fetch
//! (`refreshBazaar`) is out of scope for the domain core — the goldens serialize
//! the bazaar price map as an INPUT, loaded via [`Bazaar::from_prices`].
//! REFORGE_STONE / TOKEN_PRODUCT tables extracted verbatim from the TS.

use std::collections::HashMap;

const BAZAAR_TTL_MS: i64 = 6 * 60 * 60 * 1000;

/// reforge name → reforge-stone product id. `Some("")` = plain blacksmith reforge
/// (no coin value); `None` = unknown reforge (statistics decide).
fn reforge_stone(name: &str) -> Option<&'static str> {
    match name {
        "withered" => Some("WITHER_BLOOD"),
        "fabled" => Some("DRAGON_CLAW"),
        "renowned" => Some("DRAGON_HORN"),
        "spiritual" => Some("SPIRIT_STONE"),
        "giant" => Some("GIANT_TOOTH"),
        "submerged" => Some("DEEP_SEA_ORB"),
        "jaded" => Some("JADERALD"),
        "ancient" => Some("PRECURSOR_GEAR"),
        "necrotic" => Some("NECROMANCER_BROOCH"),
        "loving" => Some("RED_SCARF"),
        "mossy" => Some("OVERGROWN_GRASS"),
        "festive" => Some("FROZEN_BAUBLE"),
        "gilded" => Some("MIDAS_JEWEL"),
        "stiff" => Some("HARDENED_WOOD"),
        "chomp" => Some("KUUDRA_MANDIBLE"),
        "ambered" => Some("AMBER_MATERIAL"),
        "fruitful" => Some("ONYX"),
        "magnetic" => Some("LAPIS_CRYSTAL"),
        "fleet" => Some("DIAMONITE"),
        "refined" => Some("REFINED_AMBER"),
        "blazing" => Some("BLAZEN_SPHERE"),
        "royal" => Some("DWARVEN_TREASURE"),
        "glacial" => Some("FRIGID_HUSK"),
        "waxed" => Some("BLAZE_WAX"),
        "fortified" => Some("METEOR_SHARD"),
        "rooted" => Some("BURROWING_SPORES"),
        "blood_soaked" => Some("PRESUMED_GALLON_OF_RED_PAINT"),
        "treacherous" => Some("RUSTY_ANCHOR"),
        "lucky" => Some("LUCKY_DICE"),
        "dirty" => Some("DIRT_BOTTLE"),
        "moil" => Some("MOIL_LOG"),
        "toil" => Some("TOIL_LOG"),
        "bustling" => Some("SKYMART_BROCHURE"),
        "suspicious" => Some("SUSPICIOUS_VIAL"),
        "snowy" => Some("TERRY_SNOWGLOBE"),
        "bulky" => Some("BULKY_STONE"),
        "pitchin" => Some("PITCHIN_KOI"),
        "precise" => Some("OPTICAL_LENS"),
        "spiked" => Some("DRAGON_SCALE"),
        "perfect" => Some("DIAMOND_ATOM"),
        "headstrong" => Some("SALMON_OPAL"),
        "reinforced" => Some("RARE_DIAMOND"),
        "cubic" => Some("MOLTEN_CUBE"),
        "undead" => Some("PREMIUM_FLESH"),
        "ridiculous" => Some("RED_NOSE"),
        "empowered" => Some("SADAN_BROOCH"),
        "strengthened" => Some("SEARING_STONE"),
        "hyper" => Some("ENDSTONE_GEODE"),
        "coldfused" => Some("ENTROPY_SUPPRESSOR"),
        "dimensional" => Some("TITANIUM_TESSERACT"),
        "greater_spook" => Some("BOO_STONE"),
        "earthy" => Some("LARGE_WALNUT"),
        "bountiful" => Some("GOLDEN_BALL"),
        "stellar" => Some("PETRIFIED_STARFALL"),
        "heated" => Some("HOT_STUFF"),
        "scraped" => Some("POCKET_ICEBERG"),
        "candied" => Some("CANDY_CORN"),
        "sharp" => Some(""),
        "spicy" => Some(""),
        "legendary" => Some(""),
        "epic" => Some(""),
        "fair" => Some(""),
        "fast" => Some(""),
        "gentle" => Some(""),
        "heroic" => Some(""),
        "odd" => Some(""),
        "clean" => Some(""),
        "fierce" => Some(""),
        "heavy" => Some(""),
        "light" => Some(""),
        "mythic" => Some(""),
        "pure" => Some(""),
        "smart" => Some(""),
        "titanic" => Some(""),
        "wise" => Some(""),
        "bizarre" => Some(""),
        "itchy" => Some(""),
        "ominous" => Some(""),
        "pleasant" => Some(""),
        "pretty" => Some(""),
        "simple" => Some(""),
        "strange" => Some(""),
        "vivid" => Some(""),
        "godly" => Some(""),
        "demonic" => Some(""),
        "forceful" => Some(""),
        "hurtful" => Some(""),
        "keen" => Some(""),
        "strong" => Some(""),
        "superior" => Some(""),
        "unpleasant" => Some(""),
        "zealous" => Some(""),
        "awkward" => Some(""),
        "deadly" => Some(""),
        "fine" => Some(""),
        "grand" => Some(""),
        "hasty" => Some(""),
        "neat" => Some(""),
        "rapid" => Some(""),
        "rich" => Some(""),
        "unreal" => Some(""),
        "double_bit" => Some(""),
        "lumberjack" => Some(""),
        "great" => Some(""),
        "rugged" => Some(""),
        "lush" => Some(""),
        "green_thumb" => Some(""),
        "robust" => Some(""),
        "zooming" => Some(""),
        "peasant" => Some(""),
        "blessed" => Some(""),
        "bountiful_farming" => Some(""),
        _ => None,
    }
}

/// Fixed feature-token → bazaar product id (beyond enchants/scrolls/gems).
fn token_product(token: &str) -> Option<&'static str> {
    match token {
        "recomb" => Some("RECOMBOBULATOR_3000"),
        "x:art_of_war" => Some("THE_ART_OF_WAR"),
        "x:art_of_peace" => Some("THE_ART_OF_PEACE"),
        "x:wood_singularity" => Some("WOOD_SINGULARITY"),
        "x:ethermerge" => Some("ETHERWARP_MERGER"),
        "x:hpb" => Some("HOT_POTATO_BOOK"),
        "x:fuming" => Some("FUMING_POTATO_BOOK"),
        "x:jalapeno" => Some("JALAPENO_BOOK"),
        "x:mana_disintegrator" => Some("MANA_DISINTEGRATOR"),
        "x:tuner" => Some("TRANSMISSION_TUNER"),
        "x:silex" => Some("SIL_EX"),
        "x:ffd" => Some("FARMING_FOR_DUMMIES"),
        "x:polarvoid" => Some("POLARVOID_BOOK"),
        "x:wet_book" => Some("WET_BOOK"),
        "x:divan_powder" => Some("DIVAN_POWDER_COATING"),
        _ => None,
    }
}

/// Bazaar price cache: product_id → instant-sell value.
pub struct Bazaar {
    prices: HashMap<String, f64>,
    last_refresh_ms: i64,
}

impl Bazaar {
    /// Build from a serialized price map (goldens) with a given refresh stamp.
    pub fn from_prices(prices: HashMap<String, f64>, last_refresh_ms: i64) -> Self {
        Bazaar {
            prices,
            last_refresh_ms,
        }
    }

    /// Direct product lookup (e.g. "IMPLOSION_SCROLL").
    pub fn bazaar_price(&self, product_id: &str) -> Option<f64> {
        self.prices.get(product_id).copied()
    }

    pub fn bazaar_ready(&self, now_ms: i64) -> bool {
        !self.prices.is_empty() && now_ms - self.last_refresh_ms < BAZAAR_TTL_MS
    }

    /// Coin value of a candidate feature token, or `None` when the bazaar can't
    /// price this class of feature. Enchants with no listing at all return
    /// `Some(0.0)` (an untradable book can't be worth keying).
    pub fn feature_bazaar_value(&self, token: &str, now_ms: i64) -> Option<f64> {
        if !self.bazaar_ready(now_ms) {
            return None;
        }
        self.compute(token, now_ms)
    }

    fn get(&self, id: &str) -> Option<f64> {
        self.prices.get(id).copied()
    }

    fn compute(&self, token: &str, now_ms: i64) -> Option<f64> {
        // Counted token "x:fuming*5" → 5 × unit value.
        if let Some(star) = token.find('*') {
            if star > 0 {
                let unit = self.feature_bazaar_value(&token[..star], now_ms);
                let count: f64 = token[star + 1..].trim().parse().unwrap_or(f64::NAN);
                return unit.map(|u| {
                    let c = if count.is_nan() {
                        f64::NAN
                    } else {
                        count.max(1.0)
                    };
                    u * c
                });
            }
        }

        if let Some(mapped) = token_product(token) {
            return self.get(mapped);
        }

        if let Some(rest) = token.strip_prefix("ench:") {
            // "ench:ultimate_legion5" → ENCHANTMENT_ULTIMATE_LEGION_5
            let split = rest
                .rfind(|c: char| !c.is_ascii_digit())
                .map(|i| i + 1)
                .unwrap_or(0);
            let (name, lvl_s) = rest.split_at(split);
            if name.is_empty() || lvl_s.is_empty() {
                return None;
            }
            let lvl: i64 = match lvl_s.parse() {
                Ok(l) => l,
                Err(_) => return None,
            };
            let upper = name.to_uppercase();
            // Build "ENCHANTMENT_<NAME>_" ONCE and re-stamp only the level digits.
            // The descending walk below used to `format!` the whole key again on
            // every level, so a miss at level 10 cost ten allocations and ten trips
            // through core::fmt; this costs one allocation for the whole lookup.
            // `to_uppercase` is kept (not to_ascii_uppercase) so non-ASCII names
            // map exactly as before.
            let mut key = String::with_capacity(12 + upper.len() + 4);
            key.push_str("ENCHANTMENT_");
            key.push_str(&upper);
            key.push('_');
            let stem = key.len();
            let stamp = |k: &mut String, n: i64| {
                k.truncate(stem);
                let mut buf = [0u8; 20];
                k.push_str(crate::price_index::fmt_i64(&mut buf, n));
            };
            stamp(&mut key, lvl);
            if let Some(exact) = self.get(&key) {
                return Some(exact);
            }
            // Above the highest listed level: value doubles per level, capped ×4.
            let mut base = lvl - 1;
            while base >= 1 {
                stamp(&mut key, base);
                if let Some(v) = self.get(&key) {
                    let exp = (lvl - base).min(2) as u32;
                    return Some(v * 2f64.powi(exp as i32));
                }
                base -= 1;
            }
            return Some(0.0); // no level listed at all = untradable book
        }

        if let Some(rest) = token.strip_prefix("gem:") {
            return self.get(&format!("{rest}_GEM"));
        }
        if let Some(rest) = token.strip_prefix("x:enrich_") {
            return self.get(&format!("TALISMAN_ENRICHMENT_{}", rest.to_uppercase()));
        }
        if let Some(rest) = token.strip_prefix("x:power_") {
            return self.get(&rest.to_uppercase());
        }
        if let Some(rest) = token.strip_prefix("reforge:") {
            return match reforge_stone(rest) {
                None => None,          // unknown reforge → statistics decide
                Some("") => Some(0.0), // plain blacksmith reforge = no coin value
                Some(stone) => self.get(stone),
            };
        }
        if let Some(rest) = token.strip_prefix("scroll:") {
            return self.get(rest);
        }
        if let Some(rest) = token.strip_prefix("pethled:") {
            return self.get(rest);
        }
        None
    }
}
