//! Port of `baf-flip-finder/src/wsServer.ts` — the COFL flip feed + RPCs.
//!
//! Behavioral clone of the push decision (`pushFlip`): own-listing guard →
//! flood brake → BinMaster/ws-config filter → grind rescue → affordability +
//! overflow + grind routing → assign mode. The async server (tokio-tungstenite)
//! serves the JSON protocol: welcome / flip / estimate / purse / listed / ping.
//!
//! The `estimate` + `inventory` (/trex sellinv) RPCs are answered by the loop
//! thread (which owns the index/model/last-sweep state) via [`RpcRequest`]: the
//! async handler forwards the pricing work and awaits a oneshot reply, so the
//! money core never has to be Send+Sync. `inventory` ports wsServer.ts fully
//! (attrsFromInventorySlot → attrPricer → priceInventory → listInstructions).

use crate::cost_basis::CostBasis;
use crate::store::{PostedRow, Store};
use crate::ws_config::WsFilterConfig;
use finder_core::config::{
    BED_GRACE_MS, DUMP_DETECT_LAG_MS, LIST_FAIL_DAYS, LIST_FAIL_MIN_GAP_H, LIST_LOW_CONF,
    LORE_WEIGHT_ITEMS,
};
use finder_core::filter::{Filter, FilterFlip};
use finder_core::inventory_pricing::{price_inventory, InventoryPricingInput};
use finder_core::nbt::{attrs_from_inventory_slot, ItemAttributes};
use finder_core::price_index::KeyStats;
use finder_core::sniper::Flip;
use futures_util::{SinkExt, StreamExt};
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// SkyBlock ids that are UI/menu filler, never real auction items — skipped
/// silently by the inventory RPC (never listed, never a "Won't list" line).
const NON_AUCTIONABLE_IDS: [&str; 5] = [
    "SKYBLOCK_MENU",
    "DUNGEON_MENU",
    "PET_MENU",
    "TRICK_OR_TREAT_BAG",
    "HUB_SELECTOR",
];
const MIN_SAMPLES: i64 = 5;

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as f64
}

/// A pricing request forwarded from an async ws handler to the single-threaded
/// orchestration loop (which owns the PriceIndex + model + last-sweep state).
/// Keeps the money core off the async threads — it never has to be Send+Sync.
pub enum RpcRequest {
    /// `estimate` RPC: price a live auction by uuid (decodeCache lookup).
    Estimate {
        uuid: String,
        reply: oneshot::Sender<Option<ListingEstimate>>,
    },
    /// `inventory` RPC: price a batch of decoded inventory items (attrPricer).
    PriceAttrs {
        attrs: Vec<ItemAttributes>,
        reply: oneshot::Sender<Vec<Option<ListingEstimate>>>,
    },
    /// Page-flipper RPC: price each crawled `(attrs, observed bin price)` AND
    /// resolve a buyable auction uuid from the live BIN set (prev_by_key), which
    /// only the loop thread owns.
    PriceAndResolve {
        items: Vec<(ItemAttributes, f64)>,
        reply: oneshot::Sender<Vec<AhResolved>>,
    },
}

/// Result of a [`RpcRequest::PriceAndResolve`] item: its listing estimate and,
/// when the crawled auction was found in the live BIN set, its real auction uuid.
pub struct AhResolved {
    pub est: Option<ListingEstimate>,
    /// The matched live auction's uuid, or `None` if it isn't in the live set
    /// (e.g. listed too recently to be in any API dump yet → not buyable by uuid).
    pub uuid: Option<String>,
}

/// Live account status a buyer reports alongside its purse.
#[derive(Clone, Copy, Default)]
pub struct ClientStatus {
    pub purse: f64,
    pub worth: f64,
    pub inv_free: Option<f64>,
    pub auctions: Option<f64>,
    /// Items occupying the 36-slot player inventory (bought, not yet listed).
    pub inv_used: Option<f64>,
    /// The AH is at this account's per-coop slot cap (bot's own is_auction_at_limit).
    pub auction_at_limit: Option<bool>,
}

/// The time-to-sell facts the filter needs, carried from `main` (which holds the
/// price index) down to `push_flip` (which does not).
///
/// `fair_tts_h` is how fast the listings that SOLD sold; `sell_through` is what
/// fraction sold at all. The second only exists once the `censored` table has
/// data for the item, and the filter refuses the volume waiver without it —
/// see [`finder_core::survival`].
#[derive(Clone, Copy)]
pub struct TtsForFilter {
    pub fair_tts_h: f64,
    pub n_fair: i64,
    pub sell_through: Option<f64>,
}

/// Listing estimate returned by the estimate/attr pricer hooks.
#[derive(Clone)]
pub struct ListingEstimate {
    pub target: f64,
    pub lbin: Option<f64>,
    pub volume_per_day: Option<f64>,
    pub confidence: f64,
    pub samples: i64,
    pub key: String,
    pub basis: Option<String>, // "refs" | "model" | "lbin"
    /// Median of recent sales for the item+star group — what it ACTUALLY trades
    /// at, independent of how narrow the final key got. Anchors LIST_MARKET_CAP.
    pub market_median: Option<f64>,
    /// `final_key != base_key`: the item's value comes from a significant feature
    /// (recomb, enrichment, reforge), so `market_median` is a base-pool number
    /// that does NOT describe it. Suppresses the LIST_MARKET_CAP clamp.
    pub variant_priced: bool,
}

/// Outcome of a push, mirrored to the discord embed.
#[derive(Clone)]
pub struct PushResult {
    pub delivered: usize,
    pub mismatch: Option<String>,
}

struct Client {
    is_lister: bool,
    tx: mpsc::UnboundedSender<Message>,
    status: Mutex<Option<ClientStatus>>,
    /// `?player=` identity. The grind rotation is keyed on THIS, not the socket,
    /// so a bot that reconnects keeps its place in the queue (wsServer.ts:334).
    name: String,
}

/// Shared server state (the TS module-level globals). Held in an Arc.
pub struct WsShared {
    pub filters: RwLock<WsFilterConfig>,
    bin_filter: RwLock<Option<Filter>>, // BinMaster filter; Some ⇒ filterActive()
    clients: Mutex<HashMap<u64, Arc<Client>>>,
    next_id: AtomicU64,
    assign_cursor: AtomicUsize,
    anon_seq: AtomicU64,
    /// Least-recently-served rotation for GRIND flips, keyed by client name.
    /// A plain index cursor is wrong here: the eligible set changes size between
    /// flips (purse/status gating), so an index would skip accounts.
    grind_last_assigned: Mutex<HashMap<String, u64>>,
    grind_assign_seq: AtomicU64,
    sweep_detect_at_ms: AtomicI64,
    /// `lastUpdated` of the most recent dump. The publish clock is a PERFECT 60.0s
    /// metronome (11 consecutive publishes, spread 0.0s), but its PHASE drifts
    /// over weeks (:31s in 2026-07, :38.514s in 2026-08) — so derive the phase
    /// from this observed value, never hardcode it.
    last_dump_lu_ms: AtomicI64,
    max_held_per_base: i64,
    cost_basis: Mutex<CostBasis>,
    store: Arc<Mutex<Store>>,
    /// estimate/inventory pricing is answered by the loop thread (owns the index).
    rpc_tx: mpsc::UnboundedSender<RpcRequest>,
    /// itemUuid → firstSeen ms (fallback age for items not bought via this finder).
    listing_tracker: Mutex<HashMap<String, f64>>,
    /// itemUuid → (ask we last instructed, when we instructed it ms, failures so far).
    ///
    /// The evidence behind [`LIST_FAIL_DAYS`]. An item is only handed to us for
    /// pricing while it sits in a bot's INVENTORY, and an item on the auction
    /// house is not in the inventory — so seeing the same itemUuid again, a full
    /// BIN duration after we last told a bot to list it, means that listing ended
    /// and we still hold the thing.
    ///
    /// ⚠️ Deliberately NOT pruned by the `present` set the way `listing_tracker`
    /// is: the whole point is to survive the window where the item is off the
    /// inventory and on the auction house. Pruned by age instead.
    list_attempts: Mutex<HashMap<String, (f64, f64, f64)>>,
    /// Page-flipper: content hashes of AH-crawled auctions already analyzed, so a
    /// listing re-seen across pages/sweeps isn't re-priced or re-pushed. Bounded;
    /// cleared when it grows large. Entirely separate from the API flip path.
    ah_seen: Mutex<HashSet<u64>>,
    /// auction uuid -> listing `start` (epoch ms), for flips that arrived from a
    /// source that can see an auction DURING its bed/grace window.
    ///
    /// The free paginated dump withholds a BIN for a hard 20s (measured over 10
    /// refreshes: youngest ever 19.95s), so a dump flip is always already
    /// buyable. The per-player lookup through NetherAPI has no such delay and
    /// serves BINs 3-7s old, i.e. mid-bed. Those are the only flips where "when
    /// does this become buyable" is a real question, so only seller-follow
    /// populates this.
    auction_start: Mutex<HashMap<String, f64>>,
    /// `posted` rows queued for the background writer instead of being INSERTed
    /// on the sweep loop. See [`WsShared::record_posted`].
    posted_tx: Mutex<std::sync::mpsc::Sender<crate::store::PostedRow>>,
    /// Public leftover feed (`PUBLIC_WS=1`), set once at boot. A `OnceLock` so the
    /// hot path pays one atomic load when the feature is off, which is the default.
    public_hub: std::sync::OnceLock<Arc<crate::public_ws::PublicHub>>,
    /// base_key -> rows queued but NOT yet committed, counted as delivered.
    ///
    /// ⚠️ LOAD-BEARING. The flood brake reads `unsold_held_count`, which counts
    /// the very rows `record_posted` writes (delivered = 1, bought_at IS NULL,
    /// posted_at within 10 min). Making the write async without this overlay
    /// would let a SECOND flip of the same base_key inside one sweep slip past
    /// `MAX_HELD_PER_BASE`, because the first one is not in the table yet.
    /// Decremented only AFTER the batch commits, so the count can briefly be too
    /// HIGH (conservative: we skip a flip) but never too low.
    pending_held: Mutex<HashMap<String, i64>>,
}

fn fmt_coins(n: f64) -> String {
    if n.abs() >= 1e6 {
        format!("{:.1}M", n / 1e6)
    } else {
        format!("{}k", (n / 1000.0).round() as i64)
    }
}

/// All ids an item answers to for blacklisting (its id, and for pets PET_<T> + <T>).
pub(crate) fn blacklist_ids_for(attrs: &ItemAttributes) -> Vec<String> {
    let mut ids = vec![attrs.id.to_uppercase()];
    if let Some(pet) = &attrs.pet {
        if !pet.pet_type.is_empty() {
            let t = pet.pet_type.to_uppercase();
            ids.push(format!("PET_{t}"));
            ids.push(t);
        }
    }
    ids
}

/// JS `JSON.stringify` number semantics: an INTEGRAL f64 must serialize without a
/// decimal point (`1848997`, not `1848997.0`).
///
/// This is load-bearing, not cosmetic. The mod reads `price`/`target`/`listAt`
/// with serde_json's `as_u64()` (client.rs:267, main.rs:1334/3901), and `as_u64()`
/// returns **None for any float**. So `1848997.0` makes the mod read `0`, fail its
/// `starting_bid > 0` guard, and SILENTLY DROP the flip — the bot logs the message
/// and does nothing. TS never hit this because JS emits integral numbers as ints.
/// Least-recently-served pick (wsServer.ts:620-632). Accounts never served sort
/// first (seq -1). With a stable eligible set this is exactly round-robin; unlike
/// an index cursor it stays fair when the eligible set changes size between flips,
/// which it does (purse/status gating). Keyed on the client NAME (`?player=`), so a
/// reconnecting bot keeps its place instead of jumping the queue.
fn pick_grind_target_in(
    last: &Mutex<HashMap<String, u64>>,
    seq_counter: &AtomicU64,
    eligible: &[Arc<Client>],
) -> Arc<Client> {
    let mut last = last.lock().unwrap();
    let seq_of = |c: &Arc<Client>, l: &HashMap<String, u64>| -> i64 {
        l.get(&c.name).map(|v| *v as i64).unwrap_or(-1)
    };
    let mut best = eligible[0].clone();
    let mut best_seq = seq_of(&best, &last);
    for c in eligible {
        let seq = seq_of(c, &last);
        if seq < best_seq {
            best = c.clone();
            best_seq = seq;
        }
    }
    last.insert(
        best.name.clone(),
        seq_counter.fetch_add(1, Ordering::Relaxed),
    );
    best
}

pub(crate) fn jnum(x: f64) -> Value {
    // 2^53: beyond it f64 cannot represent consecutive ints, and JS switches to
    // exponent form anyway; keep those as floats rather than lie.
    if x.is_finite() && x.fract() == 0.0 && x.abs() <= 9_007_199_254_740_992.0 {
        Value::from(x as i64)
    } else {
        Value::from(x)
    }
}

/// jnum for optionals: None → JSON null (what TS sends).
pub(crate) fn jnum_opt(x: Option<f64>) -> Value {
    match x {
        Some(v) => jnum(v),
        None => Value::Null,
    }
}

impl WsShared {
    pub fn new(
        filters: WsFilterConfig,
        cost_basis: CostBasis,
        store: Arc<Mutex<Store>>,
        max_held_per_base: i64,
        rpc_tx: mpsc::UnboundedSender<RpcRequest>,
    ) -> Arc<Self> {
        let (posted_tx, posted_rx) = std::sync::mpsc::channel();
        let store_for_writer = store.clone();
        let shared = Arc::new(WsShared {
            filters: RwLock::new(filters),
            bin_filter: RwLock::new(None),
            clients: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            assign_cursor: AtomicUsize::new(0),
            anon_seq: AtomicU64::new(0),
            grind_last_assigned: Mutex::new(HashMap::new()),
            grind_assign_seq: AtomicU64::new(0),
            sweep_detect_at_ms: AtomicI64::new(0),
            last_dump_lu_ms: AtomicI64::new(0),
            max_held_per_base,
            cost_basis: Mutex::new(cost_basis),
            store,
            rpc_tx,
            listing_tracker: Mutex::new(HashMap::new()),
            list_attempts: Mutex::new(HashMap::new()),
            ah_seen: Mutex::new(HashSet::new()),
            auction_start: Mutex::new(HashMap::new()),
            posted_tx: Mutex::new(posted_tx),
            pending_held: Mutex::new(HashMap::new()),
            public_hub: std::sync::OnceLock::new(),
        });
        Self::spawn_posted_writer(&shared, posted_rx, store_for_writer);
        shared
    }

