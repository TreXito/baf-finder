//! Port of `baf-flip-finder/src/nbt.ts`.
//!
//! `decodeItemBytes` (base64 → gzip → NBT → simplify → `.i[0].tag.ExtraAttributes`)
//! and `attrsFromExtra` (raw ExtraAttributes → the simplified [`ItemAttributes`]
//! used for keying). Behavior is fixed by `goldens/nbt/page0.json`.
//!
//! NBT is parsed with **simdnbt** (SIMD-accelerated; needs the nightly pin). The
//! SEMANTICS must match prismarine-nbt's `simplify()` as the TS consumed it. The
//! load-bearing quirk: prismarine simplifies a `TAG_Long` to a `[high, low]` int32
//! array, so the TS `Number(longValue)` yields `NaN`. We reproduce that in
//! [`tag_num`] (Long → NaN) so keys/variants match bug-for-bug.

use crate::config::{
    CROWN_COINS_BAND, LORE_WEIGHT_ITEMS, LORE_WEIGHT_MIN, MIDAS_TOTAL_COINS, NBT_AUTO_FIELDS,
    NBT_COUNT, NBT_EXTRA_FIELDS, PULSE_CHARGE_BAND,
};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use flate2::{Decompress, FlushDecompress, Status};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use simdnbt::borrow::{NbtCompound, NbtTag};
use std::io::Cursor;

/// The decoded, simplified attributes of a SkyBlock item used for keying.
/// Field names / shape match the TS `ItemAttributes` (serialized camelCase).
/// Numeric maps are `IndexMap` (insertion order) so `candidateFeatures` /
/// `sigSignature` iterate in the same order the TS `Object.entries` does.
/// `#[serde(default)]` lets legacy refs (missing gems/parts/itemUuid…) deserialize.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ItemAttributes {
    pub id: String,
    #[serde(default)]
    pub attributes: IndexMap<String, f64>,
    #[serde(default)]
    pub enchantments: IndexMap<String, f64>,
    #[serde(default)]
    pub scrolls: Vec<String>,
    #[serde(default)]
    pub reforge: Option<String>,
    #[serde(default)]
    pub skin: Option<String>,
    #[serde(default)]
    pub recombobulated: bool,
    #[serde(default)]
    pub upgrade_level: Option<f64>,
    #[serde(default)]
    pub gem_slots: i64,
    #[serde(default)]
    pub gems: Vec<String>,
    #[serde(default)]
    pub parts: Vec<String>,
    #[serde(default)]
    pub extras: IndexMap<String, f64>,
    #[serde(default)]
    pub pet: Option<Pet>,
    #[serde(default)]
    pub variant: String,
    #[serde(default)]
    pub item_uuid: Option<String>,
    /// Stack size, from `i[0].Count` -- OUTSIDE ExtraAttributes, which is why it
    /// was invisible until `NBT_COUNT`. Defaults to 1 so the millions of refs
    /// already persisted without it deserialize as singles, and is omitted when
    /// it IS 1 so the serialized shape stays byte-identical to the TS one for
    /// every non-stacked item (which is all of them under the old parser).
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub count: i64,
}

fn one() -> i64 {
    1
}

fn is_one(n: &i64) -> bool {
    *n == 1
}