    /// Drains queued `posted` rows and commits them in batches, off the sweep
    /// loop. Coalesces whatever arrived while the last batch was committing, so a
    /// burst of flips in one sweep becomes ONE transaction.
    fn spawn_posted_writer(
        shared: &Arc<Self>,
        rx: std::sync::mpsc::Receiver<crate::store::PostedRow>,
        store: Arc<Mutex<Store>>,
    ) {
        let shared = Arc::downgrade(shared);
        std::thread::spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut batch = vec![first];
                // Take everything already queued; do not wait for more.
                while let Ok(next) = rx.try_recv() {
                    batch.push(next);
                }
                let res = store.lock().unwrap().record_posted_batch(&batch);
                if let Err(e) = res {
                    eprintln!("posted writer: batch of {} failed: {e}", batch.len());
                }
                // Release the overlay only once the rows are actually visible to
                // `unsold_held_count`, even on failure -- a stuck counter would
                // silently choke the flood brake forever.
                if let Some(sh) = shared.upgrade() {
                    let mut pend = sh.pending_held.lock().unwrap();
                    for r in &batch {
                        if !r.delivered {
                            continue;
                        }
                        if let Some(bk) = &r.base_key {
                            if let Some(n) = pend.get_mut(bk) {
                                *n -= 1;
                                if *n <= 0 {
                                    pend.remove(bk);
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    /// Attach the public leftover feed. Called once at boot, before any sweep.
    pub fn set_public_hub(&self, hub: Arc<crate::public_ws::PublicHub>) {
        let _ = self.public_hub.set(hub);
    }

    pub fn set_bin_filter(&self, f: Option<Filter>) {
        *self.bin_filter.write().unwrap() = f;
    }
    pub fn set_sweep_detect_at(&self, ms: i64) {
        self.sweep_detect_at_ms.store(ms, Ordering::Relaxed);
    }
    pub fn set_last_dump_lu(&self, ms: i64) {
        self.last_dump_lu_ms.store(ms, Ordering::Relaxed);
    }

    /// When the PUBLIC dump would first show this auction — i.e. the moment our
    /// pre-API edge on it expires.
    ///
    /// Not every pre-API find is a bed. A BIN is withheld from the dump for 20s,
    /// but the dump only publishes once every 60s, so an auction listed just
    /// after a publish is buyable at +20s and still invisible to every
    /// dump-reader until +60s or later. Those are pre-API AND immediately
    /// purchasable — strictly better than a bed, and previously indistinguishable
    /// from an ordinary flip on the card.
    ///
    /// = first metronome tick at or after `start + BED_GRACE_MS`, plus the lag
    /// between a publish and us having it.
    pub fn api_visible_at_ms(&self, uuid: &str) -> Option<f64> {
        let start = *self.auction_start.lock().unwrap().get(uuid)?;
        let lu = self.last_dump_lu_ms.load(Ordering::Relaxed) as f64;
        if lu <= 0.0 {
            return None;
        }
        let eligible = start + *BED_GRACE_MS;
        // Walk the 60s grid (anchored on the observed phase) to the first tick
        // at or after `eligible`. Works for ticks before or after `lu`.
        let period = 60_000.0;
        let k = ((eligible - lu) / period).ceil();
        Some(lu + k * period + *DUMP_DETECT_LAG_MS)
    }
    fn filter_active(&self) -> bool {
        self.bin_filter.read().unwrap().is_some()
    }

    /// Whether a flip that FAILED the normal filter still qualifies as a grind
    /// flip for small accounts. Off entirely when maxPurse is 0.
    /// Least-recently-served grind account (wsServer.ts:620). Delegates so the
    /// rotation can be tested without standing up a Store/CostBasis.
    fn pick_grind_target(&self, eligible: &[Arc<Client>]) -> Arc<Client> {
        pick_grind_target_in(&self.grind_last_assigned, &self.grind_assign_seq, eligible)
    }

    fn grind_eligible(&self, f: &Flip) -> bool {
        let ft = self.filters.read().unwrap();
        if ft.max_purse <= 0.0 {
            return false;
        }
        let Some(ms) = &f.median_stats else {
            return false;
        };
        if ms.volume_per_day < ft.grind_min_volume_per_day {
            return false;
        }
        if ms.volatility > ft.grind_max_volatility {
            return false;
        }
        if f.profit < ft.grind_min_profit {
            return false;
        }
        if f.confidence < ft.grind_min_confidence {
            return false;
        }
        let ids = blacklist_ids_for(&f.attrs);
        if ft
            .blacklist_ids
            .iter()
            .any(|b| ids.contains(&b.to_uppercase()))
        {
            return false;
        }
        for g in &ft.blocked_guards {
            if !g.is_empty() && f.guard.contains(g) {
                return false;
            }
        }
        true
    }

    /// Why a flip does NOT match the ws-config filters, or None when it matches.
    fn filter_mismatch(&self, f: &Flip) -> Option<String> {
        let ft = self.filters.read().unwrap();
        let ids = blacklist_ids_for(&f.attrs);
        if !ft.blacklist_ids.is_empty()
            && ft
                .blacklist_ids
                .iter()
                .any(|b| ids.contains(&b.to_uppercase()))
        {
            return Some(format!("item {} blacklisted", ids[0]));
        }
        if f.profit < ft.min_profit {
            return Some(format!(
                "profit {} < minProfit {}",
                fmt_coins(f.profit),
                fmt_coins(ft.min_profit)
            ));
        }
        if f.roi_pct < ft.min_roi_pct {
            return Some(format!(
                "ROI {:.0}% < minRoiPct {}%",
                f.roi_pct, ft.min_roi_pct
            ));
        }
        // A large ABSOLUTE profit is a different risk from a small one at the
        // same ROI, and confidence is a bad proxy for it. See
        // `BIG_FLIP_MIN_PROFIT` for the 202M drill this was written for.
        let big_flip = *finder_core::config::BIG_FLIP_MIN_PROFIT > 0.0
            && f.profit >= *finder_core::config::BIG_FLIP_MIN_PROFIT
            && f.roi_pct >= *finder_core::config::BIG_FLIP_MIN_ROI_PCT
            && f.samples >= *finder_core::config::BIG_FLIP_MIN_SAMPLES;
        if f.confidence < ft.min_confidence && !big_flip {
            return Some(format!(
                "confidence {}% < minConfidence {}%",
                (f.confidence * 100.0).round(),
                (ft.min_confidence * 100.0).round()
            ));
        }
        if f.finder == "lbin" && !ft.allow_lbin {
            return Some("lbin flips disabled (allowLbin=false)".to_string());
        }
        let vol = f.median_stats.as_ref().map(|m| m.volume_per_day);
        if f.finder != "lbin" && !big_flip {
            if let Some(v) = vol {
                if v < ft.min_volume_per_day {
                    return Some(format!(
                        "volume {:.1}/day < minVolumePerDay {}",
                        v, ft.min_volume_per_day
                    ));
                }
            }
        }
        if ft.min_profit_per_hour > 0.0 {
            if let Some(v) = vol {
                let pph = (f.profit * v) / 24.0;
                if pph < ft.min_profit_per_hour {
                    return Some(format!(
                        "profit/h {} < minProfitPerHour {}",
                        fmt_coins(pph),
                        fmt_coins(ft.min_profit_per_hour)
                    ));
                }
            }
        }
        for g in &ft.blocked_guards {
            if !g.is_empty() && f.guard.contains(g) {
                return Some(format!("guard '{g}' blocked"));
            }
        }
        None
    }

    /// Eligibility of a flip for a client given its status (overflow + affordability).
    /// None ⇒ eligible; else the reason. status None ⇒ never gated.
    fn client_ineligibility(&self, f: &Flip, st: Option<&ClientStatus>) -> Option<String> {
        let ft = self.filters.read().unwrap();
        let st = st?;
        if ft.min_free_inv_slots > 0.0 {
            if let Some(free) = st.inv_free {
                if free < ft.min_free_inv_slots {
                    return Some(format!(
                        "only {} free inv slots (< {})",
                        free, ft.min_free_inv_slots
                    ));
                }
            }
        }
        if ft.max_active_auctions > 0.0 {
            if let Some(auc) = st.auctions {
                if auc >= ft.max_active_auctions {
                    return Some(format!(
                        "auction house full ({}/{} listings)",
                        auc, ft.max_active_auctions
                    ));
                }
            }
        }
        if ft.max_spend_fraction > 0.0
            && st.purse > 0.0
            && f.price > st.purse * ft.max_spend_fraction
        {
            return Some(format!(
                "price {} > {}% of purse {}",
                fmt_coins(f.price),
                (ft.max_spend_fraction * 100.0).round(),
                fmt_coins(st.purse)
            ));
        }
        // Per-bot effective min-profit floor. Only ever RAISES this bot's bar:
        //  - purse-scaled: big-purse (non-grind) accounts demand flips sized to
        //    the account (a 400k/4M flip is a wasted slot on a whale);
        //  - congestion: a bot low on free inventory slots can't list what it
        //    buys, so only fat flips justify a slot until it drains.
        // Because this filters the eligible set before the assign broadcast, a
        // sub-floor flip simply routes to a bot with headroom.
        let mut floor = 0.0f64;
        let mut why = "";
        if ft.min_profit_purse_fraction > 0.0 && st.purse > ft.max_purse {
            let pf = st.purse * ft.min_profit_purse_fraction;
            if pf > floor {
                floor = pf;
                why = "big-purse floor";
            }
        }
        if ft.full_min_profit > 0.0 && ft.full_min_profit > floor {
            // Exact rule when the bot reports it: AH at this account's per-coop slot
            // cap AND enough items stuck in inventory (bought but not listable). Falls
            // back to the free-slot proxy for bots that don't yet send the two fields.
            let congested = match (st.auction_at_limit, st.inv_used) {
                (Some(at_limit), Some(used)) => at_limit && used >= ft.full_inv_used_at_least,
                _ => st
                    .inv_free
                    .map_or(false, |free| free <= ft.full_inv_free_threshold),
            };
            if congested {
                floor = ft.full_min_profit;
                why = "inv full";
            }
        }
        if floor > 0.0 && f.profit < floor {
            return Some(format!(
                "{why}: profit {} < {}",
                fmt_coins(f.profit),
                fmt_coins(floor)
            ));
        }
        None
    }

    /// Only flips at/above hardMinProfit go to the webhook (grind kept out).
    pub fn webhook_worthy(&self, f: &Flip) -> bool {
        let ft = self.filters.read().unwrap();
        !(ft.hard_min_profit > 0.0 && f.profit < ft.hard_min_profit)
    }

    /// The base merit filter (hardMinProfit → BinMaster → ws-config), independent of
    /// any auction uuid or buyer. Returns `(mismatch reason | None, chosen price scale)`.
    /// Shared by `push_flip` and the pageflipper `passes_filter` probe so the two can
    /// never drift.
    #[allow(clippy::needless_pass_by_value)]
    fn base_filter_decision(&self, f: &Flip, tts: Option<TtsForFilter>) -> (Option<String>, f64) {
        let mut scale_price = 1.0f64;
        let hard_min = self.filters.read().unwrap().hard_min_profit;
        if hard_min > 0.0 && f.profit < hard_min {
            return (
                Some(format!(
                    "profit {} < hardMinProfit {}",
                    fmt_coins(f.profit),
                    fmt_coins(hard_min)
                )),
                scale_price,
            );
        }
        let mismatch = if self.filter_active() {
            let ids = blacklist_ids_for(&f.attrs);
            let blacklist = self.filters.read().unwrap().blacklist_ids.clone();
            if !ids.is_empty() && blacklist.iter().any(|b| ids.contains(&b.to_uppercase())) {
                Some(format!("item {} blacklisted", ids[0]))
            } else {
                let blocked = self.filters.read().unwrap().blocked_guards.clone();
                let mut m = None;
                for g in &blocked {
                    if !g.is_empty() && f.guard.contains(g) {
                        m = Some(format!("guard '{g}' blocked"));
                        break;
                    }
                }
                if m.is_none() {
                    let ff = FilterFlip {
                        attrs: f.attrs.clone(),
                        profit: f.profit,
                        roi_pct: f.roi_pct,
                        confidence: f.confidence,
                        volume_per_day: f.median_stats.as_ref().map(|m| m.volume_per_day),
                        fair_tts_ms: tts.map(|t| t.fair_tts_h * 3_600_000.0),
                        tts_samples: tts.map(|t| t.n_fair),
                        sell_through: tts.and_then(|t| t.sell_through),
                    };
                    let decision = self
                        .bin_filter
                        .read()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .evaluate_flip(&ff);
                    scale_price = decision.scale_price;
                    m = decision.reason;
                }
                m
            }
        } else {
            self.filter_mismatch(f)
        };
        (mismatch, scale_price)
    }

    /// Why this flip would NOT clear the uuid-independent merit gates — the flood
    /// brake and the base filter (hardMinProfit → BinMaster/ws-config), same as
    /// `push_flip` applies — or `None` when it passes. Pageflipper checks this
    /// BEFORE spending a scarce NetherAPI seller lookup: no point resolving an
    /// auction uuid for a flip the buyers' filter would reject. Returning the
    /// REASON (not just a bool) keeps a rejected flip from vanishing silently — an
    /// +18.8M flip was being dropped with no trace.
    ///
    /// Must mirror `push_flip`'s grind-rescue fallback: a flip that fails the base
    /// filter but clears `grind_eligible` still gets bought (routed to small-purse
    /// accounts only), so it's still worth the lookup. Before this, a flip crawled
    /// pre-dump (no uuid yet, needs a seller lookup) was measurably worse off than
    /// the identical flip crawled post-dump (uuid already known, straight into
    /// `push_flip`) purely because this probe never checked grind eligibility —
    /// the one lane pageflipper exists for (catching flips before they hit the
    /// public dump) was silently disadvantaged relative to the other.
    /// Per-buyer affordability is still re-checked by `push_flip` once the uuid is known.
    pub fn filter_reject_reason(&self, f: &Flip) -> Option<String> {
        if self.max_held_per_base > 0 {
            let base_k = f.key.split('#').next().unwrap_or(&f.key).to_string();
            let held = self.store.lock().unwrap().unsold_held_count(&base_k)
                + self
                    .pending_held
                    .lock()
                    .unwrap()
                    .get(&base_k)
                    .copied()
                    .unwrap_or(0);
            if held >= self.max_held_per_base {
                return Some(format!(
                    "holding {held} unsold {base_k} (cap {})",
                    self.max_held_per_base
                ));
            }
        }
        let mismatch = self.base_filter_decision(f, None).0;
        if mismatch.is_some() && self.grind_eligible(f) {
            return None;
        }
        mismatch
    }

    /// `pushFlip` — the full push decision + routing, plus the public-feed tap.
    ///
    /// The tap lives HERE rather than at the call sites so that every path that
    /// can decline a flip (page sweep, seller-follow, pageflipper) publishes
    /// identically and none can drift. A declined flip is one no bot of ours was
    /// sent, so handing it to the public feed cannot compete with our own buying.
    /// `publish` returns on an atomic load when nobody is connected and drops
    /// rather than blocks when the queue is full: the money path never waits on a
    /// public consumer.
    pub fn push_flip(&self, f: &Flip, lbin: Option<f64>, tts: Option<TtsForFilter>) -> PushResult {
        let res = self.push_flip_inner(f, lbin, tts);
        if let (Some(reason), Some(hub)) = (&res.mismatch, self.public_hub.get()) {
            hub.publish(f, lbin, reason);
        }
        res
    }

    fn push_flip_inner(
        &self,
        f: &Flip,
        lbin: Option<f64>,
        tts: Option<TtsForFilter>,
    ) -> PushResult {
        // The cheap filter runs FIRST, because it is what actually rejects.
        // Measured over 1,742 rejections: profit 86.8%, confidence 4.2%, roi
        // 2.5%, volume 0.1% -- 93.5% decided by pure arithmetic -- against
        // flood 2.2% and the self-buy guard 2.1%. Both guards below take a
        // mutex, and the flood brake runs two indexed COUNT(*) queries over a
        // 189k-row `posted`. Running them ahead of the arithmetic meant paying
        // for both on ~19 of every 20 flips we were about to discard.
        //
        // That cost is not theoretical: 720 SLOWPUSH events totalling 5,554ms,
        // **95% of them with clients=0**, i.e. stalls of mean 7.7ms spent on
        // flips delivered to nobody. Every one sits on the loop thread while the
        // dump is still streaming, so it delays every later item -- including
        // the next real flip. That is precisely how a 16ms race against COFL is
        // lost, and unlike emit-ordering it needs no prediction of which flip
        // will be big.
        //
        // ⚠️ This changes which tier a MULTI-reject flip is reported under (the
        // filter-reason ordering trap): a flip tripping both `profit` and the
        // self-buy guard now reports `profit`, so the funnel's `guard`/`flood`
        // counts fall. Delivery is unchanged -- anything delivered still passes
        // all three checks below -- so this is diagnostics only.
        let (mismatch, mut scale_price) = self.base_filter_decision(f, tts);
        if mismatch.is_some() && !self.grind_eligible(f) {
            return PushResult {
                delivered: 0,
                mismatch,
            };
        }

        let item_uuid = f.attrs.item_uuid.as_deref();
        // Self-listing guard.
        if self
            .cost_basis
            .lock()
            .unwrap()
            .is_own_listing(Some(&f.uuid), item_uuid)
        {
            return PushResult {
                delivered: 0,
                mismatch: Some("own listing (self-buy guard)".to_string()),
            };
        }
        // Flood brake.
        if self.max_held_per_base > 0 {
            let base_k = f.key.split('#').next().unwrap_or(&f.key).to_string();
            let held = self.store.lock().unwrap().unsold_held_count(&base_k)
                + self
                    .pending_held
                    .lock()
                    .unwrap()
                    .get(&base_k)
                    .copied()
                    .unwrap_or(0);
            if held >= self.max_held_per_base {
                return PushResult {
                    delivered: 0,
                    mismatch: Some(format!(
                        "holding {held} unsold {base_k} (cap {})",
                        self.max_held_per_base
                    )),
                };
            }
        }
        // Grind rescue for small accounts. The base filter (hardMinProfit →
        // BinMaster → ws-config) and its grind check both ran at the top; a
        // still-set `mismatch` here therefore means grind_eligible was true and
        // the flip has now also cleared both guards above.
        let mut grind_only = false;
        if mismatch.is_some() {
            grind_only = true;
            scale_price = 1.0;
        }
        // Snapshot open clients (buyers).
        let clients = self.clients.lock().unwrap();
        let mut open: Vec<Arc<Client>> =
            clients.values().filter(|c| !c.is_lister).cloned().collect();
        drop(clients);
        if open.is_empty() {
            return PushResult {
                delivered: 0,
                mismatch: Some("no client connected".to_string()),
            };
        }
        // Per-client narrowing: grind routing + affordability/overflow.
        let max_purse = self.filters.read().unwrap().max_purse;
        let any_status = open.iter().any(|c| c.status.lock().unwrap().is_some());
        if grind_only || any_status {
            let mut last_reason: Option<String> = None;
            let eligible: Vec<Arc<Client>> = open
                .iter()
                .filter(|c| {
                    let st = *c.status.lock().unwrap();
                    if grind_only {
                        match st {
                            None => {
                                last_reason =
                                    Some("grind flip: bot not reporting purse".to_string());
                                return false;
                            }
                            Some(s) if s.purse > max_purse => {
                                last_reason = Some(format!(
                                    "grind flip: purse {} > maxPurse {}",
                                    fmt_coins(s.purse),
                                    fmt_coins(max_purse)
                                ));
                                return false;
                            }
                            _ => {}
                        }
                    }
                    if let Some(r) = self.client_ineligibility(f, st.as_ref()) {
                        last_reason = Some(r);
                        return false;
                    }
                    true
                })
                .cloned()
                .collect();
            if eligible.is_empty() {
                return PushResult {
                    delivered: 0,
                    mismatch: Some(last_reason.unwrap_or_else(|| "no eligible client".to_string())),
                };
            }
            open = eligible;
        }
        // Record buy price by item uuid for the break-even resale floor.
        if let Some(u) = item_uuid {
            self.cost_basis.lock().unwrap().record_cost(u, f.price);
        }
        let payload = self.flip_payload(f, lbin, scale_price);
        // wsServer.ts:782 — grind flips follow `grindAssignMode`, regular flips
        // `assignMode`. Broadcasting a grind flip to every small account makes them
        // all race for one item only one can win: the losers burn the attempt
        // instead of grinding their own item, which is the opposite of the point.
        let assign_mode = {
            let ft = self.filters.read().unwrap();
            if grind_only {
                ft.grind_assign_mode.clone()
            } else {
                ft.assign_mode.clone()
            }
        };
        if assign_mode == "all" {
            let mut delivered = 0;
            for c in &open {
                if c.tx.send(Message::Text(payload.clone())).is_ok() {
                    delivered += 1;
                }
            }
            return PushResult {
                delivered,
                mismatch: None,
            };
        }
        // 'single': grind rotates by least-recently-served, regular by cursor.
        if grind_only {
            let target = self.pick_grind_target(&open);
            let delivered = usize::from(target.tx.send(Message::Text(payload)).is_ok());
            return PushResult {
                delivered,
                mismatch: None,
            };
        }
        let idx = self.assign_cursor.fetch_add(1, Ordering::Relaxed) % open.len();
        let _ = open[idx].tx.send(Message::Text(payload));
        PushResult {
            delivered: 1,
            mismatch: None,
        }
    }

    /// Record when an auction was listed, so `flip_payload` can tell a bot the
    /// instant it becomes purchasable.
    ///
    /// Call this from EVERY path that evaluates an auction, not just
    /// seller-follow. The original version was follow-only on the theory that the
    /// dump's 20s wall makes every dump flip already buyable — but prod disproved
    /// it: bots on our own feed logged 69 grace-period bed attempts overnight
    /// while `purchaseAt` was null on all of them, so beds reach the bots through
    /// a path that was not recording `start`.
    ///
    /// Self-guarding: it stores nothing unless the auction is STILL inside its
    /// grace window, so the map only ever holds live beds (a handful), and the
    /// cheap arithmetic check runs before the lock is taken — this sits on the
    /// per-BIN hot path, ~150-900 calls per sweep.
    /// `force` = record regardless of whether the grace window is still open.
    /// The DUMP path passes false: it fires ~150-900x per sweep and a dump BIN is
    /// almost never inside grace, so the arithmetic guard keeps it off the lock.
    /// SELLER-FOLLOW passes true, because a follow find can be pre-API without
    /// being a bed (buyable already, but not yet in any published dump) and we
    /// still want to report that lead.
    pub fn record_auction_start(&self, uuid: &str, start_ms: f64, now_ms: f64, force: bool) {
        if start_ms <= 0.0 || (!force && start_ms + *BED_GRACE_MS <= now_ms) {
            return;
        }
        let mut m = self.auction_start.lock().unwrap();
        if m.len() > 50_000 {
            m.clear();
        }
        m.insert(uuid.to_string(), start_ms);
    }

    /// `start + grace`, but only while it is still in the FUTURE. A dump flip is
    /// always past that point, and a bot must not be told to wait on a timestamp
    /// that has already gone by.
    pub fn purchase_at_ms(&self, uuid: &str) -> Option<f64> {
        let start = *self.auction_start.lock().unwrap().get(uuid)?;
        let at = start + *BED_GRACE_MS;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0);
        (at > now).then_some(at)
    }

    fn flip_payload(&self, f: &Flip, lbin: Option<f64>, scale_price: f64) -> String {
        let ft = self.filters.read().unwrap();
        let list_at = if ft.listing_recommendations {
            let hedge = if f.finder == "model" { 0.97 } else { 1.0 };
            Some(
                (f.reference * scale_price * hedge)
                    .round()
                    .max((f.price * 1.05).ceil()),
            )
        } else {
            None
        };
        let vol = f
            .median_stats
            .as_ref()
            .map(|m| (m.volume_per_day * 100.0).round() / 100.0);
        let sweep_at = self.sweep_detect_at_ms.load(Ordering::Relaxed);
        json!({
            "type": "flip",
            "flip": {
                "uuid": f.uuid,
                "itemName": f.item_name,
                "finder": f.finder,
                "price": jnum(f.price),
                "target": jnum(f.reference.round()),
                "profit": jnum(f.profit.round()),
                "roiPct": jnum((f.roi_pct * 10.0).round() / 10.0),
                "confidence": jnum((f.confidence * 1000.0).round() / 1000.0),
                "samples": f.samples,
                "volumePerDay": jnum_opt(vol),
                "lbin": jnum_opt(lbin),
                "key": f.key,
                "guard": f.guard,
                "listAt": jnum_opt(list_at),
                // Non-null ONLY for a flip caught mid-bed: the auction exists but
                // is not buyable until this instant. Null means buy now, which is
                // every dump flip. MUST be an integer — the mod reads it with
                // `as_i64()` and a float makes the WHOLE flip fail to parse.
                "purchaseAt": match self.purchase_at_ms(&f.uuid) {
                    Some(t) => {
                        // DIAGNOSTIC: rare by construction (only live beds), and
                        // it is the ONLY way to confirm from the finder side that
                        // a bot was actually handed bed timing.
                        eprintln!(
                            "BED: flip {} carries purchaseAt, buyable in {:.0}ms",
                            f.uuid,
                            t - std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as f64)
                                .unwrap_or(0.0)
                        );
                        Value::from(t.round() as i64)
                    }
                    None => Value::Null,
                },
                "foundAtMs": jnum(f.found_at_ms),
                "foundAfterRefreshMs": jnum(f.found_after_refresh_ms),
                // Wall clock at the moment we serialise, so the delivery leg can
                // be measured without instrumenting the bot: the bot logs this
                // whole payload with its OWN timestamp, and bot_log_ts - sentAtMs
                // is transport + both runtimes' scheduling, nothing else. Bots and
                // finder share a box, so the clocks are the same.
                "sentAtMs": Value::from(crate::now_ms_real()),
                "dumpDetectedAtMs": if sweep_at != 0 { Value::from(sweep_at) } else { Value::Null },
            }
        })
        .to_string()
    }

    /// Connected bots that can take a bazaar order, with the two capacities the
    /// sizing needs: `(name, purse, free inventory slots)`. Listing-only clients
    /// are excluded — they hold no coins and cannot walk to a bazaar terminal.
    /// One entry per named account, so a bot that reconnected mid-pass is not
    /// counted twice and handed two orders for the same slot.
    ///
    /// Free slots are load-bearing for unstackable products (every
    /// `ENCHANTMENT_*` book): one unit costs one slot, and the mod skips the
    /// order entirely when there is no room. `inv_free` is only present once a
    /// bot has reported status, so absence is treated as "no room" rather than
    /// as unlimited — an unstackable order sized against a guess is one the mod
    /// silently cuts.
    pub fn bazaar_targets(&self) -> Vec<(String, f64, i64)> {
        let clients = self.clients.lock().unwrap();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut out = Vec::new();
        for c in clients.values() {
            if c.is_lister || c.name.is_empty() || !seen.insert(&c.name) {
                continue;
            }
            let (purse, free) = c
                .status
                .lock()
                .unwrap()
                .as_ref()
                .map(|s| (s.purse, s.inv_free.unwrap_or(0.0).max(0.0) as i64))
                .unwrap_or((0.0, 0));
            if purse > 0.0 {
                out.push((c.name.clone(), purse, free));
            }
        }
        out
    }

    /// Every connected bot, REGARDLESS of purse.
    ///
    /// ⛔ `bazaar_targets()` drops anything with `purse <= 0`, which is correct
    /// for BUYING and wrong for SELLING. Purse is a buy constraint; a bot with no
    /// coins is precisely the one that most needs to liquidate. Using the buy
    /// roster for the orphan sweep created a deadlock: spend the purse → buy
    /// orders fill → inventory fills → purse is 0 → dropped from targets → the
    /// sweep never runs → nothing sells → the purse stays 0.
    ///
    /// Observed 2026-08-16: `ROSTER eligible 0` while 7 bots held positions and
    /// inventories sat at `invUsed` 24-33.
    pub fn bazaar_holders(&self) -> Vec<(String, f64, i64)> {
        let clients = self.clients.lock().unwrap();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut out = Vec::new();
        for c in clients.values() {
            if c.is_lister || c.name.is_empty() || !seen.insert(&c.name) {
                continue;
            }
            let (purse, free) = c
                .status
                .lock()
                .unwrap()
                .as_ref()
                .map(|s| (s.purse, s.inv_free.unwrap_or(0.0).max(0.0) as i64))
                .unwrap_or((0.0, 0));
            out.push((c.name.clone(), purse, free));
        }
        out
    }

    /// Send one payload to one named bot. `false` when it is no longer connected.
    pub fn send_to_bot(&self, name: &str, payload: String) -> bool {
        let clients = self.clients.lock().unwrap();
        clients
            .values()
            .find(|c| c.name == name)
            .map(|c| c.tx.send(Message::Text(payload)).is_ok())
            .unwrap_or(false)
    }

    /// Record a posted flip for validation (called by orchestration after push).
    pub fn record_posted(&self, f: &Flip, delivered: bool) {
        let base_key = f.key.split('#').next().unwrap_or(&f.key).to_string();
        let est_roi = if f.price > 0.0 {
            Some((f.reference - f.price) / f.price)
        } else {
            None
        };
        let row = PostedRow {
            uuid: f.uuid.clone(),
            item_name: f.item_name.clone(),
            finder: f.finder.clone(),
            buy: f.price,
            reference: f.reference,
            item_uuid: f.attrs.item_uuid.clone(),
            base_key: Some(base_key),
            delivered,
            est_roi,
            conf: Some(f.confidence),
            samples: Some(f.samples),
            volatility: f.median_stats.as_ref().map(|m| m.volatility),
        };
        // Queue instead of INSERTing here: this runs inside `emit`, on the sweep
        // loop, and one autocommit INSERT per flip made emit 12-32ms of a 19-42ms
        // `eval` -- delaying evaluation of every auction found after it.
        // The overlay keeps the flood brake honest until the row lands.
        if row.delivered {
            if let Some(bk) = &row.base_key {
                *self
                    .pending_held
                    .lock()
                    .unwrap()
                    .entry(bk.clone())
                    .or_insert(0) += 1;
            }
        }
        if self.posted_tx.lock().unwrap().send(row).is_err() {
            eprintln!("posted writer: channel closed, flip record dropped");
        }
    }

    /// Flush the cost-basis map to disk (called on a cadence by the loop).
    pub fn flush_cost_basis(&self) {
        self.cost_basis.lock().unwrap().flush();
    }

    /// Do we have a recorded purchase price for this physical item?
    ///
    /// The proof that an item is genuinely OURS. `handle_dump`'s self-relist
    /// linkage needs it because its candidate set comes from `bought_at IS NOT
    /// NULL`, which only means the flagged auction ended, bought by anyone.
    pub fn have_cost_basis(&self, item_uuid: &str) -> bool {
        self.cost_basis
            .lock()
            .unwrap()
            .cost_for(Some(item_uuid))
            .is_some()
    }

    /// Mark an auction as one of ours, so the self-buy guard never buys it back.
    /// Exposed for the sweep-side self-listing linkage in `handle_dump`, which
    /// spots our relists in the dump instead of polling for them over the API.
    pub fn record_own_listing(&self, auction_uuid: Option<&str>, item_uuid: Option<&str>) {
        self.cost_basis
            .lock()
            .unwrap()
            .record_own_listing(auction_uuid, item_uuid);
    }

    fn welcome_json(&self) -> String {
        let ft = self.filters.read().unwrap();
        let mut v = serde_json::to_value(&*ft).unwrap_or(json!({}));
        if let Some(obj) = v.as_object_mut() {
            obj.remove("token");
        }
        json!({ "type": "welcome", "filters": v }).to_string()
    }
}

/// Handle one incoming client message. Sync; sends replies via `tx`. The
/// pricing RPCs (`estimate`/`inventory`) are handled by [`handle_pricing_message`]
/// (async — they await the loop thread) and are routed there by the reader loop.
fn handle_message(shared: &Arc<WsShared>, client: &Arc<Client>, msg: &Value) {
    let ty = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "listed" => {
            let auction_uuid = msg.get("auctionUuid").and_then(|u| u.as_str());
            let item_uuid = msg.get("itemUuid").and_then(|u| u.as_str());
            shared
                .cost_basis
                .lock()
                .unwrap()
                .record_own_listing(auction_uuid, item_uuid);
            if let (Some(a), Some(fu)) =
                (auction_uuid, msg.get("flipUuid").and_then(|u| u.as_str()))
            {
                let item_name = msg.get("itemName").and_then(|u| u.as_str());
                let _ = shared
                    .store
                    .lock()
                    .unwrap()
                    .record_listing_uuid(a, fu, item_name);
            }
        }
        "purse" => {
            if client.is_lister {
                return;
            }
            let Some(purse_raw) = msg.get("purse").and_then(|p| p.as_f64()) else {
                return;
            };
            if !purse_raw.is_finite() {
                return;
            }
            let purse = purse_raw.max(0.0).floor();
            let locked = msg
                .get("locked")
                .and_then(|p| p.as_f64())
                .filter(|x| x.is_finite())
                .map(|x| x.max(0.0).floor())
                .unwrap_or(0.0);
            let worth = msg
                .get("worth")
                .and_then(|p| p.as_f64())
                .filter(|x| x.is_finite())
                .map(|x| x.max(0.0).floor())
                .unwrap_or(purse + locked);
            let inv_free = msg
                .get("invFree")
                .and_then(|p| p.as_f64())
                .filter(|x| x.is_finite())
                .map(|x| x.max(0.0).floor());
            let auctions = msg
                .get("auctions")
                .and_then(|p| p.as_f64())
                .filter(|x| x.is_finite())
                .map(|x| x.max(0.0).floor());
            let inv_used = msg
                .get("invUsed")
                .and_then(|p| p.as_f64())
                .filter(|x| x.is_finite())
                .map(|x| x.max(0.0).floor());
            let auction_at_limit = msg.get("auctionAtLimit").and_then(|p| p.as_bool());
            *client.status.lock().unwrap() = Some(ClientStatus {
                purse,
                worth,
                inv_free,
                auctions,
                inv_used,
                auction_at_limit,
            });
        }
        // Hypixel's own bazaar confirmations, which the mod already forwards
        // here. `data` is a STRING holding a JSON array of chat lines. This is
        // the finder's only ground truth for whether an order actually reached
        // the book -- see `bazaar_finder::BazaarChat`. No-op unless BZ_FINDER=1.
        "chatBatch" | "chat" => {
            let lines: Vec<String> = match msg.get("data") {
                Some(Value::String(s)) => {
                    serde_json::from_str(s).unwrap_or_else(|_| vec![s.clone()])
                }
                Some(Value::Array(a)) => a
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
                _ => Vec::new(),
            };
            crate::bazaar_finder::note_chat(&client.name, &lines);
        }
        "ping" => {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let _ = client
                .tx
                .send(Message::Text(json!({"type":"pong","ts":ts}).to_string()));
        }
        _ => {}
    }
}

fn is_non_auctionable(id: &str) -> bool {
    let up = id.to_uppercase();
    NON_AUCTIONABLE_IDS.contains(&up.as_str())
}

/// Handle a pricing RPC (`estimate` / `inventory`). Async: forwards the pricing
/// work to the loop thread (which owns the index) and awaits the reply, so the
/// reader loop stays responsive (this runs in a spawned task).
async fn handle_pricing_message(shared: &Arc<WsShared>, client: &Arc<Client>, msg: &Value) {
    match msg.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "estimate" => {
            let Some(uuid) = msg.get("uuid").and_then(|u| u.as_str()).map(str::to_string) else {
                return;
            };
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            if !shared.filters.read().unwrap().listing_recommendations {
                let _ = client.tx.send(Message::Text(
                    json!({"type":"estimate","id":id,"ok":false,"reason":"listing recommendations disabled"}).to_string(),
                ));
                return;
            }
            let (tx, rx) = oneshot::channel();
            if shared
                .rpc_tx
                .send(RpcRequest::Estimate { uuid, reply: tx })
                .is_err()
            {
                let _ = client.tx.send(Message::Text(
                    json!({"type":"estimate","id":id,"ok":false,"reason":"unknown auction"})
                        .to_string(),
                ));
                return;
            }
            let est = rx.await.ok().flatten();
            let reply = match est {
                // jnum: `target` is one of the fields the mod reads with as_u64().
                Some(e) => json!({"type":"estimate","id":id,"ok":true,"estimate":{
                    "target": jnum(e.target), "lbin": jnum_opt(e.lbin), "volumePerDay": jnum_opt(e.volume_per_day),
                    "confidence": jnum(e.confidence), "samples": e.samples, "key": e.key
                }}),
                None => json!({"type":"estimate","id":id,"ok":false,"reason":"unknown auction"}),
            };
            let _ = client.tx.send(Message::Text(reply.to_string()));
        }
        "inventory" => {
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            let force = msg.get("force").and_then(|f| f.as_bool()).unwrap_or(false);
            let items: &[Value] = msg
                .get("items")
                .and_then(|i| i.as_array())
                .map(|v| v.as_slice())
                .unwrap_or(&[]);

            // The bazaar finder confirms its buy orders filled by seeing the
            // units land here. No-op unless BZ_FINDER=1.
            crate::bazaar_finder::note_inventory(&client.name, items);

            // Flood-brake reconcile FIRST: bought-but-unsold items no longer in the
            // real inventory have left it → stop counting toward maxHeldPerBase.
            let present: HashSet<String> = items
                .iter()
                .filter_map(|it| attrs_from_inventory_slot(it).and_then(|a| a.item_uuid))
                .collect();
            let freed = shared
                .store
                .lock()
                .unwrap()
                .reconcile_held_against_inventory(&present, 180)
                .unwrap_or(0);
            if freed > 0 {
                eprintln!("flood-brake reconcile: {freed} held items marked gone (left inventory)");
            }

            if !shared.filters.read().unwrap().listing_recommendations && !force {
                let _ = client.tx.send(Message::Text(
                    json!({"type":"listInstructions","id":id,"items":[],"skipped":[]}).to_string(),
                ));
                return;
            }

            // Decode each priceable item, then batch-price via the loop thread.
            let mut decoded: Vec<(&Value, ItemAttributes)> = items
                .iter()
                .filter_map(|it| attrs_from_inventory_slot(it).map(|a| (it, a)))
                .collect();
            // Restore any variant we could see when we BOUGHT the item but cannot
            // see now. A lore-only component (`LORE_WEIGHT_ITEMS`) never survives
            // the round trip: the mod sends ExtraAttributes only, so an 85M heavy
            // Loudmouth Bass decodes here as a plain one and would be listed
            // against the 1.20M pooled median. Only fills an EMPTY variant, so it
            // can never override something the item genuinely carries.
            if !LORE_WEIGHT_ITEMS.is_empty() {
                let store = shared.store.lock().unwrap();
                for (_, a) in decoded.iter_mut() {
                    if !a.variant.is_empty() || !LORE_WEIGHT_ITEMS.iter().any(|i| *i == a.id) {
                        continue;
                    }
                    if let Some(u) = a.item_uuid.clone() {
                        if let Some(v) = store.bought_variant(&u) {
                            a.variant = v;
                        }
                    }
                }
            }
            let decoded = decoded;
            let (tx, rx) = oneshot::channel();
            if shared
                .rpc_tx
                .send(RpcRequest::PriceAttrs {
                    attrs: decoded.iter().map(|(_, a)| a.clone()).collect(),
                    reply: tx,
                })
                .is_err()
            {
                let _ = client.tx.send(Message::Text(json!({"type":"listInstructions","id":id,"force":force,"items":[],"skipped":[]}).to_string()));
                return;
            }
            let ests = rx.await.unwrap_or_default();

            let now = now_ms();
            let min_conf = shared.filters.read().unwrap().min_confidence;
            let mut instructions: Vec<Value> = Vec::new();
            let mut skipped: Vec<(String, String)> = Vec::new();
            // INV-FUNNEL: why held items never get listed. The `skipped` vec above only
            // fills when force=true, so on the normal (force=false) path every drop was
            // silent and "received 46, priced 0" named no cause. Counters are pure
            // instrumentation — no gate changes here.
            let (mut d_nonauc, mut d_noprice, mut d_samples, mut d_conf) =
                (0usize, 0usize, 0usize, 0usize);
            // Held items priced DESPITE failing the confidence gate (LIST_LOW_CONF).
            let mut d_lowconf_listed = 0usize;
            // What we actually ask, versus the market. See LIST-PRICE below.
            let mut floor_bound = 0usize;
            let mut list_vs_market: Vec<f64> = Vec::new();
            // Failed-listing counts among items priced this cycle, so LIST_FAIL_DAYS
            // is observable rather than inferred. Empty while nothing has re-failed.
            let mut fail_hist: Vec<f64> = Vec::new();
            let mut mode_counts: HashMap<String, usize> = HashMap::new();
            // Worst-case confidence among items that ONLY the confidence gate rejected:
            // tells us how far minConfidence would have to move to release them.
            let mut conf_max_rejected: f64 = 0.0;
            for ((it, attrs), est) in decoded.iter().zip(ests) {
                let name = it
                    .get("displayName")
                    .and_then(|x| x.as_str())
                    .or_else(|| it.get("name").and_then(|x| x.as_str()))
                    .map(str::to_string)
                    .unwrap_or_else(|| attrs.id.clone());
                let id_tag = it
                    .get("tag")
                    .and_then(|x| x.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| attrs.id.clone());
                if is_non_auctionable(&id_tag) {
                    d_nonauc += 1;
                    continue;
                }
                let Some(est) = est else {
                    d_noprice += 1;
                    if force {
                        skipped.push((name, "no price data for this item".to_string()));
                    }
                    continue;
                };
                if est.basis.as_deref() != Some("lbin") && est.samples < MIN_SAMPLES {
                    d_samples += 1;
                    if force {
                        skipped.push((name, format!("not enough samples ({})", est.samples)));
                    }
                    continue;
                }
                let item_uuid = attrs.item_uuid.clone();
                let first_seen: Option<f64> = item_uuid.as_ref().map(|u| {
                    let mut lt = shared.listing_tracker.lock().unwrap();
                    *lt.entry(u.clone()).or_insert(now)
                });
                let (paid, acquired) = {
                    let cb = shared.cost_basis.lock().unwrap();
                    (
                        cb.cost_for(item_uuid.as_deref()),
                        cb.cost_recorded_at_for(item_uuid.as_deref())
                            .map(|x| x as f64),
                    )
                };
                // The confidence gate is a BUY-side question ("am I sure enough
                // this is worth X to spend coins on it") applied on the SELL
                // side, where the alternative to listing at an uncertain price
                // is holding forever and realising nothing. It is the single
                // biggest reason held items never get listed: measured over
                // 21,466 cycles, 64.3% of real held items were dropped here and
                // only 1.2% were ever priced at all.
                //
                // With a known cost basis it is safe to price anyway, because
                // `price_inventory`'s cost floor still refuses to list under
                // paid*1.05 until the item has sat in clearance for days, and
                // the clearance ladder then walks it down to the live lbin. So
                // the worst case is listing high and not selling, which is
                // exactly what skipping already guarantees.
                let low_conf = est.confidence < min_conf;
                if low_conf && !force && !(*LIST_LOW_CONF && paid.is_some()) {
                    d_conf += 1;
                    conf_max_rejected = conf_max_rejected.max(est.confidence);
                    continue;
                }
                if low_conf && !force {
                    d_lowconf_listed += 1;
                }
                // Has this exact item failed to sell before? Read the counter
                // BEFORE pricing, bump it only when the gap is long enough to mean
                // a real listing came and went, and write the new ask back after.
                let failed_listings = match item_uuid.as_deref() {
                    Some(u) => {
                        let mut la = shared.list_attempts.lock().unwrap();
                        match la.get_mut(u) {
                            Some((_, at, fails)) => {
                                if now - *at >= *LIST_FAIL_MIN_GAP_H * 3_600_000.0 {
                                    *fails += 1.0;
                                    *at = now;
                                }
                                *fails
                            }
                            None => 0.0,
                        }
                    }
                    None => 0.0,
                };
                let pricing = price_inventory(&InventoryPricingInput {
                    target: est.target,
                    lbin: est.lbin,
                    basis: est.basis.clone(),
                    paid,
                    acquired_at_ms: acquired,
                    fallback_first_seen_ms: first_seen,
                    volume_per_day: est.volume_per_day,
                    market_median: est.market_median,
                    variant_priced: est.variant_priced,
                    failed_listings,
                    now_ms: now,
                });
                if failed_listings > 0.0 {
                    fail_hist.push(failed_listings);
                }
                if let Some(u) = item_uuid.as_deref() {
                    let mut la = shared.list_attempts.lock().unwrap();
                    la.entry(u.to_string())
                        .and_modify(|e| e.0 = pricing.list_at)
                        .or_insert((pricing.list_at, now, 0.0));
                }
                // What ask we are ACTUALLY sending, versus the market we can see.
                // Every listing-price conclusion so far has been inferred by
                // reading `price_inventory` rather than observed, and inference
                // is how `target*1.05` survived: the opening ask was never in a
                // log. Cheap (a ratio per priced item) and it makes "are we
                // listing above market" answerable directly.
                if pricing.list_at <= pricing.cost_floor {
                    floor_bound += 1;
                }
                if let Some(l) = est.lbin {
                    if l > 0.0 {
                        list_vs_market.push(pricing.list_at / l);
                    }
                }
                *mode_counts.entry(pricing.mode.clone()).or_insert(0usize) += 1;
                // A FRESH listing far under our own valuation, itemised.
                //
                // `LIST-PRICE` above ratios listAt against `lbin`, so it is blind
                // to exactly this: on 2026-08-14 a Midas' Sword bought at 5.00M
                // against a 9,999,999 reference (key MIDAS_SWORD~bid=0, 63
                // samples) was listed at 6.00M — 0.60x our own target, and around
                // the 12th percentile of that band's 677 real sales (p50 7.20M,
                // 7d p50 9.999M). Our own finder then re-flagged our listing as a
                // 57% ROI flip off the same key, which is the tell that the two
                // sides disagreed while holding the same item.
                //
                // Only `normal` mode: the clearance ladder is SUPPOSED to walk the
                // ask down, and floor-bound items are already counted. So this
                // fires only when a first ask is unexplainably low, and prints the
                // inputs that decide it — which basis won, whether the key forked,
                // and whether the market anchor or a self-referential lbin pulled
                // it down.
                if pricing.mode == "normal"
                    && pricing.list_at > pricing.cost_floor
                    && est.target > 0.0
                    && pricing.list_at < est.target * 0.85
                {
                    eprintln!(
                        "LIST-UNDER: {} listAt {:.0} = {:.2}x target {:.0} | basis {} key {} \
                         variant_priced {} | lbin {} market_median {} | paid {} floor {:.0} \
                         conf {:.2} samples {}",
                        name,
                        pricing.list_at,
                        pricing.list_at / est.target,
                        est.target,
                        est.basis.as_deref().unwrap_or("none"),
                        est.key,
                        est.variant_priced,
                        est.lbin.map(|l| format!("{l:.0}")).unwrap_or("-".into()),
                        est.market_median
                            .map(|m| format!("{m:.0}"))
                            .unwrap_or("-".into()),
                        paid.map(|p| format!("{p:.0}")).unwrap_or("-".into()),
                        pricing.cost_floor,
                        est.confidence,
                        est.samples,
                    );
                }
                // jnum on EVERY number here: the mod reads `slot` and `listAt` with
                // as_u64() (main.rs:3584/3901), which is None for a float — that is
                // what printed "Mythos Boots -> 0 coins" on 2026-07-15.
                instructions.push(json!({
                    "slot": jnum(it.get("slot").and_then(|s| s.as_f64()).unwrap_or(0.0)),
                    "name": name,
                    "id": id_tag,
                    "listAt": jnum(pricing.list_at.round()),
                    "confidence": jnum((est.confidence * 1000.0).round() / 1000.0),
                    "volumePerDay": jnum(est.volume_per_day.unwrap_or(0.0)),
                }));
            }
            if !list_vs_market.is_empty() || floor_bound > 0 {
                list_vs_market
                    .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let q = |f: f64| -> f64 {
                    if list_vs_market.is_empty() {
                        return 0.0;
                    }
                    list_vs_market[((list_vs_market.len() as f64 - 1.0) * f).round() as usize]
                };
                let mut modes: Vec<String> = mode_counts
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                modes.sort();
                // Re-failures are the whole basis for LIST_FAIL_DAYS, so report them
                // even when the knob is off: that is how we find out whether the
                // ladder acceleration would ever have anything to bite on.
                let refail = if fail_hist.is_empty() {
                    String::new()
                } else {
                    let mut f = fail_hist.clone();
                    f.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    format!(
                        " | re-failed {} (med {:.0} max {:.0}, +{:.0}% ladder)",
                        f.len(),
                        f[f.len() / 2],
                        f[f.len() - 1],
                        100.0 * 0.02 * *LIST_FAIL_DAYS * f[f.len() / 2],
                    )
                };
                eprintln!(
                    "LIST-PRICE: priced {} | floor-bound {} | listAt/market p10 {:.2} p50 {:.2} p90 {:.2} | above-market {} | {}{}",
                    instructions.len(),
                    floor_bound,
                    q(0.10),
                    q(0.50),
                    q(0.90),
                    list_vs_market.iter().filter(|r| **r > 1.0).count(),
                    modes.join(" "),
                    refail,
                );
            }
            // Drop tracker entries for items no longer in inventory (present set).
            shared
                .listing_tracker
                .lock()
                .unwrap()
                .retain(|k, _| present.contains(k));
            // `list_attempts` gets the opposite treatment on purpose: an item is
            // absent from `present` for the entire 6h it is on the auction house,
            // which is exactly the interval it exists to measure. Age it out well
            // past any plausible hold instead, so a sold item's entry cannot
            // resurrect a stale failure count onto a later purchase.
            shared
                .list_attempts
                .lock()
                .unwrap()
                .retain(|_, (_, at, _)| now - *at < 30.0 * 24.0 * 3_600_000.0);
            if !items.is_empty() {
                eprintln!("inventory priced for listing: received {}, priced {}, skipped {}, force={force}", items.len(), instructions.len(), skipped.len());
                // Only worth a line when something was actually held back.
                let dropped = d_nonauc + d_noprice + d_samples + d_conf;
                if dropped > 0 || d_lowconf_listed > 0 {
                    // `undecodable` = slots attrs_from_inventory_slot could not read at all,
                    // so they never even reached the gates below.
                    eprintln!(
                        "INV-FUNNEL: received {} decoded {} undecodable {} | priced {} (lowconf {}) | dropped {} = nonauctionable {} noprice {} samples<{} {} conf<{:.2} {} (best rejected conf {:.3})",
                        items.len(),
                        decoded.len(),
                        items.len().saturating_sub(decoded.len()),
                        instructions.len(),
                        d_lowconf_listed,
                        dropped,
                        d_nonauc,
                        d_noprice,
                        MIN_SAMPLES,
                        d_samples,
                        min_conf,
                        d_conf,
                        conf_max_rejected,
                    );
                }
            }
            // Collapse duplicate skip lines (N copies of the same unpriceable item).
            let mut dedup: IndexMap<String, (String, String, usize)> = IndexMap::new();
            for (n, r) in skipped {
                let key = format!("{n} {r}");
                dedup
                    .entry(key)
                    .and_modify(|e| e.2 += 1)
                    .or_insert((n, r, 1));
            }
            let skipped_json: Vec<Value> = dedup
                .values()
                .map(|(n, r, c)| json!({"name": n, "reason": if *c > 1 { format!("{r} (×{c})") } else { r.clone() }}))
                .collect();
            // An Err here is the ONE path where a recommendation is built and
            // then lost: the funnel counted these items as `priced`, but the
            // client disconnected before the frame. Naming the loss so the
            // INV-FUNNEL deltas are attributable. Rare (a disconnect mid-RPC),
            // so the log cost is nil at the boundaries it matters.
            if client
                .tx
                .send(Message::Text(
                    json!({"type":"listInstructions","id":id,"force":force,"items":instructions,"skipped":skipped_json}).to_string(),
                ))
                .is_err()
            {
                eprintln!(
                    "listInstructions: {} instruction(s) lost — {} disconnected before the reply",
                    instructions.len(),
                    client.name
                );
            }
        }
        _ => {}
    }
}

/// Stable content hash for AH dedupe: item identity + price (+ item uuid when
/// present), so a listing re-seen on a later page or sweep is skipped.
fn ah_content_hash(it: &Value, attrs: &ItemAttributes, bin: u64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    attrs.id.hash(&mut h);
    if let Some(u) = &attrs.item_uuid {
        u.hash(&mut h);
    } else if let Some(dn) = it.get("displayNameColored").and_then(|x| x.as_str()) {
        dn.hash(&mut h);
    }
    bin.hash(&mut h);
    h.finish()
}

/// Page-flipper ingest. A crawler (never a buyer) sends `{type:"ahPage", items:[…]}`
/// snapshots of the live Auction Browser. We decode + price each auction OFF the
/// API-flip latency path — via the same between-dumps `PriceAttrs` RPC the
/// inventory pricer uses, so the single-threaded index and the dump→push path are
/// never contended — and when a crawled BIN is underpriced vs its estimate, push
/// it to the buyer bots tagged `finder="pageflipper"` (through the exact same push
/// gates as an API flip). All logging is prefixed "baf pageflipper" so a
/// page-found flip is obvious on the finder line.
async fn handle_ahpage_message(shared: &Arc<WsShared>, msg: &Value) {
    let items: &[Value] = msg
        .get("items")
        .and_then(|i| i.as_array())
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    if items.is_empty() {
        return;
    }

    struct Cand {
        attrs: ItemAttributes,
        bin: f64,
        name: String,
        seller: Option<String>,
        auction_uuid: Option<String>,
    }

    // When the crawler read this page (ms epoch) — needed below both for the
    // webhook latency field and for the raw sighting log (moved up from where it
    // used to be read, after cand-building, so the sighting insert can use it).
    let crawl_ts = msg.get("ts").and_then(|t| t.as_u64());
    let page = msg.get("page").and_then(|p| p.as_u64()).unwrap_or(0) as u32;

    // Decode + dedupe: keep only NEW, priceable BIN auctions.
    let mut cands: Vec<Cand> = Vec::new();
    // Every priceable BIN candidate gets a raw sighting logged, regardless of
    // whether it ends up profitable or clears any gate below — this is the only
    // record of "pageflipper laid eyes on this listing", used to check later
    // whether a flip COFL bought was ever seen by our own crawler, and when.
    let mut sightings: Vec<(String, i64, String, i64, i64)> = Vec::new();
    {
        let mut seen = shared.ah_seen.lock().unwrap();
        if seen.len() > 400_000 {
            seen.clear();
        }
        for it in items {
            let Some(bin) = it.get("binPrice").and_then(|p| p.as_u64()) else {
                continue;
            };
            if bin == 0 {
                continue;
            }
            let Some(attrs) = attrs_from_inventory_slot(it) else {
                continue;
            };
            if !seen.insert(ah_content_hash(it, &attrs, bin)) {
                continue; // already analyzed this listing
            }
            let name = it
                .get("displayName")
                .and_then(|x| x.as_str())
                .or_else(|| it.get("name").and_then(|x| x.as_str()))
                .unwrap_or(&attrs.id)
                .to_string();
            // A real auction uuid (if the crawled item carries one) is required for
            // the buyer to `/viewauction`; without it we recover it from the dump
            // or the seller lookup below.
            let auction_uuid = it
                .get("auctionUuid")
                .or_else(|| it.get("auctionId"))
                .and_then(|x| x.as_str())
                .map(str::to_string);
            let seller = it
                .get("seller")
                .and_then(|x| x.as_str())
                .map(str::to_string);
            sightings.push((
                name.clone(),
                bin as i64,
                seller.clone().unwrap_or_default(),
                crawl_ts
                    .map(|t| t as i64)
                    .unwrap_or_else(|| now_ms() as i64),
                page as i64,
            ));
            cands.push(Cand {
                attrs,
                bin: bin as f64,
                name,
                seller,
                auction_uuid,
            });
        }
    }
    if !sightings.is_empty() {
        let store = shared.store.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = store.lock().unwrap().record_pf_sightings(&sightings) {
                eprintln!("baf pageflipper: failed to record sightings: {e}");
            }
        });
    }
    if cands.is_empty() {
        return;
    }