impl ItemAttributes {
    /// Append a component to `variant`, matching `build_variant`'s `'|'` join so
    /// a lore-derived component and an ExtraAttributes one compose the same way.
    pub fn push_variant(&mut self, part: &str) {
        if self.variant.is_empty() {
            self.variant = part.to_string();
        } else {
            self.variant.push('|');
            self.variant.push_str(part);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Pet {
    #[serde(rename = "type")]
    pub pet_type: String,
    pub tier: String,
    #[serde(default)]
    pub exp: f64,
    #[serde(default)]
    pub candied: bool,
    #[serde(default)]
    pub held_item: Option<String>,
    #[serde(default)]
    pub skin: Option<String>,
}

// ----- JS-coercion helpers (the prismarine-simplify quirks live here) -----

/// `Number(value)` where `value` is prismarine's *simplified* tag. Long → NaN
/// (simplified to a 2-elem array → `Number([hi,lo])` = NaN). Non-scalars → NaN.
/// `tag_num`, except a Long is read as a number instead of NaN.
///
/// ⚠️ Use this ONLY for fields that are genuinely numeric in raw NBT. The NaN in
/// [`tag_num`] is deliberate JS parity, not an oversight: the TS side decoded a
/// Long into a two-int ARRAY, and `Number([hi, lo])` is NaN, which is what the
/// MIDAS_SWORD `hibid=` banding is calibrated against. Widening `tag_num` itself
/// would silently re-band every Midas ever sold.
///
/// Measured 2026-08-08 on 7,747 sold PULSE_RINGs: Hypixel sends `thunder_charge`
/// as an Int, so this changes NOTHING today (variants ''/charge=1/2/3 with
/// medians 2.84M/12.4M/39.5M/106M are already correct). It closes the latent
/// hole: were the field ever to arrive as a Long, `tag_num` would hand back NaN,
/// `pulse_charge_band` would fall through to band 0 by design, and a 106M ring
/// would be priced against the 2.84M bare pool.
fn tag_num_wide(t: &NbtTag) -> f64 {
    if let Some(l) = t.long() {
        return l as f64;
    }
    tag_num(t)
}

fn tag_num(t: &NbtTag) -> f64 {
    if let Some(b) = t.byte() {
        return b as f64;
    }
    if let Some(s) = t.short() {
        return s as f64;
    }
    if let Some(i) = t.int() {
        return i as f64;
    }
    if t.long().is_some() {
        return f64::NAN;
    }
    if let Some(f) = t.float() {
        return f as f64;
    }
    if let Some(d) = t.double() {
        return d;
    }
    if let Some(s) = t.string() {
        return num_from_str(&s.to_str());
    }
    f64::NAN
}

/// `Number(string)` (the subset real data hits): trim, ""→0, else parse or NaN.
fn num_from_str(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() {
        0.0
    } else {
        t.parse::<f64>().unwrap_or(f64::NAN)
    }
}

/// Template-literal `String(value)` coercion for the scalar tags variant/scroll
/// building actually touch.
fn tag_str(t: &NbtTag) -> String {
    if let Some(s) = t.string() {
        return s.to_str().into_owned();
    }
    if let Some(b) = t.byte() {
        return b.to_string();
    }
    if let Some(s) = t.short() {
        return s.to_string();
    }
    if let Some(i) = t.int() {
        return i.to_string();
    }
    if let Some(l) = t.long() {
        // prismarine [high, low].toString() == "high,low"
        return format!("{},{}", (l >> 32) as i32, l as i32);
    }
    if let Some(f) = t.float() {
        return fmt_js_f64(f as f64);
    }
    if let Some(d) = t.double() {
        return fmt_js_f64(d);
    }
    String::new()
}

/// JS truthiness of a tag (strings: non-empty; numbers: non-zero/non-NaN;
/// objects/arrays including Long: truthy).
fn tag_truthy(t: &NbtTag) -> bool {
    if let Some(s) = t.string() {
        return !s.to_str().is_empty();
    }
    if let Some(b) = t.byte() {
        return b != 0;
    }
    if let Some(s) = t.short() {
        return s != 0;
    }
    if let Some(i) = t.int() {
        return i != 0;
    }
    if t.long().is_some() {
        return true; // simplified to an array → truthy object
    }
    if let Some(f) = t.float() {
        return f != 0.0 && !f.is_nan();
    }
    if let Some(d) = t.double() {
        return d != 0.0 && !d.is_nan();
    }
    true // list / compound / arrays → objects → truthy
}

/// JS number `toString`: "3" for 3.0, "3.5" for 3.5, "NaN"/"Infinity" specials.
pub fn fmt_js_f64(x: f64) -> String {
    if x.is_nan() {
        "NaN".to_string()
    } else if x.is_infinite() {
        if x > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else {
        format!("{x}")
    }
}

fn js_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.min(b)
    }
}

// ----- decode entry point -----

/// The `Current weight: N lb` value Hypixel renders into the tooltip.
///
/// ⚠️ Minecraft colour codes are `§` followed by ONE character, and that
/// character is a DIGIT for `§0`-`§9`. The real line is `§7Current weight: §a1
/// lb`, so a naive "collect the digits" reads `§4` as a 4 and would report a 1 lb
/// bass as 41 lb. Every `§` therefore consumes the character after it.
fn parse_lore_weight(line: &str) -> Option<i64> {
    let start = line.find("Current weight:")? + "Current weight:".len();
    let mut digits = String::new();
    let mut chars = line[start..].chars();
    while let Some(c) = chars.next() {
        if c == '§' {
            chars.next();
            continue;
        }
        match c {
            d if d.is_ascii_digit() => digits.push(d),
            // Thousands separator INSIDE the number. Treating it as a terminator
            // read "1,200 lbs" as 1 and dropped the heaviest fish in the game
            // back into the light pool — caught on a live 5,409 lb listing asking
            // 5.3B. Skipped rather than ended.
            ',' | '.' | ' ' if digits.is_empty() => {}
            ',' | '.' => {}
            _ if !digits.is_empty() => break,
            _ => return None,
        }
    }
    digits.parse().ok()
}

/// Coarse weight bands. Deliberately few: heavy items are rare (tens of sales a
/// day against 2,350 for the item overall), and a band that lands under
/// `MIN_REFS` prices nothing at all, which misses the flip just as completely as
/// pooling everything did. Matches the measured ask structure — median ask 21M
/// at 10-49, 85M at 50-99, 146M at 100-199, 970M at 200+.
/// Scan `display.Lore` for the weight line. Only called for allowlisted ids.
fn lore_weight(tag: &NbtCompound) -> Option<i64> {
    let lore = tag.compound("display")?.list("Lore")?;
    for l in lore.strings()?.iter() {
        if let Some(w) = parse_lore_weight(&l.to_str()) {
            return Some(w);
        }
    }
    None
}

fn weight_band(w: i64) -> i64 {
    match w {
        w if w >= 200 => 200,
        w if w >= 100 => 100,
        w if w >= 50 => 50,
        _ => 10,
    }
}

/// Returns `None` on any decode/parse failure or a missing `ExtraAttributes.id`
/// (mirrors the TS `try/catch → null` and `attrsFromExtra` guard).
pub fn decode_item_bytes(item_bytes: &str) -> Option<ItemAttributes> {
    SCRATCH.with(|cell| {
        let s = &mut *cell.borrow_mut();
        let raw = s.inflate_b64(item_bytes)?;
        let mut cursor = Cursor::new(raw);
        let base = match simdnbt::borrow::read(&mut cursor).ok()? {
            simdnbt::borrow::Nbt::Some(b) => b,
            simdnbt::borrow::Nbt::None => return None,
        };
        // .i[0].tag.ExtraAttributes
        let items = base.list("i")?;
        let first = items.compounds()?.first()?;
        let tag = first.compound("tag")?;
        let extra = tag.compound("ExtraAttributes")?;
        let mut a = attrs_from_extra(&extra)?;
        // Count lives on i[0], a level ABOVE the ExtraAttributes this parser was
        // written against, so it needs picking up here rather than in attrs_from_extra.
        if *NBT_COUNT {
            if let Some(c) = first.get("Count").map(|t| tag_num(&t)) {
                if c.is_finite() && c >= 1.0 {
                    a.count = c as i64;
                }
            }
        }
        // Lore-only value drivers. See `LORE_WEIGHT_ITEMS`: the id check runs
        // first so the common item pays one string compare, not a lore walk.
        if !LORE_WEIGHT_ITEMS.is_empty() && LORE_WEIGHT_ITEMS.iter().any(|i| *i == a.id) {
            if let Some(w) = lore_weight(&tag) {
                if w as f64 >= *LORE_WEIGHT_MIN {
                    a.push_variant(&format!("w={}", weight_band(w)));
                }
            }
        }
        Some(a)
    })
}

thread_local! {
    /// One scratch per decoding thread, so the worker pool needs no locking.
    static SCRATCH: std::cell::RefCell<DecodeScratch> = std::cell::RefCell::new(DecodeScratch::new());
}

/// base64 + inflate only, returning the inflated length. Exists so
/// `examples/decode_bench` can attribute cost to the stages of the *current*
/// path rather than re-implementing the old one. Not part of the API.
#[doc(hidden)]
pub fn bench_inflate_len(item_bytes: &str) -> Option<usize> {
    SCRATCH.with(|cell| cell.borrow_mut().inflate_b64(item_bytes).map(|r| r.len()))
}

/// Reusable buffers + one long-lived inflate state for the hot decode path.
///
/// The old code allocated a fresh `Vec` for the base64 output, a fresh `Vec` for
/// the inflated NBT, **and a fresh `GzDecoder`** for every auction. Constructing
/// the decoder dominates: it allocates and initialises a 32KB window and the
/// Huffman tables to inflate a ~1.3KB payload. Measured over a live page 0
/// (`examples/decode_bench`), inflate was 85% of `decode_item_bytes`.
///
/// ⚠️ `Decompress::reset(zlib_header)` maps to `inflateReset2` with windowBits
/// `+15` (zlib) or `-15` (raw) — **never `+31` (gzip)**. So a
/// `Decompress::new_gzip` state cannot be reset and reused; the second call
/// would silently decode as zlib. We therefore strip the RFC 1952 header
/// ourselves and keep a **raw-deflate** state, which resets correctly. The gzip
/// CRC32 trailer is skipped deliberately: a corrupt blob fails the NBT parse
/// immediately after, which is the same `None` the CRC would have produced.
struct DecodeScratch {
    /// base64-decoded bytes (still gzipped).
    gz: Vec<u8>,
    /// inflated NBT.
    raw: Vec<u8>,
    inflate: Decompress,
}

impl DecodeScratch {
    fn new() -> Self {
        Self {
            gz: Vec::with_capacity(4096),
            raw: Vec::with_capacity(8192),
            inflate: Decompress::new(false),
        }
    }

    /// base64 → gunzip, into the reusable buffers. Returns the inflated bytes.
    fn inflate_b64(&mut self, item_bytes: &str) -> Option<&[u8]> {
        let need = base64::decoded_len_estimate(item_bytes.len());
        if self.gz.len() < need {
            self.gz.resize(need, 0);
        }
        let n = STANDARD
            .decode_slice(item_bytes.as_bytes(), &mut self.gz)
            .ok()?;
        let buf = &self.gz[..n];

        // Not gzipped: some payloads are bare NBT. Copy through unchanged.
        if !(buf.len() >= 2 && buf[0] == 0x1f && buf[1] == 0x8b) {
            self.raw.clear();
            self.raw.extend_from_slice(buf);
            return Some(&self.raw);
        }

        let start = gzip_body_offset(buf)?;
        // ISIZE, the last 4 bytes, is the exact inflated length — so the output
        // buffer is sized once and never grows mid-inflate.
        let isize_le = u32::from_le_bytes(buf[buf.len() - 4..].try_into().ok()?) as usize;
        let want = if isize_le > 0 && isize_le <= 1 << 22 {
            isize_le
        } else {
            buf.len() * 4
        };
        self.raw.clear();
        self.raw.reserve(want);

        self.inflate.reset(false);
        let body = &self.gz[start..n];
        match self
            .inflate
            .decompress_vec(body, &mut self.raw, FlushDecompress::Finish)
        {
            Ok(Status::StreamEnd) => Some(&self.raw),
            // ISIZE lied (or the payload is not a single deflate block): finish
            // the stream by growing the buffer rather than dropping the auction.
            Ok(Status::Ok) | Ok(Status::BufError) => {
                for _ in 0..8 {
                    self.raw.reserve(self.raw.capacity().max(4096));
                    let consumed = self.inflate.total_in() as usize;
                    match self.inflate.decompress_vec(
                        &body[consumed.min(body.len())..],
                        &mut self.raw,
                        FlushDecompress::Finish,
                    ) {
                        Ok(Status::StreamEnd) => return Some(&self.raw),
                        Ok(_) => continue,
                        Err(_) => return None,
                    }
                }
                None
            }
            Err(_) => None,
        }
    }
}

/// Offset of the deflate stream inside a gzip member (RFC 1952 §2.3): the fixed
/// 10-byte header plus whichever optional fields FLG advertises.
fn gzip_body_offset(buf: &[u8]) -> Option<usize> {
    if buf.len() < 18 || buf[2] != 8 {
        return None; // too short to hold header+trailer, or not DEFLATE
    }
    let flg = buf[3];
    let mut i = 10usize;
    if flg & 0b0000_0100 != 0 {
        // FEXTRA: 2-byte little-endian length, then that many bytes
        let xlen = u16::from_le_bytes(buf.get(i..i + 2)?.try_into().ok()?) as usize;
        i += 2 + xlen;
    }
    if flg & 0b0000_1000 != 0 {
        // FNAME: NUL-terminated
        i += buf.get(i..)?.iter().position(|&b| b == 0)? + 1;
    }
    if flg & 0b0001_0000 != 0 {
        // FCOMMENT: NUL-terminated
        i += buf.get(i..)?.iter().position(|&b| b == 0)? + 1;
    }
    if flg & 0b0000_0010 != 0 {
        i += 2; // FHCRC
    }
    if i + 8 > buf.len() {
        return None;
    }
    Some(i)
}

/// numMap: object of numbers → { key.toLowerCase(): Number(v) } keeping finite.
fn num_map(obj: Option<NbtCompound>) -> IndexMap<String, f64> {
    let mut out = IndexMap::new();
    if let Some(c) = obj {
        for (k, v) in c.iter() {
            let n = tag_num(&v);
            if n.is_finite() {
                out.insert(k.to_str().to_lowercase(), n);
            }
        }
    }
    out
}

const QUALITIES: [&str; 5] = ["ROUGH", "FLAWED", "FINE", "FLAWLESS", "PERFECT"];

/// String ExtraAttributes keys that name a distinct GOOD rather than an upgrade,
/// gated behind `NBT_EXTRA_FIELDS`. Both were found by walking a live dump with
/// `examples/nbt_probe.rs`: `model` on ABICASE (SUMSUNG_1/SUMSUNG_2/BLUE_BLUE/…)
/// and `dye_donated` on BUCKET_OF_DYE. Add to this list only after the probe
/// shows the values actually price apart (`nbt_probe --item X --group KEY`) --
/// a field that does not move the price only fragments pools.
const EXTRA_IDENTITY_FIELDS: [&str; 2] = ["model", "dye_donated"];

/// Every ExtraAttributes key this parser reads. Anything outside this set is
/// invisible to pricing no matter how much it moves the price.
///
/// ⚠️ This is the ONE definition. `examples/nbt_probe.rs` used to keep a private
/// copy with a "keep in step with nbt.rs" comment on it, and that is exactly how
/// `additional_coins` stayed hidden: the probe's job is to report unread fields,
/// and a hand-synced copy of the read-set silently mis-reports them. When you
/// teach the parser a new key, add it here and both the probe and
/// [`NBT_AUTO_FIELDS`] follow automatically.
pub const READ_KEYS: &[&str] = &[
    // attrs_from_extra
    "id",
    "attributes",
    "enchantments",
    "ability_scroll",
    "art_of_war_count",
    "artOfPeaceApplied",
    "art_of_peace_count",
    "ethermerge",
    "wood_singularity_count",
    "power_ability_scroll",
    "hot_potato_count",
    "is_shiny",
    "jalapeno_count",
    "mana_disintegrator_count",
    "tuned_transmission",
    "transmission_tuner_count",
    "sil_ex",
    "sil_ex_count",
    "farming_for_dummies_count",
    "polarvoid_byte",
    "polarvoid_book_count",
    "wet_book_count",
    "divan_powder_coating",
    "talisman_enrichment",
    "gems",
    "drill_part_engine",
    "drill_part_fuel_tank",
    "drill_part_upgrade_module",
    "petInfo",
    "modifier",
    "skin",
    "rarity_upgrades",
    "upgrade_level",
    "dungeon_item_level",
    "uuid",
    // build_variant
    "potion",
    "potion_type",
    "potion_name",
    "potion_level",
    "splash",
    "runes",
    "new_years_cake",
    "party_hat_color",
    "party_hat_year",
    "dye_item",
    "winning_bid",
    // Read with `winning_bid` when MIDAS_TOTAL_COINS is on. Listed unconditionally
    // so the probe stops reporting it once the flag ships.
    "additional_coins",
    // Likewise for PULSE_CHARGE_BAND.
    "thunder_charge",
    // Likewise for CROWN_COINS_BAND.
    "collected_coins",
    // Read only under NBT_EXTRA_FIELDS, but never a blind spot either way.
    "model",
    "dye_donated",
];

/// Keys [`NBT_AUTO_FIELDS`] must never turn into a feature.
///
/// These are all but unique per item, so as features they are worthless AND
/// costly: every one would be seen once, fail `MIN_FEATURE_SAMPLES`, and be
/// dropped — after being allocated, hashed and carried on every item in every
/// sweep. Significance would reject them correctly; the deny-list is to avoid
/// paying for the rejection 40,000 times a minute.
///
/// ⚠️ Identity, not worthlessness, is the criterion. Do NOT extend this with
/// fields you merely doubt: doubt is what the significance pass is for, and
/// [[finder-unread-nbt-fields-ranked]] records that human ranking of these keys
/// is unreliable in both directions (`timestamp` ranked #1 and is worthless;
/// `collected_coins` ranked 18th and was the biggest pricing win of the month).
const AUTO_FIELD_DENYLIST: &[&str] = &[
    "uid",
    "uuid",
    "timestamp",
    "spawnedFor",
    "bossId",
    "originTag",
    "donated_museum",
];

/// Port of `attrsFromExtra`.
pub fn attrs_from_extra(extra: &NbtCompound) -> Option<ItemAttributes> {
    // guard: `typeof extra.id !== 'string'` → null
    let id = extra.string("id")?.to_str().into_owned();

    let num0 = |k: &str| -> f64 { extra.get(k).map(|t| tag_num(&t)).unwrap_or(0.0) };
    let num_chain = |keys: &[&str]| -> f64 {
        for k in keys {
            if let Some(t) = extra.get(k) {
                return tag_num(&t);
            }
        }
        0.0
    };
    let cstr = |k: &str| -> Option<String> { extra.string(k).map(|s| s.to_str().into_owned()) };

    let attributes = num_map(extra.compound("attributes"));
    let enchantments = num_map(extra.compound("enchantments"));

    let scrolls: Vec<String> = extra
        .list("ability_scroll")
        .and_then(|l| l.strings())
        .map(|ss| {
            let mut v: Vec<String> = ss.iter().map(|s| s.to_str().to_uppercase()).collect();
            v.sort();
            v
        })
        .unwrap_or_default();

    // ----- extras ----- (IndexMap: insertion order matches the TS build order)
    let mut extras: IndexMap<String, f64> = IndexMap::new();
    if num0("art_of_war_count") > 0.0 {
        extras.insert("art_of_war".into(), 1.0);
    }
    if num_chain(&["artOfPeaceApplied", "art_of_peace_count"]) > 0.0 {
        extras.insert("art_of_peace".into(), 1.0);
    }
    if num0("ethermerge") > 0.0 {
        extras.insert("ethermerge".into(), 1.0);
    }
    if num0("wood_singularity_count") > 0.0 {
        extras.insert("wood_singularity".into(), 1.0);
    }
    if let Some(s) = cstr("power_ability_scroll") {
        extras.insert(format!("power_{}", s.to_lowercase()), 1.0);
    }
    let hpc = num0("hot_potato_count");
    if hpc > 0.0 {
        extras.insert("hpb".into(), hpc.min(10.0));
    }
    if hpc > 10.0 {
        extras.insert("fuming".into(), hpc - 10.0);
    }
    if num0("is_shiny") > 0.0 {
        extras.insert("shiny".into(), 1.0);
    }
    let counted: [(&str, f64); 8] = [
        ("jalapeno", num0("jalapeno_count")),
        ("mana_disintegrator", num0("mana_disintegrator_count")),
        (
            "tuner",
            num_chain(&["tuned_transmission", "transmission_tuner_count"]),
        ),
        ("silex", num_chain(&["sil_ex", "sil_ex_count"])),
        ("ffd", num0("farming_for_dummies_count")),
        (
            "polarvoid",
            num_chain(&["polarvoid_byte", "polarvoid_book_count"]),
        ),
        ("wet_book", num0("wet_book_count")),
        ("divan_powder", num0("divan_powder_coating")),
    ];
    for (name, n) in counted {
        if n > 0.0 {
            extras.insert(name.into(), n);
        }
    }
    if let Some(s) = cstr("talisman_enrichment") {
        extras.insert(format!("enrich_{}", s.to_lowercase()), 1.0);
    }
    // Identity fields the parser never read. Unlike everything above these do not
    // say how UPGRADED an item is, they say WHICH item it is -- and two items
    // that are not the same thing were sharing one median. Same string-to-extras
    // idiom as power_/enrich_ above, so they flow through significance unchanged.
    if *NBT_EXTRA_FIELDS {
        for f in EXTRA_IDENTITY_FIELDS {
            if let Some(s) = cstr(f) {
                extras.insert(format!("{f}_{}", s.to_lowercase()), 1.0);
            }
        }
    }
    // The generalisation of the block above: stop deciding by hand which keys are
    // worth reading, emit every unread one, and let significance judge each. Same
    // idiom, same `extras` (never the base key), so a worthless field is a no-op.
    if *NBT_AUTO_FIELDS {
        for (k, v) in extra.iter() {
            let k = k.to_str();
            if READ_KEYS.contains(&k.as_ref()) || AUTO_FIELD_DENYLIST.contains(&k.as_ref()) {
                continue;
            }
            if let Some(feat) = auto_field_feature(&k, &v) {
                extras.insert(feat, 1.0);
            }
        }
    }
    // Band 0 inserts nothing, so a crown with no coins consumed keeps the exact
    // feature set it has today. tag_num_wide, not tag_num: the counter reaches
    // 1e9 and a Long must not read as NaN and collapse to band 0.
    if *CROWN_COINS_BAND {
        if let Some(t) = extra.get("collected_coins") {
            let band = collected_coins_band(tag_num_wide(&t));
            if band > 0 {
                extras.insert(format!("crown_coins_{band}"), 1.0);
            }
        }
    }

    // ----- gems -----
    let mut gem_slots: i64 = 0;
    let mut gems: Vec<String> = Vec::new();
    if let Some(g) = extra.compound("gems") {
        if let Some(slots) = g.list("unlocked_slots") {
            // unlocked_slots is a list of slot-name strings
            gem_slots = slots.strings().map(|s| s.len()).unwrap_or(0) as i64;
        } else {
            gem_slots = g
                .iter()
                .filter(|(k, _)| {
                    let ks = k.to_str();
                    !ks.ends_with("_gem") && ks != "unlocked_slots"
                })
                .count() as i64;
        }
        for (k, v) in g.iter() {
            let ks = k.to_str();
            if ks == "unlocked_slots" || ks.ends_with("_gem") {
                continue;
            }
            let quality = if let Some(s) = v.string() {
                s.to_str().into_owned()
            } else if let Some(c) = v.compound() {
                c.string("quality")
                    .map(|s| s.to_str().into_owned())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            if !QUALITIES.contains(&quality.as_str()) {
                continue;
            }
            let gem_type = match g.string(&format!("{ks}_gem")) {
                Some(t) => t.to_str().into_owned(),
                None => strip_trailing_index(&ks).to_string(),
            };
            gems.push(format!("{quality}_{gem_type}").to_uppercase());
        }
        gems.sort();
    }

    // ----- drill parts -----
    let mut parts: Vec<String> = Vec::new();
    for f in [
        "drill_part_engine",
        "drill_part_fuel_tank",
        "drill_part_upgrade_module",
    ] {
        if let Some(s) = cstr(f) {
            parts.push(s.to_uppercase());
        }
    }
    parts.sort();

    // ----- pet -----
    let mut pet: Option<Pet> = None;
    if id == "PET" {
        if let Some(pi) = cstr("petInfo") {
            if let Ok(p) = serde_json::from_str::<serde_json::Value>(&pi) {
                if let (Some(ptype), Some(tier)) = (
                    p.get("type").and_then(|x| x.as_str()),
                    p.get("tier").and_then(|x| x.as_str()),
                ) {
                    let raw_exp = json_num(p.get("exp"));
                    let exp = if raw_exp == 0.0 || raw_exp.is_nan() {
                        0.0
                    } else {
                        raw_exp
                    };
                    pet = Some(Pet {
                        pet_type: ptype.to_string(),
                        tier: tier.to_string(),
                        exp,
                        candied: json_num(p.get("candyUsed")) > 0.0,
                        held_item: p
                            .get("heldItem")
                            .and_then(|x| x.as_str())
                            .map(str::to_string),
                        skin: p.get("skin").and_then(|x| x.as_str()).map(str::to_string),
                    });
                }
            }
        }
    }

    Some(ItemAttributes {
        id,
        attributes,
        enchantments,
        scrolls,
        reforge: cstr("modifier").map(|s| s.to_lowercase()),
        skin: cstr("skin"),
        recombobulated: num0("rarity_upgrades") > 0.0,
        upgrade_level: extra
            .get("upgrade_level")
            .map(|t| tag_num(&t))
            .or_else(|| extra.get("dungeon_item_level").map(|t| tag_num(&t))),
        gem_slots,
        gems,
        parts,
        extras,
        pet,
        variant: build_variant(extra),
        item_uuid: cstr("uuid").filter(|s| !s.is_empty()),
        // Set by decode_item_bytes, which is the only caller that can see i[0].
        count: 1,
    })
}

/// `${first ?? second ?? ''}` for template building.
fn coerce_or(extra: &NbtCompound, keys: &[&str]) -> String {
    for k in keys {
        if let Some(t) = extra.get(k) {
            return tag_str(&t);
        }
    }
    String::new()
}

/// Band a PULSE_RING's `thunder_charge` by the upgrade threshold it has PASSED.
///
/// Banding, not the raw value: the price steps at the thresholds, while raw
/// charge fragments the pool into singletons (charge 1,450,000 has n=1 over 1000
/// sales). Thresholds and the evidence are documented on [`PULSE_CHARGE_BAND`].
///
/// NaN-safe by construction: every `>=` against NaN is false, so an unparseable
/// charge falls through to band 0, the cheapest and therefore safest bucket. Do
/// not "fix" this into a partial_cmp chain.
fn pulse_charge_band(charge: f64) -> u8 {
    if charge >= 5_000_000.0 {
        3
    } else if charge >= 1_000_000.0 {
        2
    } else if charge >= 150_000.0 {
        1
    } else {
        0
    }
}

/// Band `collected_coins` by DIGIT COUNT, which is the item's own mechanic:
/// a Crown of Avarice grants damage and Magic Find "for each digit of Coins
/// consumed", and its perk changes at 1B (where the counter caps). Evidence and
/// the reason this is an `extras` feature rather than a base-key part are on
/// [`CROWN_COINS_BAND`].
///
/// Returns 0 for "no feature": zero coins, negatives, and NaN alike. NaN-safe by
/// construction — `v >= 1.0` is false for NaN, so an unparseable value takes the
/// band-0 branch and keys exactly as an untouched crown does. Do not "fix" this
/// into a partial_cmp chain.
fn collected_coins_band(v: f64) -> u32 {
    if !(v >= 1.0) {
        return 0;
    }
    // Digits of the integer part, saturating at the 1e9 cap (10 digits). Counted
    // by division rather than log10 so no float rounding can put 999_999_999 in
    // the capped band.
    let mut n = v.min(1e9) as u64;
    let mut digits = 0;
    while n > 0 {
        digits += 1;
        n /= 10;
    }
    digits
}

fn build_variant(extra: &NbtCompound) -> String {
    let mut parts: Vec<String> = Vec::new();

    let potion_truthy = ["potion", "potion_type", "potion_name"]
        .iter()
        .any(|k| extra.get(k).map(|t| tag_truthy(&t)).unwrap_or(false));
    if potion_truthy {
        let name = coerce_or(extra, &["potion", "potion_name"]);
        let level = coerce_or(extra, &["potion_level"]);
        let ptype = coerce_or(extra, &["potion_type"]);
        let splash = if extra.get("splash").map(|t| tag_truthy(&t)).unwrap_or(false) {
            1
        } else {
            0
        };
        parts.push(format!("potion={name}:{level}:{ptype}:{splash}"));
    }

    if let Some(runes) = extra.compound("runes") {
        let mut items: Vec<String> = runes
            .iter()
            .map(|(k, v)| format!("{}{}", k.to_str(), tag_str(&v)))
            .collect();
        items.sort();
        parts.push(format!("rune={}", items.join(",")));
    }

    if let Some(t) = extra.get("new_years_cake") {
        parts.push(format!("cake={}", tag_str(&t)));
    }
    if let Some(color) = extra.get("party_hat_color") {
        let year = coerce_or(extra, &["party_hat_year"]);
        parts.push(format!("hat={}:{}", tag_str(&color), year));
    }
    if let Some(t) = extra.get("dye_item") {
        parts.push(format!("dye={}", tag_str(&t)));
    }
    // Keyed on the field's presence, like cake=/hat=/dye= above, rather than on
    // id == PULSE_RING. The thresholds are calibrated on PULSE_RING, but they are
    // monotonic in charge, so any future item carrying thunder_charge still bands
    // in the right direction instead of pooling flat.
    if *PULSE_CHARGE_BAND {
        if let Some(t) = extra.get("thunder_charge") {
            // tag_num_wide, not tag_num: a Long charge must not read as NaN and
            // collapse to band 0. See tag_num_wide.
            parts.push(format!("charge={}", pulse_charge_band(tag_num_wide(&t))));
        }
    }
    if let Some(t) = extra.get("winning_bid") {
        // Number(winning_bid) is NaN (Long→array); floor(NaN/1e7)=NaN, and NaN<10 is
        // false in JS and Rust alike, so NaN takes the same branch on both sides.
        //
        // A Midas item's stats scale with the TOTAL coins in it, and a top-up after
        // the original auction lands in `additional_coins`, not `winning_bid`. See
        // MIDAS_TOTAL_COINS. Absent field contributes 0, so the band is unchanged
        // for every item that was never topped up. NaN + 0.0 is still NaN, so the
        // JS-parity branch above is untouched too.
        //
        // tag_num_wide for the top-up, tag_num for the bid. The NaN-on-Long in
        // `tag_num` is JS parity that every historical `bid=` label is calibrated
        // against, so `winning_bid` keeps it. `additional_coins` has never been
        // read, so it has NO history to stay parity with, and reading it as NaN
        // would drag the whole sum to NaN and mis-key precisely the topped-up
        // items this exists to fix. Same reasoning as `thunder_charge`.
        let bid = tag_num(&t)
            + if *MIDAS_TOTAL_COINS {
                extra
                    .get("additional_coins")
                    .map_or(0.0, |a| tag_num_wide(&a))
            } else {
                0.0
            };
        let is_sword = extra
            .get("id")
            .map(|t| tag_str(&t) == "MIDAS_SWORD")
            .unwrap_or(false);
        parts.push(midas_bid_token(bid, is_sword));
    }

    parts.join("|")
}

/// Feature name for one auto-ingested numeric field, or `None` for values that
/// must not become features.
///
/// ⚠️ The whole safety of [`NBT_AUTO_FIELDS`] rests here. A feature name derived
/// from a raw number is unique-per-item, which is not "a weak signal" but active
/// harm: it is precisely the key fragmentation that
/// [[finder-notpriceable-is-key-fragmentation]] measured at 51B. So numbers are
/// banded by DIGIT COUNT, the banding that worked for `collected_coins`, which
/// bounds any field to ~10 buckets no matter its scale — a 0-10 counter and a 1e9
/// counter both stay legible.
///
/// Zero and negative return `None`. Zero is the overwhelmingly common value for a
/// counter, so emitting it would put a feature on nearly every item, and a feature
/// carried by everything cannot separate anything (it defines the base, exactly
/// how `reforge:gilded` failed to fork in [`SIG_TWO_SIDED`]).
fn auto_field_num_feature(k: &str, n: f64) -> Option<String> {
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    let band = (n.log10().floor() as i64 + 1).clamp(1, 10);
    Some(format!("nbt_{}_b{band}", k.to_lowercase()))
}

/// Feature name for one auto-ingested string field.
///
/// Values are lowercased and length-capped so a stray long string cannot become a
/// giant feature name. A string that is unique per item still produces a
/// unique feature, which significance drops for want of `MIN_FEATURE_SAMPLES` —
/// the cost is bounded and the payoff is `model`-shaped fields we have not found
/// yet.
fn auto_field_str_feature(k: &str, s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() || s.len() > 48 {
        return None;
    }
    Some(format!("nbt_{}_{}", k.to_lowercase(), s.to_lowercase()))
}

/// [`NBT_AUTO_FIELDS`] feature for one ExtraAttributes tag, NBT path.
///
/// Compounds and lists are skipped: they are either already-read structures
/// (`gems`, `petInfo`) or nested `{id}` duplicates of fields we read, which
/// [[finder-unread-nbt-fields-ranked]] settled as a NON-miss for drill parts. A
/// nested compound needs a deliberate reader, not a generic one.
fn auto_field_feature(k: &str, t: &NbtTag) -> Option<String> {
    if let Some(s) = t.string() {
        return auto_field_str_feature(k, &s.to_str());
    }
    let n = tag_num_wide(t);
    if n.is_finite() {
        return auto_field_num_feature(k, n);
    }
    None
}

/// [`NBT_AUTO_FIELDS`] feature for one ExtraAttributes value, JSON path. Must
/// produce the SAME name as [`auto_field_feature`] for the same logical value.
fn auto_field_feature_json(k: &str, v: &JV) -> Option<String> {
    match v {
        JV::String(s) => auto_field_str_feature(k, s),
        JV::Number(_) | JV::Bool(_) => auto_field_num_feature(k, json_num(Some(v))),
        _ => None,
    }
}

/// The `bid=` / `hibid=` token for a Midas item, given the coin total already
/// summed by the caller.
///
/// Shared by the simdnbt and JSON variant builders so the two cannot drift — they
/// have drifted before, and the goldens do not catch it (the only MIDAS_SWORD
/// fixture is `bid=0`, which every version of this agrees on).
///
/// ⚠️ `bid` is NaN whenever `winning_bid` decoded as a Long, which is deliberate
/// JS parity that the historical labels are calibrated against. Every comparison
/// below is written so NaN takes the same branch JS would.
fn midas_bid_token(bid: f64, is_sword: bool) -> String {
    let band = (bid / 10_000_000.0).floor();
    // NaN is false here, which is what JS `band < 10` yields too, so NaN takes the
    // same branch on both sides. Do not "fix" this into a partial_cmp.
    let is_low_band = band < 10.0;
    if is_sword && !is_low_band {
        format!("hibid={}", fmt_js_f64(js_min(band, 25.0)))
    } else {
        format!("bid={}", fmt_js_f64(js_min(band, 10.0)))
    }
}

/// Strip a trailing `_<digits>` (the JS `/_\d+$/` replace).
fn strip_trailing_index(k: &str) -> &str {
    if let Some(pos) = k.rfind('_') {
        let (head, tail) = k.split_at(pos);
        if tail.len() > 1 && tail[1..].bytes().all(|b| b.is_ascii_digit()) {
            return head;
        }
    }
    k
}

/// `Number(x)` for a JSON value from `JSON.parse(petInfo)`.
fn json_num(v: Option<&serde_json::Value>) -> f64 {
    match v {
        None | Some(serde_json::Value::Null) => 0.0,
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(serde_json::Value::String(s)) => num_from_str(s),
        Some(serde_json::Value::Bool(b)) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Some(_) => f64::NAN,
    }
}

// ===================================================================
// JSON path — `attrsFromInventorySlot` (nbt.ts:231) + a JSON mirror of
// `attrsFromExtra`. The bot serialises live NBT to JSON (numbers lose their
// byte/int/long tag), so this reads a `serde_json::Value` instead of a simdnbt
// compound. Coercions mirror what TS `attrsFromExtra` does when fed a JS object:
// `Number(x)` = [`json_num`], `String(x)` = [`json_str_coerce`], JS truthiness =
// [`json_truthy`]. Kept SEPARATE from the simdnbt path so the 929-case nbt golden
// (item_bytes decode) is untouched; a dedicated inventory golden proves this path.
// ===================================================================

use serde_json::Value as JV;

/// JS `String(value)` for a JSON value (the subset build_variant touches).
fn json_str_coerce(v: &JV) -> String {
    match v {
        JV::String(s) => s.clone(),
        JV::Number(n) => fmt_js_f64(n.as_f64().unwrap_or(f64::NAN)),
        JV::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        JV::Null => "null".to_string(),
        // JS `String([hi,lo])` == "hi,lo"; nested arrays/objects join with ','.
        JV::Array(a) => a.iter().map(json_str_coerce).collect::<Vec<_>>().join(","),
        JV::Object(_) => "[object Object]".to_string(),
    }
}

/// JS truthiness of a JSON value (string: non-empty; number: non-zero/non-NaN;
/// null/false: false; array/object: truthy).
fn json_truthy(v: &JV) -> bool {
    match v {
        JV::String(s) => !s.is_empty(),
        JV::Number(n) => {
            let f = n.as_f64().unwrap_or(f64::NAN);
            f != 0.0 && !f.is_nan()
        }
        JV::Bool(b) => *b,
        JV::Null => false,
        JV::Array(_) | JV::Object(_) => true,
    }
}

/// `numMap` for a JSON object: `{ key.toLowerCase(): Number(v) }`, finite only.
fn json_num_map(obj: Option<&JV>) -> IndexMap<String, f64> {
    let mut out = IndexMap::new();
    if let Some(JV::Object(m)) = obj {
        for (k, v) in m {
            let n = json_num(Some(v));
            if n.is_finite() {
                out.insert(k.to_lowercase(), n);
            }
        }
    }
    out
}

/// `${first ?? second ?? ''}` (JSON): first present key's `String()` coercion.
fn json_coerce_or(extra: &JV, keys: &[&str]) -> String {
    for k in keys {
        if let Some(v) = extra.get(k) {
            return json_str_coerce(v);
        }
    }
    String::new()
}

fn json_build_variant(extra: &JV) -> String {
    let mut parts: Vec<String> = Vec::new();

    let potion_truthy = ["potion", "potion_type", "potion_name"]
        .iter()
        .any(|k| extra.get(k).map(json_truthy).unwrap_or(false));
    if potion_truthy {
        let name = json_coerce_or(extra, &["potion", "potion_name"]);
        let level = json_coerce_or(extra, &["potion_level"]);
        let ptype = json_coerce_or(extra, &["potion_type"]);
        let splash = if extra.get("splash").map(json_truthy).unwrap_or(false) {
            1
        } else {
            0
        };
        parts.push(format!("potion={name}:{level}:{ptype}:{splash}"));
    }

    if let Some(JV::Object(runes)) = extra.get("runes") {
        let mut items: Vec<String> = runes
            .iter()
            .map(|(k, v)| format!("{}{}", k, json_str_coerce(v)))
            .collect();
        items.sort();
        parts.push(format!("rune={}", items.join(",")));
    }

    if let Some(v) = extra.get("new_years_cake") {
        parts.push(format!("cake={}", json_str_coerce(v)));
    }
    if let Some(color) = extra.get("party_hat_color") {
        let year = json_coerce_or(extra, &["party_hat_year"]);
        parts.push(format!("hat={}:{}", json_str_coerce(color), year));
    }
    if let Some(v) = extra.get("dye_item") {
        parts.push(format!("dye={}", json_str_coerce(v)));
    }
    // Same position and same band as `build_variant`, so the NBT and JSON paths
    // produce byte-identical variant strings for the same ring.
    if *PULSE_CHARGE_BAND {
        if let Some(v) = extra.get("thunder_charge") {
            parts.push(format!("charge={}", pulse_charge_band(json_num(Some(v)))));
        }
    }
    if let Some(v) = extra.get("winning_bid") {
        // Mirror of `build_variant`: total coins, not just the closing bid. See
        // MIDAS_TOTAL_COINS. `json_num` already reads a JSON number or numeric
        // string at full width, so there is no tag_num/tag_num_wide split here.
        let bid = json_num(Some(v))
            + if *MIDAS_TOTAL_COINS {
                extra
                    .get("additional_coins")
                    .map_or(0.0, |a| json_num(Some(a)))
            } else {
                0.0
            };
        let is_sword = extra
            .get("id")
            .map(|v| json_str_coerce(v) == "MIDAS_SWORD")
            .unwrap_or(false);
        parts.push(midas_bid_token(bid, is_sword));
    }

    parts.join("|")
}

/// JSON mirror of [`attrs_from_extra`]. `extra` is the ExtraAttributes object.
pub fn attrs_from_extra_json(extra: &JV) -> Option<ItemAttributes> {
    let id = extra.get("id").and_then(|v| v.as_str())?.to_string();

    let num0 = |k: &str| -> f64 { json_num(extra.get(k)) };
    let num_chain = |keys: &[&str]| -> f64 {
        for k in keys {
            if extra.get(k).is_some() {
                return json_num(extra.get(k));
            }
        }
        0.0
    };
    let cstr =
        |k: &str| -> Option<String> { extra.get(k).and_then(|v| v.as_str()).map(str::to_string) };

    let attributes = json_num_map(extra.get("attributes"));
    let enchantments = json_num_map(extra.get("enchantments"));

    let scrolls: Vec<String> = match extra.get("ability_scroll") {
        Some(JV::Array(a)) => {
            let mut v: Vec<String> = a
                .iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.to_uppercase())
                .collect();
            v.sort();
            v
        }
        _ => Vec::new(),
    };

    let mut extras: IndexMap<String, f64> = IndexMap::new();
    if num0("art_of_war_count") > 0.0 {
        extras.insert("art_of_war".into(), 1.0);
    }
    if num_chain(&["artOfPeaceApplied", "art_of_peace_count"]) > 0.0 {
        extras.insert("art_of_peace".into(), 1.0);
    }
    if num0("ethermerge") > 0.0 {
        extras.insert("ethermerge".into(), 1.0);
    }
    if num0("wood_singularity_count") > 0.0 {
        extras.insert("wood_singularity".into(), 1.0);
    }
    if let Some(s) = cstr("power_ability_scroll") {
        extras.insert(format!("power_{}", s.to_lowercase()), 1.0);
    }
    let hpc = num0("hot_potato_count");
    if hpc > 0.0 {
        extras.insert("hpb".into(), hpc.min(10.0));
    }
    if hpc > 10.0 {
        extras.insert("fuming".into(), hpc - 10.0);
    }
    if num0("is_shiny") > 0.0 {
        extras.insert("shiny".into(), 1.0);
    }
    let counted: [(&str, f64); 8] = [
        ("jalapeno", num0("jalapeno_count")),
        ("mana_disintegrator", num0("mana_disintegrator_count")),
        (
            "tuner",
            num_chain(&["tuned_transmission", "transmission_tuner_count"]),
        ),
        ("silex", num_chain(&["sil_ex", "sil_ex_count"])),
        ("ffd", num0("farming_for_dummies_count")),
        (
            "polarvoid",
            num_chain(&["polarvoid_byte", "polarvoid_book_count"]),
        ),
        ("wet_book", num0("wet_book_count")),
        ("divan_powder", num0("divan_powder_coating")),
    ];
    for (name, n) in counted {
        if n > 0.0 {
            extras.insert(name.into(), n);
        }
    }
    if let Some(s) = cstr("talisman_enrichment") {
        extras.insert(format!("enrich_{}", s.to_lowercase()), 1.0);
    }
    // Mirror of the NBT path (see EXTRA_IDENTITY_FIELDS). The two must agree or a
    // held item keys differently from the same item on the AH.
    if *NBT_EXTRA_FIELDS {
        for f in EXTRA_IDENTITY_FIELDS {
            if let Some(s) = cstr(f) {
                extras.insert(format!("{f}_{}", s.to_lowercase()), 1.0);
            }
        }
    }
    // Mirror of the NBT path (see NBT_AUTO_FIELDS). Feature NAMES must match the
    // NBT path byte for byte, or an item we hold prices off a different feature
    // set than the same item on the AH.
    if *NBT_AUTO_FIELDS {
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                if READ_KEYS.contains(&k.as_str()) || AUTO_FIELD_DENYLIST.contains(&k.as_str()) {
                    continue;
                }
                if let Some(feat) = auto_field_feature_json(k, v) {
                    extras.insert(feat, 1.0);
                }
            }
        }
    }
    // Mirror of the NBT path (see CROWN_COINS_BAND). json_num already widens, and
    // Hypixel serves this field as a bare number here and as a Long over NBT.
    if *CROWN_COINS_BAND {
        if let Some(v) = extra.get("collected_coins") {
            let band = collected_coins_band(json_num(Some(v)));
            if band > 0 {
                extras.insert(format!("crown_coins_{band}"), 1.0);
            }
        }
    }

    // ----- gems -----
    let mut gem_slots: i64 = 0;
    let mut gems: Vec<String> = Vec::new();
    if let Some(JV::Object(g)) = extra.get("gems") {
        if let Some(JV::Array(slots)) = g.get("unlocked_slots") {
            gem_slots = slots.iter().filter(|s| s.is_string()).count() as i64;
        } else {
            gem_slots = g
                .iter()
                .filter(|(k, _)| !k.ends_with("_gem") && k.as_str() != "unlocked_slots")
                .count() as i64;
        }
        for (k, v) in g.iter() {
            if k == "unlocked_slots" || k.ends_with("_gem") {
                continue;
            }
            let quality = match v {
                JV::String(s) => s.clone(),
                JV::Object(_) => v
                    .get("quality")
                    .and_then(|q| q.as_str())
                    .unwrap_or("")
                    .to_string(),
                _ => String::new(),
            };
            if !QUALITIES.contains(&quality.as_str()) {
                continue;
            }
            let gem_type = match g.get(&format!("{k}_gem")).and_then(|t| t.as_str()) {
                Some(t) => t.to_string(),
                None => strip_trailing_index(k).to_string(),
            };
            gems.push(format!("{quality}_{gem_type}").to_uppercase());
        }
        gems.sort();
    }