    // Price + resolve on the loop thread (between dumps, off the latency path).
    // Resolution recovers a buyable auction uuid from the live dump when the
    // listing is already in it; the seller → NetherAPI per-player lookup (to be
    // layered on) covers listings that are not in the dump yet.
    let (tx, rx) = oneshot::channel();
    if shared
        .rpc_tx
        .send(RpcRequest::PriceAndResolve {
            items: cands.iter().map(|c| (c.attrs.clone(), c.bin)).collect(),
            reply: tx,
        })
        .is_err()
    {
        return;
    }
    let resolved = rx.await.unwrap_or_default();
    let now = now_ms();
    // Optional per-crawler webhook: only flips that actually pass the filter and are
    // sent to a buyer get posted here (no observed-only / filtered noise). Rich embeds.
    let webhook = msg
        .get("webhook")
        .and_then(|w| w.as_str())
        .filter(|s| s.starts_with("http"))
        .map(str::to_string);
    let mut wh_embeds: Vec<Value> = Vec::new();

    let mut candidates = 0u32;
    let mut direct: Vec<(String, PageflipPending)> = Vec::new(); // uuid already known (dump / item)
    let mut by_seller: HashMap<String, Vec<PageflipPending>> = HashMap::new(); // need NetherAPI lookup

    for (c, r) in cands.iter().zip(resolved) {
        let Some(est) = r.est else { continue };
        if est.basis.as_deref() != Some("lbin") && est.samples < MIN_SAMPLES {
            continue;
        }
        let reference = est.target;
        // After-tax proceeds − buy price = raw profit (same tax tiers the sniper uses).
        let tax = if reference >= 100_000_000.0 {
            0.025
        } else if reference >= 10_000_000.0 {
            0.02
        } else {
            0.01
        };
        let profit = reference * (1.0 - tax) - c.bin;
        if profit <= 0.0 {
            continue;
        }
        let roi = if c.bin > 0.0 {
            profit / c.bin * 100.0
        } else {
            0.0
        };
        candidates += 1;

        let uuid = r.uuid.or_else(|| c.auction_uuid.clone());
        let pending = PageflipPending {
            attrs: c.attrs.clone(),
            item_name: c.name.clone(),
            key: est.key.clone(),
            bin: c.bin,
            reference,
            profit,
            roi,
            confidence: est.confidence,
            samples: est.samples,
            lbin: est.lbin,
            volume_per_day: est.volume_per_day,
            seller: c.seller.clone(),
            crawl_ts,
            found_ts: now,
            page,
        };
        // Route the flip. A dump-resolved uuid pushes for free. Otherwise a seller
        // lookup costs a scarce NetherAPI request, so only spend one when the flip
        // would ACTUALLY be bought — probe the exact filter push_flip applies, and
        // skip the lookup for anything the buyers would reject.
        let status = match (uuid, &c.seller) {
            (Some(u), _) => {
                direct.push((u, pending));
                "buyable (in dump)".to_string()
            }
            (None, Some(seller)) => {
                let probe = pageflip_to_flip(&pending, String::new(), now);
                match shared.filter_reject_reason(&probe) {
                    None => {
                        by_seller.entry(seller.clone()).or_default().push(pending);
                        format!("seller lookup: {seller}")
                    }
                    Some(why) => format!("filtered — no lookup ({why})"),
                }
            }
            (None, None) => "observed only (no seller)".to_string(),
        };
        eprintln!(
            "baf pageflipper: {} — bin {} vs target {} → profit {} ({:.0}% ROI), conf {:.2}, samples {}, {}",
            c.name,
            fmt_coins(c.bin),
            fmt_coins(reference),
            fmt_coins(profit),
            roi,
            est.confidence,
            est.samples,
            status
        );
    }