    // ----- drill parts -----
    let mut parts: Vec<String> = Vec::new();
    for f in [
        "drill_part_engine",
        "drill_part_fuel_tank",
        "drill_part_upgrade_module",
    ] {
        if let Some(s) = cstr(f) {
            parts.push(s.to_uppercase());
        }
    }
    parts.sort();

    // ----- pet ----- (petInfo is a JSON string in both paths → identical)
    let mut pet: Option<Pet> = None;
    if id == "PET" {
        if let Some(pi) = cstr("petInfo") {
            if let Ok(p) = serde_json::from_str::<serde_json::Value>(&pi) {
                if let (Some(ptype), Some(tier)) = (
                    p.get("type").and_then(|x| x.as_str()),
                    p.get("tier").and_then(|x| x.as_str()),
                ) {
                    let raw_exp = json_num(p.get("exp"));
                    let exp = if raw_exp == 0.0 || raw_exp.is_nan() {
                        0.0
                    } else {
                        raw_exp
                    };
                    pet = Some(Pet {
                        pet_type: ptype.to_string(),
                        tier: tier.to_string(),
                        exp,
                        candied: json_num(p.get("candyUsed")) > 0.0,
                        held_item: p
                            .get("heldItem")
                            .and_then(|x| x.as_str())
                            .map(str::to_string),
                        skin: p.get("skin").and_then(|x| x.as_str()).map(str::to_string),
                    });
                }
            }
        }
    }