    // Already in the dump → buyable immediately, push now. Only flips that actually
    // reach a buyer go to the webhook.
    let mut pushed = 0u32;
    for (uuid, p) in direct {
        let (delivered, outcome) = push_pageflip(shared, &p, uuid.clone(), now);
        if delivered > 0 {
            pushed += 1;
            if webhook.is_some() {
                wh_embeds.push(pf_flip_embed(&p, &outcome, "in dump", delivered, &uuid));
            }
        }
    }

    // Not in the dump yet → recover the uuid from the seller's live AH via
    // NetherAPI. One blocking lookup per seller (returns their whole AH, so all
    // their flagged items resolve from a single request), off the async reader.
    // Only flips that already passed the filter probe are queued here.
    let queued: usize = by_seller.values().map(|v| v.len()).sum();
    for (seller, pendings) in by_seller {
        let shared = shared.clone();
        let wh = webhook.clone();
        tokio::task::spawn_blocking(move || {
            resolve_seller_and_push(&shared, &seller, pendings, now, wh)
        });
    }

    if candidates > 0 {
        eprintln!(
            "baf pageflipper: sweep {} page {} — {} candidate(s), {} pushed now, {} queued for seller lookup",
            msg.get("sweep").and_then(|s| s.as_u64()).unwrap_or(0),
            msg.get("page").and_then(|s| s.as_u64()).unwrap_or(0),
            candidates,
            pushed,
            queued
        );
    }

    // Report this page's passing flips to the crawler's webhook (one batched Discord
    // post per page so it can't get rate-limited per-flip). Fire-and-forget.
    if let (Some(url), false) = (webhook, wh_embeds.is_empty()) {
        tokio::task::spawn_blocking(move || post_discord_embeds(&url, wh_embeds));
    }
}

/// Best-effort Discord webhook POST (`{"content": …}`, truncated to Discord's 2000
/// arbitrary JSON body. Blocking; call from a blocking task. Shared by the embed
/// poster and any plain-text post.
fn post_discord_json(url: &str, body: &Value) {
    static C: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    let client = C.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .build()
            .expect("webhook client")
    });
    let _ = client.post(url).json(body).send();
}

/// Everything needed to build + push a pageflipper flip once its auction uuid is known.
struct PageflipPending {
    attrs: ItemAttributes,
    item_name: String,
    key: String,
    bin: f64,
    reference: f64,
    profit: f64,
    roi: f64,
    confidence: f64,
    samples: i64,
    lbin: Option<f64>,
    /// Liquidity for this key, straight off the pricing RPC. MUST be carried into
    /// the Flip's `median_stats` — the BinMaster filter tiers on volume, so dropping
    /// it made every pageflipper flip look volume-less and get judged far more
    /// strictly than the identical API flip.
    volume_per_day: Option<f64>,
    /// Seller name off the crawled tooltip (for the embed + fresh-lookup path).
    seller: Option<String>,
    /// Epoch-ms the crawler read this page (`ahPage.ts`); with `found_ts` gives the
    /// crawl→found latency shown on the webhook.
    crawl_ts: Option<u64>,
    /// Epoch-ms the finder priced + found this flip.
    found_ts: f64,
    /// AH page index the flip was crawled from.
    page: u32,
}