    Some(ItemAttributes {
        id,
        attributes,
        enchantments,
        scrolls,
        reforge: cstr("modifier").map(|s| s.to_lowercase()),
        skin: cstr("skin"),
        recombobulated: num0("rarity_upgrades") > 0.0,
        upgrade_level: if extra.get("upgrade_level").is_some() {
            Some(json_num(extra.get("upgrade_level")))
        } else if extra.get("dungeon_item_level").is_some() {
            Some(json_num(extra.get("dungeon_item_level")))
        } else {
            None
        },
        gem_slots,
        gems,
        parts,
        extras,
        pet,
        variant: json_build_variant(extra),
        item_uuid: cstr("uuid").filter(|s| !s.is_empty()),
        // Filled in by attrs_from_inventory_slot, which can see the slot itself.
        count: 1,
    })
}

/// Port of `attrsFromInventorySlot` (nbt.ts:231). Navigates one inventory-slot
/// JSON (`slot.nbt`) to its ExtraAttributes and runs [`attrs_from_extra_json`].
/// `custom = nbt["minecraft:custom_data"] ?? nbt.tag ?? nbt`;
/// `extra  = custom.ExtraAttributes ?? custom.nbt.ExtraAttributes ?? custom`.
pub fn attrs_from_inventory_slot(slot: &JV) -> Option<ItemAttributes> {
    let n = slot.get("nbt")?;
    let custom = n
        .get("minecraft:custom_data")
        .or_else(|| n.get("tag"))
        .unwrap_or(n);
    let extra = custom
        .get("ExtraAttributes")
        .or_else(|| custom.get("nbt").and_then(|x| x.get("ExtraAttributes")))
        .unwrap_or(custom);
    let mut a = attrs_from_extra_json(extra)?;
    // The held-item mirror of i[0].Count. The mod's slot shape is not pinned by a
    // golden the way item_bytes is, so accept either spelling; absent stays 1,
    // which is what every slot in goldens/inventorySlot has.
    if *NBT_COUNT {
        for k in ["count", "Count"] {
            if let Some(c) = slot.get(k).or_else(|| n.get(k)).map(|v| json_num(Some(v))) {
                if c.is_finite() && c >= 1.0 {
                    a.count = c as i64;
                    break;
                }
            }
        }
    }
    Some(a)
}