/// Build a `pageflipper`-tagged `Flip` from a pending + its (possibly placeholder)
/// auction uuid. Used both to push a resolved flip and to build the pre-lookup
/// `passes_filter` probe (with an empty uuid), so the probe and the real push are
/// evaluated against an identical flip.
fn pageflip_to_flip(p: &PageflipPending, uuid: String, now: f64) -> Flip {
    Flip {
        uuid,
        item_name: p.item_name.clone(),
        finder: "pageflipper".to_string(),
        price: p.bin,
        reference: p.reference,
        profit: p.profit,
        roi_pct: p.roi,
        confidence: p.confidence,
        samples: p.samples,
        key: p.key.clone(),
        guard: "pageflipper".to_string(),
        found_after_refresh_ms: 0.0,
        found_at_ms: now,
        attrs: p.attrs.clone(),
        // Carry the key's liquidity through. The BinMaster filter tiers on
        // volume_per_day, so leaving this None made every crawled flip look
        // volume-less and get judged far more strictly than the same flip found off
        // the API (a 21% ROI median flip pushes, while pageflipper hit `roi<25%`).
        // Fields the crawl path can't know are left neutral, same as the snipe
        // finder does in finder-core.
        median_stats: p.volume_per_day.map(|v| KeyStats {
            target: p.reference,
            samples: p.samples,
            volume_per_day: v,
            spread_pct: 0.0,
            lowest_ref: p.reference,
            highest_ref: p.reference,
            last_sold_ago_h: 0.0,
            confidence: p.confidence,
            volatility: 0.0,
            manipulated: false,
            trend_pct: 0.0,
        }),
    }
}

/// Build a `pageflipper`-tagged flip and push it through the normal gates.
/// Returns `(buyers_delivered, human outcome)` — the outcome shows on the webhook
/// so it's obvious whether a flip was actually bought or why it was filtered.
fn push_pageflip(
    shared: &Arc<WsShared>,
    p: &PageflipPending,
    uuid: String,
    now: f64,
) -> (usize, String) {
    let flip = pageflip_to_flip(p, uuid, now);
    let res = shared.push_flip(&flip, p.lbin, None);
    shared.record_posted(&flip, res.delivered > 0);
    if res.delivered > 0 {
        eprintln!(
            "baf pageflipper -> pushed {} to {} buyer(s)",
            flip.item_name, res.delivered
        );
        (
            res.delivered,
            format!("✅ sent to {} bot(s)", res.delivered),
        )
    } else {
        let reason = res
            .mismatch
            .unwrap_or_else(|| "no buyer connected".to_string());
        eprintln!(
            "baf pageflipper: {} not pushed — {}",
            flip.item_name, reason
        );
        (0, format!("❌ filtered: {reason}"))
    }
}

/// The real SkyBlock item id for display. `attrs.id` is "PET" for every pet, so
/// substitute the pet type (e.g. `PET_MITHRIL_GOLEM`); otherwise use the id as-is.
fn pf_item_id(attrs: &ItemAttributes) -> String {
    if let Some(pet) = &attrs.pet {
        if !pet.pet_type.is_empty() {
            return format!("PET_{}", pet.pet_type.to_uppercase());
        }
    }
    attrs.id.to_uppercase()
}

/// Epoch-ms → `HH:MM:SS.mmm` (UTC), for an exact found/crawl time on the webhook.
fn pf_hms_millis(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_else(chrono::Utc::now)
        .format("%H:%M:%S%.3f")
        .to_string()
}