#[cfg(test)]
mod bid_banding_tests {
    //! Midas' Sword bid banding — the Rust mirror of TS `npm run test:midas`
    //! (baf-flip-finder/src/midasBid.test.ts). Keep the two in step.
    //!
    //! The goldens CANNOT cover this: the only MIDAS_SWORD in goldens/nbt is
    //! `bid=0`, so every fixture passes under both the old cap-at-10 banding and
    //! the new one. This test is the only thing that catches TS/Rust drift here.
    use super::*;
    use simdnbt::owned;

    /// Builds real item_bytes in the shape decode_item_bytes expects
    /// (root.i[0].tag.ExtraAttributes), so this drives the ACTUAL auction decode
    /// path (simdnbt build_variant), not just the JSON/inventory mirror.
    fn item_bytes(id: &str, bid: Option<owned::NbtTag>) -> String {
        let mut extra = owned::NbtCompound::new();
        extra.insert("id", id);
        if let Some(t) = bid {
            extra.insert("winning_bid", t);
        }
        let mut tag = owned::NbtCompound::new();
        tag.insert("ExtraAttributes", extra);
        let mut item = owned::NbtCompound::new();
        item.insert("tag", tag);
        let mut root = owned::NbtCompound::new();
        root.insert("i", owned::NbtList::Compound(vec![item]));
        let mut buf = Vec::new();
        owned::BaseNbt::new("", root).write(&mut buf);
        STANDARD.encode(&buf)
    }

    fn variant(id: &str, bid: i32) -> String {
        decode_item_bytes(&item_bytes(id, Some(owned::NbtTag::Int(bid))))
            .expect("decodes")
            .variant
    }

    /// The token half of MIDAS_TOTAL_COINS. `midas_bid_token` takes the coin total
    /// already summed, so this pins the banding without needing the env flag (no
    /// test in this crate can set a LazyLock-backed flag; the summing half is
    /// validated by backtest on the box).
    #[test]
    fn total_coins_moves_a_topped_up_sword_off_band_0() {
        // Auction 9e1fbca293774f5185015c9907c4fdc8, a Withered Midas' Sword listed
        // at 10.0M on 2026-08-14:
        //   winning_bid       8_930_000
        //   additional_coins 41_070_000  -> total exactly 50_000_000
        // Reading only the closing bid put a maxed 50M sword in `bid=0`, the
        // cheapest bucket in the game, and the finder rejected it at 60% conf.
        assert_eq!(midas_bid_token(8_930_000.0, true), "bid=0");
        assert_eq!(midas_bid_token(8_930_000.0 + 41_070_000.0, true), "bid=5");

        // The whole safety argument for re-using `bid=` instead of minting a new
        // token: an item with no top-up sums to the same number and therefore keys
        // byte-identically, so existing refs keep matching.
        assert_eq!(
            midas_bid_token(8_930_000.0 + 0.0, true),
            midas_bid_token(8_930_000.0, true)
        );

        // Top-ups must still be able to reach the sword-only `hibid=` branch, and
        // must still clamp at the 250M stat cap rather than running away.
        assert_eq!(
            midas_bid_token(60_000_000.0 + 60_000_000.0, true),
            "hibid=12"
        );
        assert_eq!(midas_bid_token(400_000_000.0, true), "hibid=25");
        // Non-swords never reach `hibid=` and stay capped at 10 (unchanged).
        assert_eq!(midas_bid_token(400_000_000.0, false), "bid=10");
    }

    /// NBT_AUTO_FIELDS is only safe because it cannot mint a feature per item.
    /// These pin the properties that safety rests on. The name builders are pure,
    /// so they need no env flag (see the note on the token test above).
    #[test]
    fn auto_fields_cannot_fragment_a_pool() {
        // Digit-count banding: a field is bounded to ~10 buckets whatever its
        // scale, so neither a small counter nor a 1e9 one explodes the key space.
        let bands: std::collections::HashSet<_> = (0..40)
            .map(|i| 1.7_f64.powi(i))
            .filter_map(|n| auto_field_num_feature("eman_kills", n))
            .collect();
        assert!(
            bands.len() <= 10,
            "digit banding must bound a field to ~10 buckets, got {}",
            bands.len()
        );

        // Adjacent magnitudes must actually separate, or the banding is useless.
        assert_eq!(
            auto_field_num_feature("eman_kills", 950.0).unwrap(),
            "nbt_eman_kills_b3"
        );
        assert_eq!(
            auto_field_num_feature("eman_kills", 1_050.0).unwrap(),
            "nbt_eman_kills_b4"
        );

        // Zero/absent emits nothing: a feature carried by nearly every item
        // defines the base and can never be significant against it.
        assert!(auto_field_num_feature("eman_kills", 0.0).is_none());
        assert!(auto_field_num_feature("eman_kills", f64::NAN).is_none());

        // Identity fields never reach the feature set at all.
        for k in ["uuid", "uid", "timestamp", "spawnedFor", "bossId"] {
            assert!(
                AUTO_FIELD_DENYLIST.contains(&k),
                "{k} must be denied: it is unique per item"
            );
        }

        // Nothing already parsed may be re-emitted as an auto field.
        for k in ["id", "winning_bid", "additional_coins", "petInfo", "gems"] {
            assert!(READ_KEYS.contains(&k), "{k} must be in the read-set");
        }
    }

    /// The NBT and JSON paths must name a feature identically, or an item we HOLD
    /// prices off a different feature set than the same item on the AH. Both
    /// dispatchers delegate name construction to the same two builders, so pinning
    /// the JSON dispatch against those builders pins the pair: the only way they
    /// can disagree is if one stops delegating.
    #[test]
    fn auto_field_names_match_across_both_decode_paths() {
        assert_eq!(
            auto_field_feature_json("model", &serde_json::json!("SUMSUNG_2")).as_deref(),
            auto_field_str_feature("model", "SUMSUNG_2").as_deref(),
        );
        assert_eq!(
            auto_field_feature_json("eman_kills", &serde_json::json!(1234)).as_deref(),
            auto_field_num_feature("eman_kills", 1234.0).as_deref(),
        );
        // Case is normalised on both sides, so SUMSUNG_2 and sumsung_2 are one
        // feature rather than two half-populated ones.
        assert_eq!(
            auto_field_str_feature("model", "SUMSUNG_2").as_deref(),
            Some("nbt_model_sumsung_2"),
        );
        // Structures are skipped rather than stringified into a giant feature.
        assert!(auto_field_feature_json("gems", &serde_json::json!({"a": 1})).is_none());
        assert!(auto_field_feature_json("runes", &serde_json::json!([1, 2])).is_none());
    }

    /// NaN parity survives the refactor. A Long `winning_bid` decodes to NaN by
    /// design, and NaN must keep taking the JS `band < 10 == false` branch.
    #[test]
    fn nan_bid_keeps_its_js_parity_branch() {
        assert_eq!(midas_bid_token(f64::NAN, true), "hibid=NaN");
        assert_eq!(midas_bid_token(f64::NAN, false), "bid=NaN");
    }

    #[test]
    fn sub_100m_bands_are_unchanged() {
        // These already meant the right thing, and prod's refs (bid=1 n=89,
        // bid=5 n=84) must keep matching after the change.
        for (bid, want) in [
            (0, "bid=0"),
            (9_999_999, "bid=0"),
            (10_000_000, "bid=1"),
            (55_000_000, "bid=5"),
            (99_999_999, "bid=9"),
        ] {
            assert_eq!(variant("MIDAS_SWORD", bid), want, "bid {bid}");
        }
    }

    #[test]
    fn the_reported_flip_no_longer_looks_max_stat() {
        assert_eq!(variant("MIDAS_SWORD", 120_000_000), "hibid=12");
    }

    #[test]
    fn poisoned_bid_10_key_is_never_reused() {
        // The dangerous case: stored refs labelled `bid=10` mean ">=100M" and
        // cannot be re-banded (attrs are persisted already-decoded). A 105M sword
        // must NOT land on that key or it gets priced off 250M max-stat sales.
        let v = variant("MIDAS_SWORD", 105_000_000);
        assert_ne!(v, "bid=10");
        assert_eq!(v, "hibid=10");
    }

    #[test]
    fn saturates_at_the_250m_stat_cap() {
        assert_eq!(variant("MIDAS_SWORD", 249_000_000), "hibid=24");
        assert_eq!(variant("MIDAS_SWORD", 250_000_000), "hibid=25");
        assert_eq!(variant("MIDAS_SWORD", 400_000_000), "hibid=25");
    }

    #[test]
    fn distinct_stat_tiers_key_apart() {
        let tiers: Vec<String> = [120_000_000, 160_000_000, 200_000_000, 250_000_000]
            .iter()
            .map(|b| variant("MIDAS_SWORD", *b))
            .collect();
        let uniq: std::collections::HashSet<_> = tiers.iter().collect();
        assert_eq!(uniq.len(), tiers.len(), "must key apart: {tiers:?}");
    }

    #[test]
    fn scoped_to_the_sword_only() {
        // Hegemony sells 640-768M across EVERY bid band, i.e. the bid does not
        // drive its value, so re-banding it would only shred its refs.
        for id in [
            "HEGEMONY_ARTIFACT",
            "MIDAS_STAFF",
            "STARRED_MIDAS_SWORD",
            "PLASMA_NUCLEUS",
        ] {
            assert_eq!(
                variant(id, 300_000_000),
                "bid=10",
                "{id} must keep old banding"
            );
        }
    }

    #[test]
    fn long_encoded_bid_keeps_the_nan_quirk() {
        // D1: prismarine gives Long as an array => Number(...) is NaN in JS, so
        // TS emits a NaN band. `NaN < 10` is false in both languages, so NaN must
        // take the hibid branch on BOTH sides. Pins the branch, not the beauty.
        let ib = item_bytes("MIDAS_SWORD", Some(owned::NbtTag::Long(120_000_000)));
        let v = decode_item_bytes(&ib).expect("decodes").variant;
        assert_eq!(v, "hibid=NaN", "Long bid must mirror the TS NaN band");
    }

    #[test]
    fn json_inventory_path_agrees_with_the_auction_path() {
        // attrs_from_extra_json is a SEPARATE mirror of attrs_from_extra, so it
        // can drift independently. Both must band identically.
        for bid in [
            55_000_000i64,
            105_000_000,
            120_000_000,
            250_000_000,
            400_000_000,
        ] {
            let via_json = attrs_from_extra_json(&serde_json::json!({
                "id": "MIDAS_SWORD", "winning_bid": bid
            }))
            .expect("json decodes")
            .variant;
            let via_nbt = variant("MIDAS_SWORD", bid as i32);
            assert_eq!(via_json, via_nbt, "paths disagree at bid {bid}");
        }
    }
}

#[cfg(test)]
mod crown_coins_tests {
    //! Crown of Avarice `collected_coins` banding. See
    //! [`crate::config::CROWN_COINS_BAND`] for the ask data and the reason this
    //! is an `extras` feature rather than a base-key part.
    use super::*;

    #[test]
    fn bands_are_the_digit_count_the_item_pays_out_on() {
        // The lore grants damage and Magic Find "for each digit of Coins
        // consumed", so the band IS the digit count. Values are the ones seen on
        // live crowns plus the boundaries around them.
        for (coins, want) in [
            (0.0, 0),
            (1.0, 1),
            (9.0, 1),
            (10.0, 2),
            (4_900_075.0, 7),
            (100_021_025.0, 9),
            (554_910_555.0, 9),
            (702_336_030.0, 9),
            (999_999_999.0, 9),
            (1_000_000_000.0, 10),
        ] {
            assert_eq!(collected_coins_band(coins), want, "coins {coins}");
        }
    }

    #[test]
    fn the_1b_perk_step_gets_its_own_band() {
        // The one hard threshold in the item: "(Perk changes at 1B Coins
        // consumed)". Live asks jump 1.681B -> 1.880B across it with no overlap,
        // so 999,999,999 and 1,000,000,000 must never share a pool.
        assert_ne!(
            collected_coins_band(999_999_999.0),
            collected_coins_band(1_000_000_000.0),
            "the 1B perk step must split the pool"
        );
        // The counter caps at 1e9; anything at or past the cap is the same item.
        assert_eq!(collected_coins_band(1_000_000_000.0), 10);
        assert_eq!(collected_coins_band(2_000_000_000.0), 10);
    }

    #[test]
    fn an_unparseable_count_keys_as_an_untouched_crown() {
        // Never let a decode failure promote a bare 650M crown into the 1.93B
        // pool. Every one of these must take the band-0 (no feature) branch.
        assert_eq!(collected_coins_band(f64::NAN), 0);
        assert_eq!(collected_coins_band(-1.0), 0);
        assert_eq!(collected_coins_band(f64::NEG_INFINITY), 0);
        assert_eq!(collected_coins_band(0.5), 0);
    }

    #[test]
    fn band_0_is_byte_identical_to_today() {
        // A crown with no coins consumed must key exactly as it does now, which
        // is what makes this safe to ship hot: `collected_coins` is absent on
        // most crowns and 0 on the rest.
        let bare = attrs_from_extra_json(&serde_json::json!({"id": "CROWN_OF_AVARICE"}))
            .expect("json decodes");
        let zeroed = attrs_from_extra_json(
            &serde_json::json!({"id": "CROWN_OF_AVARICE", "collected_coins": 0}),
        )
        .expect("json decodes");
        assert_eq!(bare.extras, zeroed.extras);
        assert!(
            !zeroed.extras.keys().any(|k| k.starts_with("crown_coins")),
            "band 0 must not emit a feature"
        );
    }