/// Build a rich Discord embed for one delivered flip — every detail worth seeing:
/// prices, profit/ROI, confidence + samples, item id, source (in-dump vs the
/// pre-dump seller lookup), seller, page, exact found time, the crawl→found latency,
/// and how many buyers it went to. `source` is "in dump" / "pre-dump (fresh …)".
fn pf_flip_embed(
    p: &PageflipPending,
    outcome: &str,
    source: &str,
    delivered: usize,
    uuid: &str,
) -> Value {
    // Crawl → found latency: how long after the crawler read the page we found+pushed
    // it. NOTE this is our INTERNAL pipeline latency, not a head start on the market —
    // an "in dump" flip is already public, so the API/COFL path sees it too. Only a
    // "pre-dump (fresh)" source is a genuine timing edge.
    let latency = p
        .crawl_ts
        .map(|ts| format!("{} ms", (p.found_ts as i64 - ts as i64).max(0)))
        .unwrap_or_else(|| "n/a".to_string());
    // Real SkyBlock item id: for pets `attrs.id` is just "PET", so use the pet type.
    let item_id = pf_item_id(&p.attrs);
    // Green when it actually went to a buyer, amber otherwise (shouldn't happen here).
    let color = if delivered > 0 { 0x2ecc71 } else { 0xf1c40f };
    // Exact wall-clock (UTC) with milliseconds — for comparing against COFL/API times.
    let found_exact = pf_hms_millis(p.found_ts as i64);
    let mut fields = vec![
        json!({ "name": "Buy",        "value": fmt_coins(p.bin),       "inline": true }),
        json!({ "name": "Sells for",  "value": fmt_coins(p.reference), "inline": true }),
        json!({ "name": "Profit",     "value": format!("**+{}**", fmt_coins(p.profit)), "inline": true }),
        json!({ "name": "ROI",        "value": format!("{:.0}%", p.roi), "inline": true }),
        json!({ "name": "Confidence", "value": format!("{:.0}% ({} samples)", p.confidence * 100.0, p.samples), "inline": true }),
        json!({ "name": "Source",     "value": source, "inline": true }),
        json!({ "name": "Found (UTC)", "value": format!("`{found_exact}`"), "inline": true }),
        json!({ "name": "Crawl→found","value": latency, "inline": true }),
        json!({ "name": "Page",       "value": format!("{}", p.page), "inline": true }),
        json!({ "name": "Buyers",     "value": format!("{delivered}"), "inline": true }),
    ];
    if let Some(ts) = p.crawl_ts {
        fields.push(json!({ "name": "Crawled (UTC)", "value": format!("`{}`", pf_hms_millis(ts as i64)), "inline": true }));
    }
    if let Some(s) = &p.seller {
        fields.push(json!({ "name": "Seller", "value": s, "inline": true }));
    }
    if !item_id.is_empty() {
        fields.push(json!({ "name": "Item id", "value": format!("`{item_id}`"), "inline": true }));
    }
    // Clickable auction link so the exact resolved uuid can be verified against the
    // real listing (wrong/misresolved uuid ⇒ the page shows a different item / 404).
    if !uuid.is_empty() {
        fields.push(json!({
            "name": "Auction",
            "value": format!("[view on Coflnet](https://sky.coflnet.com/auction/{uuid}) · `{uuid}`"),
            "inline": false,
        }));
    }
    // Epoch-ms → ISO8601 for the embed's own timestamp (renders as the day/time).
    let ts_iso = chrono::DateTime::from_timestamp_millis(p.found_ts as i64)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    json!({
        "title": format!("💰 {}", p.item_name),
        "description": outcome,
        "color": color,
        "fields": fields,
        "footer": { "text": "BAF Pageflipper" },
        "timestamp": ts_iso,
    })
}

/// Best-effort Discord webhook POST of up to 10 rich embeds (Discord's per-message
/// cap). Blocking; call from a blocking task. Chunks if there are more than 10.
fn post_discord_embeds(url: &str, embeds: Vec<Value>) {
    for chunk in embeds.chunks(10) {
        let body = json!({
            "username": "BAF Pageflipper",
            "avatar_url": "https://mc-heads.net/avatar/MHF_ArrowRight",
            "embeds": chunk,
        });
        post_discord_json(url, &body);
    }
}

/// Blocking: resolve a seller name → uuid (Mojang) → pull their live AH via
/// NetherAPI (the per-player endpoint runs ahead of the dump), match each flagged
/// item by price (then name), and push the buyable ones. Runs on the blocking
/// pool so it never sits on the async reader; the whole-AH fetch means all of one
/// seller's flagged items cost a single request.
fn resolve_seller_and_push(
    shared: &Arc<WsShared>,
    seller_name: &str,
    pendings: Vec<PageflipPending>,
    now: f64,
    webhook: Option<String>,
) {
    let Some(key) = nether_api_key() else { return };
    let Some(uuid) = crate::hypixel::resolve_player_uuid(seller_name) else {
        eprintln!("baf pageflipper: couldn't resolve seller '{seller_name}' → uuid");
        return;
    };
    let Some(auctions) = crate::hypixel::fetch_player_auctions(&uuid, &key, now as i64) else {
        return;
    };
    let mut wh_embeds: Vec<Value> = Vec::new();
    for p in &pendings {
        let mut matches: Vec<&crate::hypixel::RawAuction> = auctions
            .iter()
            .filter(|a| a.bin && (a.starting_bid - p.bin).abs() < 1.0)
            .collect();
        if matches.len() > 1 {
            let want = p.item_name.to_lowercase();
            matches.retain(|a| {
                let n = a.item_name.to_lowercase();
                n == want || n.contains(&want) || want.contains(&n)
            });
        }
        match matches.first() {
            Some(a) => {
                eprintln!(
                    "baf pageflipper: resolved {} via seller {seller_name} → auction {}",
                    p.item_name, a.uuid
                );
                // Passed the filter probe already; only a successful send reaches the webhook.
                let (delivered, outcome) = push_pageflip(shared, p, a.uuid.clone(), now);
                if webhook.is_some() && delivered > 0 {
                    wh_embeds.push(pf_flip_embed(
                        p,
                        &outcome,
                        "pre-dump (fresh via seller)",
                        delivered,
                        &a.uuid,
                    ));
                }
            }
            None => {
                eprintln!(
                    "baf pageflipper: {} not in {seller_name}'s live AH (sold/gone)",
                    p.item_name
                );
            }
        }
    }
    if let (Some(url), false) = (webhook, wh_embeds.is_empty()) {
        post_discord_embeds(&url, wh_embeds);
    }
}

/// The seller-lookup API key from env (cached). Same vars as seller-follow, so one
/// `API_KEY` (or `SELLER_FOLLOW_API_KEY`) serves both.
fn nether_api_key() -> Option<String> {
    static KEY: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    KEY.get_or_init(|| {
        std::env::var("SELLER_FOLLOW_API_KEY")
            .or_else(|_| std::env::var("API_KEY"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    })
    .clone()
}

/// Start the async ws flip feed. Binds WS_HOST:WS_PORT (default 127.0.0.1:15101).
#[allow(clippy::result_large_err)] // tungstenite's accept_hdr callback dictates the ErrorResponse type
pub async fn start_ws_server(shared: Arc<WsShared>) -> std::io::Result<()> {
    let port: u16 = std::env::var("WS_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15101);
    let host = std::env::var("WS_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let listener = TcpListener::bind((host.as_str(), port)).await?;
    eprintln!("ws flip feed listening on {host}:{port}");
    // pf_sightings only needs to cover the short window between a pageflipper
    // crawl and a COFL purchase report — prune anything older than 6h so the
    // table can't grow unbounded over days of uptime.
    {
        let store = shared.store.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30 * 60)).await;
                let cutoff = now_ms() as i64 - 6 * 3_600_000;
                match store.lock().unwrap().prune_pf_sightings(cutoff) {
                    Ok(n) if n > 0 => eprintln!("baf pageflipper: pruned {n} stale sighting(s)"),
                    Ok(_) => {}
                    Err(e) => eprintln!("baf pageflipper: sighting prune failed: {e}"),
                }
            }
        });
    }
    // NOTE: the per-bot NetherAPI self-listing poll that used to live here is gone.
    // It fetched every connected lister's own auctions every 45s purely to fill
    // `listing_uuids`, which cost one API request per bot per 45s (~1.33/min each,
    // most of a 90-per-5-minute key budget with a full lane of bots) and starved
    // the seller-follow lookups that actually need the budget. A bot's relist is a
    // brand-new BIN, so it already arrives in the page-0 dump the sweep fetches for
    // free: `note_own_relist` in main.rs's handle_dump does the same item-uuid
    // match there, one indexed query per sweep, no API requests, and sees the
    // listing in ~7s instead of up to 45s.
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => continue,
        };
        let _ = stream.set_nodelay(true);
        let shared = shared.clone();
        tokio::spawn(async move {
            // Peek the request path for token/role query params during handshake.
            let mut role_lister = false;
            let mut token_ok = true;
            let mut player: Option<String> = None;
            let ws = tokio_tungstenite::accept_hdr_async(
                stream,
                |req: &tokio_tungstenite::tungstenite::handshake::server::Request, resp| {
                    let uri = req.uri().to_string();
                    let q = uri.split_once('?').map(|x| x.1).unwrap_or("");
                    let mut token = None;
                    for kv in q.split('&') {
                        let mut it = kv.splitn(2, '=');
                        match (it.next(), it.next()) {
                            (Some("role"), Some(v)) => role_lister = v == "lister",
                            (Some("player"), Some(v)) => player = Some(v.to_string()),
                            (Some("token"), Some(v)) => token = Some(v.to_string()),
                            _ => {}
                        }
                    }
                    let cfg_token = shared.filters.read().unwrap().token.clone();
                    if !cfg_token.is_empty() && token.as_deref() != Some(cfg_token.as_str()) {
                        token_ok = false;
                    }
                    Ok(resp)
                },
            )
            .await;
            let ws = match ws {
                Ok(w) if token_ok => w,
                _ => return, // bad token or handshake failure
            };
            let (mut write, mut read) = ws.split();
            let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
            let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
            let name = player.unwrap_or_else(|| {
                format!(
                    "anon-{}",
                    shared.anon_seq.fetch_add(1, Ordering::Relaxed) + 1
                )
            });
            let client = Arc::new(Client {
                is_lister: role_lister,
                tx: tx.clone(),
                status: Mutex::new(None),
                name,
            });
            shared.clients.lock().unwrap().insert(id, client.clone());
            // Welcome.
            let _ = tx.send(Message::Text(shared.welcome_json()));
            // Writer task: drain outbound queue → socket.
            let writer = tokio::spawn(async move {
                while let Some(m) = rx.recv().await {
                    if write.send(m).await.is_err() {
                        break;
                    }
                }
            });
            // Reader loop.
            while let Some(Ok(msg)) = read.next().await {
                match msg {
                    Message::Text(t) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
                            if ty == "estimate" || ty == "inventory" {
                                // Pricing RPCs await the loop thread — run detached
                                // so this client's reader stays responsive.
                                let (s, c) = (shared.clone(), client.clone());
                                tokio::spawn(
                                    async move { handle_pricing_message(&s, &c, &v).await },
                                );
                            } else if ty == "ahPage" {
                                // Page-flipper crawl snapshot: decode + price off the
                                // latency path, then push any pageflipper flips.
                                let s = shared.clone();
                                tokio::spawn(async move { handle_ahpage_message(&s, &v).await });
                            } else {
                                handle_message(&shared, &client, &v);
                            }
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => {}
                    _ => {}
                }
            }
            // Cleanup.
            shared.clients.lock().unwrap().remove(&id);
            writer.abort();
        });
    }
}

#[cfg(test)]
mod jnum_tests {
    //! The mod reads price/target/listAt with serde_json `as_u64()`, which returns
    //! None for ANY float. Emitting `1848997.0` therefore made the bot parse 0 and
    //! silently drop every flip (2026-07-15 cutover, rolled back). TS never hit it
    //! because JS `JSON.stringify` writes integral numbers without a decimal point.
    //! These tests pin the wire format, which is the actual contract with the mod.
    use super::*;

    /// What the mod does to our payload. If this returns None, the flip is DROPPED.
    fn mod_reads_u64(v: &Value) -> Option<u64> {
        v.as_u64()
    }

    /// Guards the WHOLE wire surface, not just the fields I remembered to fix.
    /// Walks a payload and asserts no money/slot field is a float, because the mod
    /// reads every one of these with as_u64() (grep as_u64 in frikadellen-baf-121:
    /// duration/end/highest_bid/item_slot/listAt/price/slot/starting_bid/target/
    /// time_remaining_seconds/until).
    fn assert_no_float_u64_fields(v: &Value, path: &str) {
        const U64_FIELDS: &[&str] = &[
            "price",
            "target",
            "listAt",
            "slot",
            "item_slot",
            "starting_bid",
            "highest_bid",
            "until",
            "end",
            "duration",
            "time_remaining_seconds",
            "profit",
            "lbin",
            "foundAtMs",
            "dumpDetectedAtMs",
            // Not read with as_u64(), but the mod's custom deserializer uses
            // as_i64() and ERRORS on a float, which drops the whole flip.
            "purchaseAt",
        ];
        match v {
            Value::Object(m) => {
                for (k, val) in m {
                    if U64_FIELDS.contains(&k.as_str()) && val.is_f64() {
                        panic!("{path}.{k} is a float ({val}) — the mod's as_u64() will read 0 and DROP this");
                    }
                    assert_no_float_u64_fields(val, &format!("{path}.{k}"));
                }
            }
            Value::Array(a) => {
                for (i, val) in a.iter().enumerate() {
                    assert_no_float_u64_fields(val, &format!("{path}[{i}]"));
                }
            }
            _ => {}
        }
    }

    #[test]
    fn no_ws_payload_ships_a_float_where_the_mod_wants_u64() {
        // flip payload, built the way flip_payload does
        let flip = json!({"type":"flip","flip":{
            "uuid":"u","itemName":"i","finder":"median",
            "price": jnum(1_848_997.0), "target": jnum(18_489_998.0), "profit": jnum(16_271_201.0),
            "roiPct": jnum(880.0), "confidence": jnum(0.9), "samples": 541,
            "volumePerDay": jnum_opt(Some(77.44)), "lbin": jnum_opt(Some(18_489_998.0)),
            "key":"ABICASE","guard":"clean_snipe","listAt": jnum_opt(Some(16_825_898.0)),
            "foundAtMs": jnum(1_784_156_202_997.0), "foundAfterRefreshMs": jnum(8876.0),
            "dumpDetectedAtMs": Value::from(1_784_156_202_997i64),
        }});
        assert_no_float_u64_fields(&flip, "flip");

        // listInstructions item
        let li = json!({"type":"listInstructions","id":Value::Null,"force":false,"items":[
            json!({"slot": jnum(9.0), "name":"Mythos Boots", "id":"MYTHOS_BOOTS",
                   "listAt": jnum(12_345_678.0), "confidence": jnum(0.9), "volumePerDay": jnum(3.5)})
        ],"skipped":[]});
        assert_no_float_u64_fields(&li, "listInstructions");

        // estimate
        let est = json!({"type":"estimate","id":1,"ok":true,"estimate":{
            "target": jnum(18_700_000.0), "lbin": jnum_opt(Some(23_187_500.0)),
            "volumePerDay": jnum_opt(Some(22.79)), "confidence": jnum(0.843),
            "samples": 78, "key": "K"
        }});
        assert_no_float_u64_fields(&est, "estimate");
    }