    #[test]
    #[ignore = "flag-gated; run with CROWN_COINS_BAND=1"]
    fn the_band_reaches_significance_as_a_learnable_feature() {
        // The whole design rests on this: the band must arrive as an ordinary
        // candidate feature so pass 2 can promote it from real sales. If the
        // name ever drifts, every band silently stops learning and crowns pool
        // flat again. Pins the exact string.
        let maxed = attrs_from_extra_json(&serde_json::json!({
            "id": "CROWN_OF_AVARICE", "collected_coins": 1_000_000_000i64
        }))
        .expect("json decodes");
        let feats = crate::price_index::candidate_features(&maxed);
        assert!(
            feats.iter().any(|f| f == "x:crown_coins_10"),
            "maxed crown must offer x:crown_coins_10, got {feats:?}"
        );
        // And a mid-band crown must not collide with it.
        let mid = attrs_from_extra_json(&serde_json::json!({
            "id": "CROWN_OF_AVARICE", "collected_coins": 702_336_030i64
        }))
        .expect("json decodes");
        let mid_feats = crate::price_index::candidate_features(&mid);
        assert!(mid_feats.iter().any(|f| f == "x:crown_coins_9"));
        assert!(!mid_feats.iter().any(|f| f == "x:crown_coins_10"));
    }

    #[test]
    fn json_inventory_path_agrees_with_the_auction_path() {
        // Flag-independent invariant, same as the pulse-ring mirror below: a
        // crown held in inventory must key identically to the same crown on the
        // AH, or we would misprice our own stock. Hypixel serves the counter as
        // a Long over NBT and as a bare number in flatNbt.
        use simdnbt::owned;
        for coins in [0i64, 4_900_075, 702_336_030, 1_000_000_000] {
            let via_json = attrs_from_extra_json(&serde_json::json!({
                "id": "CROWN_OF_AVARICE", "collected_coins": coins
            }))
            .expect("json decodes")
            .extras;

            let mut extra = owned::NbtCompound::new();
            extra.insert("id", "CROWN_OF_AVARICE");
            extra.insert("collected_coins", owned::NbtTag::Long(coins));
            let mut tag = owned::NbtCompound::new();
            tag.insert("ExtraAttributes", extra);
            let mut item = owned::NbtCompound::new();
            item.insert("tag", tag);
            let mut root = owned::NbtCompound::new();
            root.insert("i", owned::NbtList::Compound(vec![item]));
            let mut buf = Vec::new();
            owned::BaseNbt::new("", root).write(&mut buf);
            let via_nbt = decode_item_bytes(&STANDARD.encode(&buf))
                .expect("decodes")
                .extras;

            assert_eq!(via_json, via_nbt, "paths disagree at {coins} coins");
        }
    }
}

#[cfg(test)]
mod pulse_charge_tests {
    //! PULSE_RING thunder-charge banding. See [`crate::config::PULSE_CHARGE_BAND`]
    //! for the sale data these thresholds come from.
    use super::*;
    use simdnbt::owned;

    #[test]
    fn bands_step_at_the_upgrade_thresholds() {
        // The four observed price plateaus: ~2.84M / ~12M / ~35M / ~110M.
        for (charge, want) in [
            (0.0, 0),
            (149_999.0, 0),
            (150_000.0, 1),
            (350_000.0, 1),
            (999_999.0, 1),
            (1_000_000.0, 2),
            (3_000_000.0, 2),
            (4_999_999.0, 2),
            (5_000_000.0, 3),
            (5_450_000.0, 3),
        ] {
            assert_eq!(pulse_charge_band(charge), want, "charge {charge}");
        }
    }

    #[test]
    fn an_unparseable_charge_falls_to_the_cheapest_band() {
        // Never let a decode failure promote a ring into a 110M pool.
        assert_eq!(pulse_charge_band(f64::NAN), 0);
        assert_eq!(pulse_charge_band(-1.0), 0);
    }

    #[test]
    fn a_recombed_legendary_no_longer_keys_as_a_true_legendary() {
        // The whole point: both display LEGENDARY, but the recombed one carries
        // 1M charge (~49.5M) and the true one 5M (~110M). Distinct bands, so they
        // can never share a pool again.
        assert_ne!(
            pulse_charge_band(1_000_000.0),
            pulse_charge_band(5_000_000.0),
            "recombed and true LEGENDARY must not share a band"
        );
    }

    fn nbt_variant(charge: owned::NbtTag) -> String {
        let mut extra = owned::NbtCompound::new();
        extra.insert("id", "PULSE_RING");
        extra.insert("thunder_charge", charge);
        let mut tag = owned::NbtCompound::new();
        tag.insert("ExtraAttributes", extra);
        let mut item = owned::NbtCompound::new();
        item.insert("tag", tag);
        let mut root = owned::NbtCompound::new();
        root.insert("i", owned::NbtList::Compound(vec![item]));
        let mut buf = Vec::new();
        owned::BaseNbt::new("", root).write(&mut buf);
        decode_item_bytes(&STANDARD.encode(&buf))
            .expect("decodes")
            .variant
    }

    #[test]
    fn json_inventory_path_agrees_with_the_auction_path() {
        // Flag-independent invariant: whether PULSE_CHARGE_BAND is on or off, the
        // two mirrors must agree. Hypixel serves thunder_charge as a STRING in
        // flatNbt and as a number in raw NBT, so both coercions are covered.
        for charge in [0i64, 150_000, 1_000_000, 5_000_000] {
            let via_json = attrs_from_extra_json(&serde_json::json!({
                "id": "PULSE_RING", "thunder_charge": charge.to_string()
            }))
            .expect("json decodes")
            .variant;
            let via_nbt = nbt_variant(owned::NbtTag::Long(charge));
            assert_eq!(via_json, via_nbt, "paths disagree at charge {charge}");
        }
    }
}

#[cfg(test)]
mod lore_weight_tests {
    use super::{parse_lore_weight, weight_band};

    #[test]
    fn reads_the_real_hypixel_line() {
        // Verbatim from a live dump via nbt_probe.
        assert_eq!(parse_lore_weight("§7Current weight: §a1 lb"), Some(1));
        assert_eq!(parse_lore_weight("§7Current weight: §a135 lb"), Some(135));
    }

    #[test]
    fn colour_codes_are_not_digits() {
        // THE trap: `§0`-`§9` put a digit right where the number goes. A naive
        // digit scan reads this as 41 and calls a 1 lb bass heavy.
        assert_eq!(parse_lore_weight("§7Current weight: §41 lb"), Some(1));
        assert_eq!(parse_lore_weight("§7Current weight: §9§250 lb"), Some(50));
    }

    #[test]
    fn thousands_separators_do_not_truncate_the_number() {
        // Found live: a 5,409 lb bass asking 5.3B parsed as 1 lb and fell back
        // into the light pool. The heaviest fish are exactly the ones worth
        // keying, so this is the case that matters most.
        assert_eq!(
            parse_lore_weight("§7Current weight: §a1,200 lbs"),
            Some(1200)
        );
        assert_eq!(
            parse_lore_weight("§7Current weight: §a5,409 lbs"),
            Some(5409)
        );
        assert_eq!(
            parse_lore_weight("§7Current weight: §a1.200 lbs"),
            Some(1200)
        );
        assert_eq!(weight_band(5409), 200);
    }

    #[test]
    fn plural_and_singular_units_both_parse() {
        // Both spellings occur live: "1 lb" and "121 lbs".
        assert_eq!(parse_lore_weight("§7Current weight: §a1 lb"), Some(1));
        assert_eq!(parse_lore_weight("§7Current weight: §a121 lbs"), Some(121));
    }

    #[test]
    fn ignores_every_other_lore_line() {
        assert_eq!(parse_lore_weight("§8Upgrades rarity in 49 lbs"), None);
        assert_eq!(
            parse_lore_weight("§7The heavier your bass is, the more"),
            None
        );
        assert_eq!(parse_lore_weight(""), None);
        assert_eq!(parse_lore_weight("§7Current weight:"), None);
    }

    #[test]
    fn bands_match_the_measured_ask_structure() {
        // median ask: 21M at 10-49, 85M at 50-99, 146M at 100-199, 970M at 200+
        assert_eq!(weight_band(10), 10);
        assert_eq!(weight_band(49), 10);
        assert_eq!(weight_band(50), 50);
        assert_eq!(weight_band(99), 50);
        assert_eq!(weight_band(100), 100);
        assert_eq!(weight_band(199), 100);
        assert_eq!(weight_band(200), 200);
        assert_eq!(weight_band(5300), 200);
    }

    fn bare() -> super::ItemAttributes {
        super::ItemAttributes {
            id: "LOUDMOUTH_BASS".into(),
            attributes: Default::default(),
            enchantments: Default::default(),
            scrolls: vec![],
            reforge: None,
            skin: None,
            recombobulated: false,
            upgrade_level: None,
            gem_slots: 0,
            gems: vec![],
            parts: vec![],
            extras: Default::default(),
            pet: None,
            variant: String::new(),
            item_uuid: None,
            count: 1,
        }
    }

    #[test]
    fn push_variant_composes_like_build_variant() {
        let mut a = bare();
        a.push_variant("w=50");
        assert_eq!(a.variant, "w=50");
        // An item that already carries an ExtraAttributes variant keeps it and
        // joins with '|', the same separator build_variant uses.
        a.variant = "bid=10".to_string();
        a.push_variant("w=100");
        assert_eq!(a.variant, "bid=10|w=100");
    }
}