    #[test]
    fn the_detector_actually_detects() {
        // A test that cannot fail is worthless: prove the guard above catches the
        // real 2026-07-15 payload.
        let bad = json!({"flip":{"price": 1_848_997.0_f64}});
        let caught = std::panic::catch_unwind(|| assert_no_float_u64_fields(&bad, "flip")).is_err();
        assert!(
            caught,
            "the float detector must reject the exact payload that broke prod"
        );
    }

    #[test]
    fn integral_values_serialize_as_ints_not_floats() {
        assert_eq!(jnum(1_848_997.0).to_string(), "1848997");
        assert_eq!(jnum(15_000_000.0).to_string(), "15000000");
        assert_eq!(jnum(0.0).to_string(), "0");
        // The exact failure: a float here is unreadable by the mod.
        assert_eq!(mod_reads_u64(&jnum(1_848_997.0)), Some(1_848_997));
        assert_eq!(
            mod_reads_u64(&Value::from(1_848_997.0_f64)),
            None,
            "proves the bug"
        );
    }

    #[test]
    fn non_integral_values_stay_floats() {
        // roiPct/confidence/volumePerDay are legitimately fractional; the mod reads
        // those with as_f64(), which accepts both.
        assert_eq!(jnum(22.2).to_string(), "22.2");
        assert_eq!(jnum(0.843).to_string(), "0.843");
        assert_eq!(jnum(77.44).to_string(), "77.44");
    }

    #[test]
    fn optional_none_is_null_like_ts() {
        assert_eq!(jnum_opt(None).to_string(), "null");
        assert_eq!(jnum_opt(Some(23_187_500.0)).to_string(), "23187500");
    }

    #[test]
    fn beyond_2pow53_stays_float_rather_than_lying() {
        // f64 cannot represent consecutive ints past 2^53; JS goes exponential.
        // Do not silently truncate — no real price is anywhere near this.
        let huge = 9_007_199_254_740_994.0_f64;
        assert!(jnum(huge).is_f64());
        assert!(jnum(f64::NAN).is_f64() || jnum(f64::NAN).is_null());
    }

    #[test]
    fn ts_reference_payload_is_reproduced_exactly() {
        // Captured from the bot log, TS era (21:xx, pre-cutover), uuid 866f3e55:
        //   "price":15000000,"target":18700000,"profit":3326000,"roiPct":22.2,
        //   "confidence":0.843,"samples":78,"volumePerDay":22.79,"lbin":23187500
        let got = json!({
            "price": jnum(15_000_000.0),
            "target": jnum(18_700_000.0_f64.round()),
            "profit": jnum(3_326_000.0_f64.round()),
            "roiPct": jnum((22.2_f64 * 10.0).round() / 10.0),
            "confidence": jnum((0.843_f64 * 1000.0).round() / 1000.0),
            "samples": 78,
            "volumePerDay": jnum_opt(Some(22.79)),
            "lbin": jnum_opt(Some(23_187_500.0)),
        });
        assert_eq!(
            got.to_string(),
            // Insertion order, not alphabetical: serde_json runs with preserve_order,
            // which is also why field order matches TS's payload byte for byte.
            r#"{"price":15000000,"target":18700000,"profit":3326000,"roiPct":22.2,"confidence":0.843,"samples":78,"volumePerDay":22.79,"lbin":23187500}"#
        );
    }
}

#[cfg(test)]
mod grind_rotation_tests {
    //! Port of the TS `npm run test:grind` behavior (wsServer.ts:620-632, shipped to
    //! prod TS 2026-07-15). The Rust port shipped WITHOUT this on 2026-07-15 and the
    //! ws-config rewrite ate the `grindAssignMode` key on top, so grind flips went
    //! back to broadcasting and every small account raced the same item.
    use super::*;

    /// The flood brake is money-safety: it caps how many unsold copies of one
    /// base_key we will hold. `record_posted` became ASYNC, and the brake reads
    /// the very rows it writes, so without the `pending_held` overlay a second
    /// flip of the same base_key inside one sweep would slip past the cap while
    /// the first row was still queued.
    #[test]
    /// A bot must be told to WAIT only when the bed is genuinely still counting
    /// down. Telling it to wait on a past instant would stall it on a flip it
    /// could buy right now, and every dump flip is past that point by
    /// construction (the dump withholds a BIN for the full grace period).
    #[test]
    fn purchase_at_is_future_only_and_recorder_drops_stale() {
        // Unique per RUN, not just per pid: this test is compiled into both the
        // lib and bin harnesses, which can hold the same path open at once and
        // fail with "database is locked".
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bedgrace-{}-{stamp}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("t.sqlite").to_string_lossy().to_string();
        let store = Arc::new(Mutex::new(Store::open(&db, false).expect("store")));
        let (rpc_tx, _rpc_rx) = mpsc::unbounded_channel();
        let sh = WsShared::new(
            WsFilterConfig::default(),
            CostBasis::load(&dir.join("cb.json").to_string_lossy()),
            store,
            2,
            rpc_tx,
        );

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as f64;

        // Never recorded: no timing signal at all.
        assert_eq!(sh.purchase_at_ms("never-seen"), None);

        // Mid-bed: listed 3s ago, so ~17s of grace left.
        sh.record_auction_start("bed", now - 3_000.0, now, false);
        let at = sh
            .purchase_at_ms("bed")
            .expect("mid-bed flip must carry purchaseAt");
        assert!(
            at > now && at <= now + *BED_GRACE_MS,
            "purchaseAt {at} must land inside the remaining grace window"
        );

        // Grace already elapsed: buy now, do NOT make the bot wait.
        // The recorder itself must drop it, so the map only ever holds live beds.
        sh.record_auction_start("old", now - *BED_GRACE_MS - 1_000.0, now, false);
        assert_eq!(sh.purchase_at_ms("old"), None);

        // A zero/absent start must never be treated as "listed at epoch 0".
        sh.record_auction_start("zero", 0.0, now, true);
        assert_eq!(sh.purchase_at_ms("zero"), None);
    }

    /// Calls the REAL `flip_payload`. The float-guard test above hand-builds its
    /// own payload, so it proves nothing about what the function actually emits —
    /// the same mistake that let the seller-follow `item_bytes` shape through.
    #[test]
    fn flip_payload_carries_purchase_at_only_for_a_bed() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bedpayload-{}-{stamp}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("t.sqlite").to_string_lossy().to_string();
        let store = Arc::new(Mutex::new(Store::open(&db, false).expect("store")));
        let (rpc_tx, _rpc_rx) = mpsc::unbounded_channel();
        let sh = WsShared::new(
            WsFilterConfig::default(),
            CostBasis::load(&dir.join("cb.json").to_string_lossy()),
            store,
            2,
            rpc_tx,
        );

        let f = Flip {
            uuid: "bed-uuid".into(),
            item_name: "Jaded Glossy Mineral Leggings".into(),
            finder: "median".into(),
            price: 42.0,
            reference: 36_960_716.0,
            profit: 33_885_542.0,
            roi_pct: 806_798.0,
            confidence: 0.957,
            samples: 79,
            key: "GLOSSY_MINERAL_LEGGINGS#recomb".into(),
            guard: "none".into(),
            found_after_refresh_ms: 0.0,
            found_at_ms: 1_786_555_365_735.0,
            attrs: serde_json::from_str(r#"{"id":"GLOSSY_MINERAL_LEGGINGS"}"#).unwrap(),
            median_stats: None,
        };

        // A dump flip: nothing recorded, so the bot is told to buy now.
        let v: Value = serde_json::from_str(&sh.flip_payload(&f, None, 1.0)).unwrap();
        assert!(
            v["flip"].as_object().unwrap().contains_key("purchaseAt"),
            "the key must always be present, so the mod's parse is unambiguous"
        );
        assert!(v["flip"]["purchaseAt"].is_null(), "dump flip = buy now");

        // Same flip caught mid-bed by seller-follow, listed 3s ago.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as f64;
        sh.record_auction_start("bed-uuid", now - 3_000.0, now, false);
        let v: Value = serde_json::from_str(&sh.flip_payload(&f, None, 1.0)).unwrap();
        let at = v["flip"]["purchaseAt"]
            .as_i64()
            .expect("must be an INTEGER — the mod's as_i64() errors on a float and drops the flip");
        assert!(at > now as i64, "must still be in the future");
        assert!(at <= (now + *BED_GRACE_MS) as i64);
    }

    #[test]
    fn queued_posted_rows_still_count_against_the_flood_brake() {
        let dir = std::env::temp_dir().join(format!("floodbrake-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("t.sqlite");
        let db = db.to_string_lossy().to_string();
        let store = Arc::new(Mutex::new(Store::open(&db, false).expect("store")));
        let (rpc_tx, _rpc_rx) = mpsc::unbounded_channel();
        let sh = WsShared::new(
            WsFilterConfig::default(),
            CostBasis::load(&dir.join("cb.json").to_string_lossy()),
            store.clone(),
            2,
            rpc_tx,
        );

        let base = "HYPERION".to_string();
        let count = |sh: &WsShared| {
            sh.store.lock().unwrap().unsold_held_count(&base)
                + sh.pending_held
                    .lock()
                    .unwrap()
                    .get(&base)
                    .copied()
                    .unwrap_or(0)
        };
        assert_eq!(count(&sh), 0, "clean db starts at zero");

        // Two delivered flips of the same base, as one sweep would emit them.
        for i in 0..2 {
            let mut row = crate::store::PostedRow {
                uuid: format!("u{i}"),
                base_key: Some(base.clone()),
                delivered: true,
                ..Default::default()
            };
            row.item_name = "Hyperion".into();
            if row.delivered {
                if let Some(bk) = &row.base_key {
                    *sh.pending_held
                        .lock()
                        .unwrap()
                        .entry(bk.clone())
                        .or_insert(0) += 1;
                }
            }
            sh.posted_tx.lock().unwrap().send(row).unwrap();
        }
        // Immediately, before the writer thread can commit anything, the brake
        // must ALREADY see both -- that is the whole point.
        assert!(
            count(&sh) >= 2,
            "queued rows must count immediately, got {}",
            count(&sh)
        );

        // Once committed, the total must not double-count.
        for _ in 0..200 {
            if sh.pending_held.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            count(&sh),
            2,
            "after the batch commits the count must stay 2, not 4"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn client(name: &str) -> Arc<Client> {
        let (tx, _rx) = mpsc::unbounded_channel();
        Arc::new(Client {
            is_lister: false,
            tx,
            status: Mutex::new(None),
            name: name.to_string(),
        })
    }

    /// Just the rotation state — no Store/CostBasis needed.
    struct Rot {
        last: Mutex<HashMap<String, u64>>,
        seq: AtomicU64,
    }
    impl Rot {
        fn new() -> Self {
            Rot {
                last: Mutex::new(HashMap::new()),
                seq: AtomicU64::new(0),
            }
        }
        fn pick_grind_target(&self, e: &[Arc<Client>]) -> Arc<Client> {
            pick_grind_target_in(&self.last, &self.seq, e)
        }
    }
    fn shared() -> Rot {
        Rot::new()
    }

    #[test]
    fn rotates_evenly_across_grind_accounts() {
        let s = shared();
        let pool = vec![client("a"), client("b"), client("c")];
        let mut got = Vec::new();
        for _ in 0..9 {
            got.push(s.pick_grind_target(&pool).name.clone());
        }
        // 3 each, never the same account twice in a row with a stable set.
        for n in ["a", "b", "c"] {
            assert_eq!(got.iter().filter(|x| *x == n).count(), 3, "{n} in {got:?}");
        }
    }

    #[test]
    fn stays_fair_when_a_bot_drops_out_of_eligibility() {
        // The reason this is least-recently-served and not an index cursor: the
        // eligible set changes size between flips (purse/status gating), and an
        // index would skip accounts.
        let s = shared();
        let (a, b, c) = (client("a"), client("b"), client("c"));
        s.pick_grind_target(&[a.clone(), b.clone(), c.clone()]); // a
        s.pick_grind_target(&[a.clone(), b.clone(), c.clone()]); // b
                                                                 // c drops out; a and b already served, so the least-recent of them wins.
        let x = s.pick_grind_target(&[a.clone(), b.clone()]).name.clone();
        assert_eq!(x, "a", "least-recently-served must win");
        // c returns having never been served → it sorts first (seq -1).
        let y = s.pick_grind_target(&[a, b, c]).name.clone();
        assert_eq!(y, "c", "an account never served must sort first");
    }

    #[test]
    fn reconnect_keeps_position_because_it_is_keyed_on_player_not_socket() {
        let s = shared();
        let a1 = client("arga_binny");
        let b = client("darcy_lunar");
        assert_eq!(
            s.pick_grind_target(&[a1.clone(), b.clone()]).name,
            "arga_binny"
        );
        // arga reconnects: NEW socket, same ?player= name.
        let a2 = client("arga_binny");
        assert_eq!(
            s.pick_grind_target(&[a2, b]).name,
            "darcy_lunar",
            "a reconnect must not reset the rotation and re-serve the same account"
        );
    }

    #[test]
    fn config_default_is_single_like_ts() {
        // wsServer.ts:155 defaults grindAssignMode to 'single'. A ws-config.json
        // missing the key must round-robin, NOT broadcast.
        let cfg: crate::ws_config::WsFilterConfig =
            serde_json::from_str(r#"{"assignMode":"all"}"#).expect("parses");
        assert_eq!(cfg.grind_assign_mode, "single");
        assert_eq!(
            cfg.assign_mode, "all",
            "regular flips still follow assignMode"
        );
    }
}
