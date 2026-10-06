//! finder-rs — Phase 3 binary.
//!
//! Modes (env):
//!   (default)  boot: load snapshot refs + live bazaar → build PriceIndex + model.
//!   SWEEP=1    live read-only sweep: find flips over the live AH, log pino FLIP lines.
//!   COMPARE=1  live Rust-vs-TS head-to-head: capture one AH dump, run BOTH finders
//!              on identical inputs, and POST the comparison (flips + speed) to
//!              DISCORD_WEBHOOK_URL.
//!   BAZAAR_COLLECT_ONLY=1  run ONLY the bazaar snapshot collector (its own sqlite
//!              file) and exit, for a dedicated collector process/container. When
//!              serving, the collector also auto-spawns unless BAZAAR_COLLECT_OFF=1.
//!
//! READ-ONLY shadow: no bot, no ws feed. Nothing reaches a buyer.

/// The deploy artifact is a **static musl** binary, and musl's malloc serialises
/// on a global lock. That is not a theory: the same `decode_item_bytes` costs
/// 7.8us/auction on glibc and 15.8us on prod's musl, and the split is lopsided —
/// inflate (barely allocates) goes 1.7x while `attrs_from_extra` (an IndexMap plus
/// a `to_lowercase` String per key) goes **3.5x**. Under 4 detect lanes plus the
/// bazaar poller the contention multiplies, which also explains why
/// `DECODE_THREADS` made things *worse* (commit ca9cc7f): more decoding threads
/// meant more contention on one lock, not more throughput.
///
/// Measure with `finder-core/examples/decode_bench` built for
/// `x86_64-unknown-linux-musl` and run ON the box; a glibc host number is
/// meaningless for this.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use finder_core::bazaar::Bazaar;
use finder_core::config::{LBIN_FALLBACK_CAP, LIST_MARKET_MIN_REFS};
use finder_core::modifier_model::ModifierModel;
use finder_core::nbt::{decode_item_bytes, ItemAttributes};
use finder_core::price_index::{base_key, PriceIndex};
use finder_core::sniper::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use ws_server::{AhResolved, ListingEstimate, RpcRequest};

/// Identify ourselves on every outbound API call.
///
/// `reqwest` sends no User-Agent unless told to, so until now every request to
/// Hypixel arrived headless. A high-rate client with no UA is a textbook bot
/// signature to a WAF, and on 2026-07-27 Cloudflare blocked all ten of the
/// box's IPs at once — across five different /24s, which is far too broad for
/// per-IP reputation and points at the client signature instead.
///
/// This is the opposite of hiding: it says who we are and how to reach us, so a
/// rule match can be discussed rather than guessed at.
pub const USER_AGENT: &str = concat!(
    "baf-finder/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/TreXito/baf-finder)"
);

mod bazaar_collect;
mod bazaar_finder;
mod bazaar_ledger;
#[allow(dead_code)] // consumed by the wsServer port (next Phase 3 brick)
mod cost_basis;
mod detect;
mod discord;
mod flip_api;
mod hypixel;
/// Web UI + JSON API letting feed consumers manage their own saved filter.
mod public_api;
/// Public read-only feed of the flips our own filter declined (`PUBLIC_WS=1`).
mod public_ws;
mod recent_flips;
mod seller_follow;
#[allow(dead_code)] // write path consumed by the orchestration loop (in progress)
mod store;
#[allow(dead_code)] // consumed by the orchestration loop (in progress)
mod ws_config;
#[allow(dead_code)] // consumed by the orchestration loop (in progress)
mod ws_server;

fn now_ms_real() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
fn rss_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: f64 = s
        .split_whitespace()
        .nth(1)
        .and_then(|x| x.parse().ok())
        .unwrap_or(0.0);
    pages * 4096.0 / 1_048_576.0
}
fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

#[derive(Serialize, Deserialize, Clone)]
struct FlipOut {
    uuid: String,
    finder: String,
    item: String,
    price: f64,
    reference: f64,
    profit: f64,
    #[serde(rename = "roiPct")]
    roi_pct: f64,
    confidence: f64,
    samples: i64,
    key: String,
    guard: String,
    /// ms after dump release this flip was found = dumpAge + decode + pipeline-elapsed.
    #[serde(rename = "foundAfterMs")]
    found_after_ms: f64,
}
#[derive(Serialize, Deserialize)]
struct RunResult {
    engine: String,
    #[serde(rename = "findMs")]
    find_ms: f64,
    #[serde(rename = "decodeMs")]
    decode_ms: f64,
    #[serde(rename = "dumpAgeMs")]
    dump_age_ms: f64,
    candidates: usize,
    count: usize,
    flips: Vec<FlipOut>,
}

fn flip_out(f: &Flip, found_after_ms: f64) -> FlipOut {
    FlipOut {
        uuid: f.uuid.clone(),
        finder: f.finder.clone(),
        item: f.item_name.clone(),
        price: f.price,
        reference: f.reference,
        profit: f.profit,
        roi_pct: f.roi_pct,
        confidence: f.confidence,
        samples: f.samples,
        key: f.key.clone(),
        guard: f.guard.clone(),
        found_after_ms,
    }
}

fn log_flip_out(
    fo: &FlipOut,
    now_ms: i64,
    tts: Option<&finder_core::price_index::TtsInfo>,
    vol_per_day: Option<f64>,
    // Item-level sell-through, read from a BROADER map than `tts` — that one needs
    // a fair-priced sale carrying a `tts_ms` before the key gets an entry at all,
    // so on its own it hides the number on exactly the thin keys the liquidity
    // haircut exists for.
    item_sell_through: Option<f64>,
) {
    let mut line = serde_json::json!({
        "level": 30, "time": iso(now_ms), "item": fo.item, "finder": fo.finder,
        "price": fo.price, "reference": fo.reference, "profit": fo.profit,
        "roi": format!("{}%", fo.roi_pct.round() as i64), "confidence": fo.confidence,
        "samples": fo.samples, "guard": fo.guard, "foundAfterMs": fo.found_after_ms,
        "key": fo.key, "msg": "FLIP",
    });
    // The haircut actually applied to `reference`, so a log line is enough to tell
    // a discounted quote from an undiscounted one without re-deriving anything.
    if let Some(st) = item_sell_through {
        let r2 = |x: f64| (x * 100.0).round() / 100.0;
        line["itemSellThrough"] = serde_json::json!(r2(st));
        line["liqFactor"] =
            serde_json::json!(r2(finder_core::price_index::liquidity_factor_for(Some(st))));
    }
    // Phase 0 (volume→TTS migration): log the OBSERVED fair-price TTS beside what
    // VOLUME predicts (the filter's fake 24h/volume). Purely for the A/B — nothing
    // gates on it. `ttsBias` = observed / predicted: >1 means volume says faster
    // than reality, <1 means volume WRONGLY rejects a fast-selling niche item.
    if let Some(t) = tts {
        let r2 = |x: f64| (x * 100.0).round() / 100.0;
        line["fairTtsH"] = serde_json::json!(r2(t.fair_tts_h));
        line["ttsNFair"] = serde_json::json!(t.n_fair);
        line["ttsNAll"] = serde_json::json!(t.n_all);
        // The de-biasing half: what fraction of this ITEM's listings sell at all.
        // `fairTtsH` alone is survivorship-biased; read them together or not at all.
        if let Some(st) = t.sell_through {
            line["sellThrough"] = serde_json::json!(r2(st));
            line["ttsNCensored"] = serde_json::json!(t.n_censored);
        }
        if let Some(v) = vol_per_day {
            if v > 0.0 {
                let pred = 24.0 / v;
                line["volPredTtsH"] = serde_json::json!(r2(pred));
                if pred > 0.0 {
                    line["ttsBias"] = serde_json::json!(r2(t.fair_tts_h / pred));
                }
            }
        }
    }
    if let Some(v) = vol_per_day {
        line["volPerDay"] = serde_json::json!((v * 100.0).round() / 100.0);
    }
    println!("{line}");
}

/// Decode BIN auctions → DecodedAuction (with finalKey), timed.
fn decode_all(auctions: &[hypixel::RawAuction], idx: &PriceIndex) -> (Vec<DecodedAuction>, f64) {
    let t = Instant::now();
    let decoded = auctions
        .iter()
        .filter(|a| a.bin && a.starting_bid > 0.0 && !a.item_bytes.is_empty())
        .filter_map(|a| {
            decode_item_bytes(&a.item_bytes).map(|attrs| {
                let key = idx.final_key(&attrs);
                DecodedAuction {
                    a: ActiveAuction {
                        uuid: a.uuid.clone(),
                        starting_bid: a.starting_bid,
                        auctioneer: a.auctioneer.clone(),
                        item_name: a.item_name.clone(),
                    },
                    attrs,
                    key,
                }
            })
        })
        .collect();
    (decoded, t.elapsed().as_secs_f64() * 1000.0)
}

/// Cross-sweep state.
///
/// Prod keeps every one of these at module scope and prunes them to the live-BIN
/// set at each sweep end: `seen` (index.ts:25/864), `primed` (29/879),
/// `prevLiveUuids` (49/867), `prevByKey` (50/868), the decode/key caches
/// (865/866), and the relist-spam tracker (sniper.ts:83).
///
/// This is NOT incidental bookkeeping — it is load-bearing for money decisions:
///   * `prev_by_key` is what snipe/median price against (index.ts:574/588); the
///     CURRENT dump's map would let a flood of sibling listings from the same
///     dump undercut each other into different decisions.
///   * `relist` only blocks seller spam if it accumulates ACROSS sweeps.
///   * `seen` + `prev_live_uuids` are why a flip fires once, instead of being
///     re-pushed to the bots every dump for as long as it stays listed.
#[derive(Default)]
struct SweepMemory {
    seen: HashSet<String>,
    relist: RelistTracker,
    prev_by_key: HashMap<String, Vec<Bin>>,
    prev_live_uuids: HashSet<String>,
    decode_cache: HashMap<String, ItemAttributes>,
    key_cache: HashMap<String, String>,
    primed: bool,
}

/// Evaluate one genuinely-new BIN: snipe lane, then median lane, else it falls
/// through to the lbin/dominance candidates that run once the full dump is in.
/// Verbatim shape of index.ts:571-601.
#[allow(clippy::too_many_arguments)]
/// Splits `eval` (the biggest slice of our own compute — 16-54ms at a 21-day
/// window, vs decode 3ms / feed 3ms / parse 5ms) into its two halves, so the next
/// person optimising it aims instead of guesses. Two `Instant::now()` per
/// auction is ~20ns each against a ~100-270us eval, so it is always on.
/// Read via `P0SPLIT ... snipe N median N`.
thread_local! {
    static EVAL_PROF: std::cell::Cell<(u128, u128, u128)> = const { std::cell::Cell::new((0, 0, 0)) };
}

fn eval_prof_take() -> (f64, f64, f64) {
    EVAL_PROF.with(|c| {
        let (a, b, e) = c.replace((0, 0, 0));
        (a as f64 / 1000.0, b as f64 / 1000.0, e as f64 / 1000.0)
    })
}

fn eval_new(
    d: DecodedAuction,
    idx: &PriceIndex,
    model: &ModifierModel,
    prev_by_key: &HashMap<String, Vec<Bin>>,
    seen: &mut HashSet<String>,
    relist: &mut RelistTracker,
    last_updated: f64,
    now_ms: f64,
    lbin_c: &mut Vec<DecodedAuction>,
    emit: &mut dyn FnMut(&Flip),
) {
    // Prod screened in the page worker to skip 'r'/'l' verdicts before the main
    // thread; this single-box port has no worker, so it runs the full eval on
    // every candidate. The screen is deliberately permissive (no false
    // negatives), so full eval is decision-identical. The prior `let _ =
    // screen_auction(...)` here was pure dead work: its verdict was discarded and
    // eval_clean_snipe/eval_median_flip below redo the identical price lookups.
    let t = Instant::now();
    let snipe = eval_clean_snipe(&d, idx, prev_by_key, seen, relist, last_updated, now_ms);
    let d_snipe = t.elapsed().as_micros();
    if let Some(f) = snipe {
        let t = Instant::now();
        emit(&f);
        let d_emit = t.elapsed().as_micros();
        EVAL_PROF.with(|c| {
            let (a, b, e) = c.get();
            c.set((a + d_snipe, b, e + d_emit))
        });
        return;
    }
    let t = Instant::now();
    let (flip, priceable) = eval_median_flip(
        &d,
        idx,
        model,
        prev_by_key,
        seen,
        relist,
        last_updated,
        now_ms,
    );
    let d_median = t.elapsed().as_micros();
    let mut d_emit = 0u128;
    let flip_emitted = flip.is_some();
    if let Some(f) = flip {
        let t = Instant::now();
        emit(&f);
        d_emit = t.elapsed().as_micros();
    }
    EVAL_PROF.with(|c| {
        let (a, b, e) = c.get();
        c.set((a + d_snipe, b + d_median, e + d_emit))
    });
    if flip_emitted {
        return;
    }
    if !priceable {
        lbin_c.push(d); // lbin runs after the full dump is in
    }
}

/// Filter-layer rejection funnel: of the flips the finder EMITTED, why did the
/// push filter (BinMaster tiers / ws-config / routing) drop them? Complements the
/// eval-layer funnel in sniper.rs — together they show the full "missing flips"
/// picture. Categorised from the mismatch string push_flip already produces.
#[derive(Default)]
struct FilterFunnel {
    global: u32,
    roi: u32,
    profit: u32,
    volume: u32,
    confidence: u32,
    tts: u32,
    guard: u32,
    blacklist: u32,
    flood: u32,
    routing: u32,
    other: u32,
}

fn bump_filter_miss(ff: &std::cell::RefCell<FilterFunnel>, m: &str) {
    let ml = m.to_lowercase();
    let mut f = ff.borrow_mut();
    // "below global" carries profit/roi/conf substrings, so it MUST be tested first.
    if ml.contains("below global") {
        f.global += 1;
    } else if ml.contains("roi") {
        f.roi += 1;
    } else if ml.contains("slow to sell") || ml.contains("time to sell") {
        f.tts += 1;
    } else if ml.contains("vol") {
        f.volume += 1;
    } else if ml.contains("conf") {
        f.confidence += 1;
    } else if ml.contains("guard") {
        f.guard += 1;
    } else if ml.contains("blacklist") {
        f.blacklist += 1;
    } else if ml.contains("holding") {
        f.flood += 1;
    } else if ml.contains("profit") {
        f.profit += 1;
    } else if ml.contains("purse")
        || ml.contains("slot")
        || ml.contains("client")
        || ml.contains("eligible")
        || ml.contains("auction house full")
        || ml.contains("own listing")
    {
        f.routing += 1;
    } else {
        f.other += 1;
    }
}

/// One detected dump → the full sweep.
///
/// Page-0's new BINs are priced AS THEY STREAM (prod does the same, index.ts:433):
/// the lane pushes each chunk's decoded bins over the channel and they are
/// evaluated here while the rest of the body is still downloading. Waiting for the
/// finished body instead would cost the whole drain (~94ms median) before the first
/// flip could fire. Then the deep pages, then the lanes that need the whole dump,
/// then the cross-sweep state rolls.
/// Push one emitted flip through the full serving path (filter → routing → record
/// → webhook → log) and return the push result. Shared by the sweep's streaming
/// emit and the seller-follow path so a followed listing is served identically to
/// a page-sweep flip. `lbin` is the previous sweep's second-cheapest for the key
/// (what the bot lists against), exactly as index.ts:276 reads it.
fn post_flip(
    f: &Flip,
    prev_by_key: &HashMap<String, Vec<Bin>>,
    idx: &PriceIndex,
    shared: &ws_server::WsShared,
    recent: &std::sync::Arc<std::sync::Mutex<recent_flips::RecentFlips>>,
    discord: &discord::Discord,
) -> ws_server::PushResult {
    let lbin = prev_by_key
        .get(&f.key)
        .and_then(|l| l.iter().find(|x| x.uuid != f.uuid))
        .map(|x| x.price);
    // Real fair-TTS for the Phase-1 liquidity gate (idx is in scope here; the ws
    // layer isn't). Harmless when TTS_LIQUIDITY is off: the filter ignores it.
    let tts = idx.tts_info(&f.key).map(|t| ws_server::TtsForFilter {
        fair_tts_h: t.fair_tts_h,
        n_fair: t.n_fair,
        sell_through: t.sell_through,
    });
    // Our own share of "found -> bot has it". Measured against bot log lines that
    // path is p50 ~87ms with a 25ms floor, which is absurd for loopback with
    // TCP_NODELAY already set -- so it has to be split. This is everything the
    // finder does before the bytes are queued to the writer task; whatever is
    // left over is the bot's own scheduling, parse and logging.
    let t_push = Instant::now();
    let res = shared.push_flip(f, lbin, tts);
    let push_us = t_push.elapsed().as_micros();
    if push_us > 2_000 {
        eprintln!(
            "SLOWPUSH {}us uuid={} key={} clients={}",
            push_us, f.uuid, f.key, res.delivered
        );
    }
    shared.record_posted(f, res.delivered > 0);
    // index.ts:316: postFlip remembers EVERY posted flip, including small grind
    // ones the Discord webhook filters out. baf-backend reads these.
    recent.lock().unwrap().remember(f, now_ms_real());
    // index.ts:319: small grind flips (profit < hardMinProfit) still went to the
    // bots above, but are kept OUT of the webhook so it isn't flooded.
    // Stamp the EXACT emit instant here, not in the webhook drain task (which
    // posts up to MIN_GAP_MS later) and not from `found_at_ms` (the sweep start).
    let ctx = discord::FoundCtx {
        exact_ms: now_ms_real(),
        purchase_at_ms: shared.purchase_at_ms(&f.uuid),
        api_visible_at_ms: shared.api_visible_at_ms(&f.uuid),
    };
    // index.ts:319: small grind flips (profit < hardMinProfit) still went to the
    // bots above, but are kept OUT of the webhook so it isn't flooded.
    // ⚠️ BEDS ARE EXEMPT. A bed is the one thing we see that nobody else can yet,
    // so it is worth seeing in #found even when the profit gate rejects it — that
    // rejection is exactly what we are trying to observe. Volume is ~40/day, not
    // the ~1900/day of ordinary follow listings, so this cannot flood the channel.
    if shared.webhook_worthy(f) || ctx.is_pre_api() {
        discord.enqueue(f, Some(&res), ctx);
    }
    let vol = f.median_stats.as_ref().map(|m| m.volume_per_day);
    log_flip_out(
        &flip_out(f, f.found_after_refresh_ms),
        now_ms_real(),
        idx.tts_info(&f.key),
        vol,
        idx.sell_through_for_key(&f.key),
    );
    res
}

#[allow(clippy::too_many_arguments)]
fn handle_dump(
    start: detect::DumpStart,
    rx: &std::sync::mpsc::Receiver<detect::DumpMsg>,
    mem: &mut SweepMemory,
    idx: &PriceIndex,
    model: &ModifierModel,
    shared: &ws_server::WsShared,
    store: &std::sync::Arc<std::sync::Mutex<store::Store>>,
    dsh: &std::sync::Arc<detect::Shared>,
    page_concurrency: usize,
    recent: &std::sync::Arc<std::sync::Mutex<recent_flips::RecentFlips>>,
    discord: &discord::Discord,
    follow_tx: &std::sync::mpsc::Sender<String>,
) {
    let now_ms = now_ms_real();
    let nm = now_ms as f64;
    let last_updated = start.last_updated as f64;
    let t_pipe = Instant::now();
    shared.set_sweep_detect_at(start.detect_at);
    // Anchor for the 60s publish metronome (phase drifts, so observe it).
    shared.set_last_dump_lu(start.last_updated as i64);
    // Clear any residue so the median rejection funnel counts THIS sweep only.
    let _ = finder_core::sniper::reject_take();

    let SweepMemory {
        seen,
        relist,
        prev_by_key,
        prev_live_uuids,
        decode_cache,
        key_cache,
        primed,
    } = mem;

    // A half-built carried-over state must rebuild silently rather than alert on
    // 45k "new" BINs (index.ts:501).
    let alerting = *primed && prev_live_uuids.len() > 20_000;
    let tts_capture = std::env::var("TTS_CAPTURE")
        .map(|v| v != "0")
        .unwrap_or(true);

    let mut live_bins: HashSet<String> = HashSet::with_capacity(64_000);
    let mut all_decoded: Vec<DecodedAuction> = Vec::new();
    let mut lbin_c: Vec<DecodedAuction> = Vec::new();
    let mut old_bins: Vec<(String, f64)> = Vec::new();
    let mut new_listings: Vec<store::ListingRow> = Vec::new();
    let mut pushed = 0usize;
    let mut delivered = 0usize;
    // ms from detection to our first flip == prod's `firstFlipMs`
    // (index.ts:917 = firstFlipAtMs - fetchStart). THE number to compare.
    let mut first_flip_ms: i64 = -1;

    // postFlip: the lbin shown to the bot comes from the PREVIOUS sweep's map
    // (index.ts:276 reads module-level prevByKey, which is only rolled at 868 —
    // after every lane has run), so this stays on prev_by_key all sweep.
    let filter_funnel = std::cell::RefCell::new(FilterFunnel::default());
    // Seller-follow (liquidation catcher): snapshot the switch once per sweep so
    // the uuid→seller map below is only populated when the feature is on (zero cost
    // when off). The map lets emit_count recover the seller for a shipped flip (the
    // Flip carries no auctioneer), so we can watch them for the rest of their gear.
    let follow_enabled = shared.filters.read().unwrap().seller_follow;
    let uuid_seller: std::cell::RefCell<HashMap<String, String>> =
        std::cell::RefCell::new(HashMap::new());
    // Self-listing linkage, the sweep-side replacement for the per-bot NetherAPI
    // poll: a bot's relist is a brand-new BIN, so it arrives in THIS dump on page
    // 0. One indexed query yields the bought-but-unlisted purchases by item uuid;
    // any dumped auction carrying one of those item uuids is our own listing of
    // that flip. Costs no API requests and sees the listing in ~7s instead of up
    // to 45s.
    //
    // ⚠️ This used to run HERE, at the top of the sweep, and it was pure
    // first-flip latency: measured on prod 2026-08-08 the query is **15.6ms
    // median** (5,264 rows, 183k-row `posted` joined against a 20k `listing_uuids`
    // NOT IN), and the loop thread sat on it before draining a single BIN --
    // against a `firstNewDecMs` of 1ms and a FIRSTFLIPMS of 42ms. It bought ~37%
    // of our time-to-first-flip for bookkeeping that no flip decision reads.
    //
    // Nothing in the flip path consumes it: `own_relists` is only drained at sweep
    // end to write `listing_uuids` rows and register own-listings. So both the
    // query and the matching now run at sweep end over `all_decoded`, which
    // collects exactly the BINs the per-BIN closure used to see (every
    // `note_own_relist` call site was immediately preceded by an `all_decoded`
    // push). Same rows written, off the critical path.
    let mut emit_count = |f: &Flip| {
        let res = post_flip(f, prev_by_key, idx, shared, recent, discord);
        if let Some(m) = &res.mismatch {
            bump_filter_miss(&filter_funnel, m);
        }
        // Seller-follow: a flip whose profit cleared the trigger bar means this
        // seller is dumping value, so watch them for the rest of their gear even if
        // THIS flip was filtered out (conf/roi/BinMaster). The point is to catch a
        // liquidation, and the first piece is often the one that trips a gate. Bar =
        // sellerFollowMinProfit, or hardMinProfit when 0.
        if follow_enabled {
            let trig = {
                let ft = shared.filters.read().unwrap();
                if ft.seller_follow_min_profit > 0.0 {
                    ft.seller_follow_min_profit
                } else {
                    ft.hard_min_profit
                }
            };
            if trig > 0.0 && f.profit >= trig {
                if let Some(seller) = uuid_seller.borrow().get(&f.uuid) {
                    let _ = follow_tx.send(seller.clone());
                }
            }
        }
        if first_flip_ms < 0 {
            first_flip_ms = now_ms_real() - start.detect_at;
        }
        if res.mismatch.is_none() {
            pushed += 1;
            delivered += res.delivered;
        }
    };

    // ---- PAGE 0, STREAMING: price each chunk's new BINs the moment they land,
    //      while the rest of the body is still downloading. ----
    let mut n_new_p0 = 0usize;
    // Page-0 phase attribution. p0_ms alone cannot say whether we are waiting on
    // the network or burning CPU, and on the CloudFanatic box the burst peaks at
    // 60% of ONE core with 8 idle -- so the question is which line owns the wall
    // time, not how much CPU is left. Cheap: three Instants per new BIN.
    let mut t_wait_us: u128 = 0;
    let mut t_key_us: u128 = 0;
    let mut t_eval_us: u128 = 0;
    // `wait + key + eval` left a 38ms p50 hole in the 135ms page-0 phase (measured
    // over 1307 sweeps, 2026-08-08) and a hole is where latency hides. These two
    // name the rest of it: `book` is the per-BIN bookkeeping between keying and
    // eval (the cache inserts, the `all_decoded` clone, the live_bins probe),
    // `oldmerge` is the carried-over merge that runs after the stream ends. Three
    // extra Instants per BIN is ~20ns each against a ~70us eval.
    let mut t_book_us: u128 = 0;
    let mut t_oldmerge_us: u128 = 0;
    let end = loop {
        // A stalled body is bounded by the lane's own request timeout; this is
        // only a backstop so a dead lane can't wedge the loop forever.
        let t_w = Instant::now();
        let msg = rx.recv_timeout(Duration::from_secs(90));
        t_wait_us += t_w.elapsed().as_micros();
        match msg {
            Ok(detect::DumpMsg::Bins(batch)) => {
                for b in batch {
                    let t_b0 = Instant::now();
                    if !live_bins.insert(b.a.uuid.clone()) {
                        t_book_us += t_b0.elapsed().as_micros();
                        continue; // dedupe (index.ts:516-519)
                    }
                    n_new_p0 += 1;
                    t_book_us += t_b0.elapsed().as_micros();
                    let t_k = Instant::now();
                    let key = idx.final_key(&b.attrs);
                    t_key_us += t_k.elapsed().as_micros();
                    let t_b = Instant::now();
                    decode_cache.insert(b.a.uuid.clone(), b.attrs.clone());
                    key_cache.insert(b.a.uuid.clone(), key.clone());
                    // TTS (B1): the single funnel every genuinely-new BIN passes
                    // exactly once. Batched to disk at sweep end (index.ts:534/992).
                    if tts_capture && b.start > 0.0 {
                        new_listings.push(store::ListingRow {
                            auction_id: b.a.uuid.clone(),
                            start: b.start as i64,
                            item_id: b.attrs.id.clone(),
                        });
                    }
                    let d = DecodedAuction {
                        a: b.a,
                        attrs: b.attrs,
                        key,
                    };
                    // A dump BIN is USUALLY past its grace window, but not always
                    // — prod bots hit 69 beds overnight with a null `purchaseAt`.
                    // No-ops unless this one is genuinely still in grace.
                    shared.record_auction_start(&d.a.uuid, b.start, nm, false);
                    all_decoded.push(d.clone());
                    if follow_enabled {
                        if let Some(s) = &d.a.auctioneer {
                            uuid_seller.borrow_mut().insert(d.a.uuid.clone(), s.clone());
                        }
                    }
                    t_book_us += t_b.elapsed().as_micros();
                    if alerting && !seen.contains(&d.a.uuid) {
                        let t_e = Instant::now();
                        eval_new(
                            d,
                            idx,
                            model,
                            prev_by_key,
                            seen,
                            relist,
                            last_updated,
                            nm,
                            &mut lbin_c,
                            &mut emit_count,
                        );
                        t_eval_us += t_e.elapsed().as_micros();
                    }
                }
            }
            Ok(detect::DumpMsg::End(e)) => break e,
            Ok(detect::DumpMsg::Start(_)) => {
                // Impossible: the single-streamer claim is only released after End.
                eprintln!("warn: nested dump Start — ignoring");
            }
            Err(e) => {
                eprintln!("warn: dump stream aborted ({e}) — abandoning sweep, state NOT rolled");
                return;
            }
        }
    };
    let t_om = Instant::now();
    for (uuid, bid) in end.old_bins {
        if live_bins.insert(uuid.clone()) {
            old_bins.push((uuid, bid));
        }
    }
    t_oldmerge_us += t_om.elapsed().as_micros();
    let p0_ms = t_pipe.elapsed().as_secs_f64() * 1000.0;

    // ---- DEEP PAGES 1..N (prod releases these at page-0 stream end too) ----
    // Two paths, selected by DEEP_STREAM (default on; `=0` is the exact old
    // blocking path, kept byte-identical as an instant kill-switch). STREAMING
    // evaluates each deep page's new BINs the moment that page lands instead of
    // blocking the loop thread to collect every page first — so a flip on page 1
    // is found at page 1's arrival, not after the slowest of ~46 pages. The page-0
    // critical path above is untouched either way; screening + NBT decode move to
    // the fetch workers (no shared pricing state), keying/eval/dedup stay here.
    let total_pages = start.total_pages.max(1);
    let deep_stream = std::env::var("DEEP_STREAM")
        .map(|v| v != "0")
        .unwrap_or(true);
    let mut n_new_deep = 0usize;
    if deep_stream {
        // The workers' prev-live screen = this sweep's `prev_live_uuids` (the
        // previous full sweep's live set), shared as an Arc. Cloned HERE, after
        // first-flip already fired on page 0, so it can never delay a flip.
        let live_arc = std::sync::Arc::new(prev_live_uuids.clone());
        let (deep_rx, pages_got) =
            hypixel::fetch_pages_streaming(1, total_pages, page_concurrency, live_arc, *primed);
        for msg in deep_rx {
            match msg {
                // Carried-over: dedup against this sweep's live set (page-0 old_bins
                // were already inserted above), then roll into old_bins for byKey.
                hypixel::DeepMsg::Old(batch) => {
                    for (uuid, bid) in batch {
                        if live_bins.insert(uuid.clone()) {
                            old_bins.push((uuid, bid));
                        }
                    }
                }
                // New + decoded on the worker: dedup, then the SAME per-BIN path as
                // page 0 / the old deep loop (key, caches, TTS, eval).
                hypixel::DeepMsg::New(batch) => {
                    for nb in batch {
                        if !live_bins.insert(nb.uuid.clone()) {
                            continue; // already seen on page 0 or an earlier deep page
                        }
                        n_new_deep += 1;
                        let key = idx.final_key(&nb.attrs);
                        decode_cache.insert(nb.uuid.clone(), nb.attrs.clone());
                        key_cache.insert(nb.uuid.clone(), key.clone());
                        if tts_capture && nb.start > 0.0 {
                            new_listings.push(store::ListingRow {
                                auction_id: nb.uuid.clone(),
                                start: nb.start as i64,
                                item_id: nb.attrs.id.clone(),
                            });
                        }
                        let nb_start = nb.start;
                        let d = DecodedAuction {
                            a: ActiveAuction {
                                uuid: nb.uuid,
                                starting_bid: nb.starting_bid,
                                auctioneer: nb.auctioneer,
                                item_name: nb.item_name,
                            },
                            attrs: nb.attrs,
                            key,
                        };
                        shared.record_auction_start(&d.a.uuid, nb_start, nm, false);
                        all_decoded.push(d.clone());
                        if follow_enabled {
                            if let Some(s) = &d.a.auctioneer {
                                uuid_seller.borrow_mut().insert(d.a.uuid.clone(), s.clone());
                            }
                        }
                        if alerting && !seen.contains(&d.a.uuid) {
                            eval_new(
                                d,
                                idx,
                                model,
                                prev_by_key,
                                seen,
                                relist,
                                last_updated,
                                nm,
                                &mut lbin_c,
                                &mut emit_count,
                            );
                        }
                    }
                }
            }
        }
        let got = pages_got.load(std::sync::atomic::Ordering::Relaxed);
        let want = (total_pages - 1).max(0) as usize;
        if got < want {
            eprintln!("deep-page fetch (stream): {got}/{want} pages — stragglers dropped, self-heal next sweep");
        }
    } else {
        let deep = hypixel::fetch_pages_from(1, total_pages, page_concurrency);
        for a in deep {
            if !a.bin || !live_bins.insert(a.uuid.clone()) {
                continue;
            }
            if *primed && prev_live_uuids.contains(&a.uuid) {
                old_bins.push((a.uuid, a.starting_bid));
                continue;
            }
            // See detect.rs: the priming sweep decodes everything (alerting is
            // false, so nothing is pushed) to seed the decode cache for later sweeps.
            if a.starting_bid <= 0.0 || a.item_bytes.is_empty() {
                continue;
            }
            let Some(attrs) = decode_item_bytes(&a.item_bytes) else {
                continue;
            };
            n_new_deep += 1;
            let key = idx.final_key(&attrs);
            decode_cache.insert(a.uuid.clone(), attrs.clone());
            key_cache.insert(a.uuid.clone(), key.clone());
            if tts_capture && a.start > 0.0 {
                new_listings.push(store::ListingRow {
                    auction_id: a.uuid.clone(),
                    start: a.start as i64,
                    item_id: attrs.id.clone(),
                });
            }
            let a_start = a.start;
            let d = DecodedAuction {
                a: ActiveAuction {
                    uuid: a.uuid,
                    starting_bid: a.starting_bid,
                    auctioneer: a.auctioneer,
                    item_name: a.item_name,
                },
                attrs,
                key,
            };
            shared.record_auction_start(&d.a.uuid, a_start, nm, false);
            all_decoded.push(d.clone());
            if follow_enabled {
                if let Some(s) = &d.a.auctioneer {
                    uuid_seller.borrow_mut().insert(d.a.uuid.clone(), s.clone());
                }
            }
            if alerting && !seen.contains(&d.a.uuid) {
                eval_new(
                    d,
                    idx,
                    model,
                    prev_by_key,
                    seen,
                    relist,
                    last_updated,
                    nm,
                    &mut lbin_c,
                    &mut emit_count,
                );
            }
        }
    }

    // ---- Full-dump structures: live-BIN price lists per key (index.ts:825-843).
    //      Carried-over BINs were never re-decoded — their attrs come from the
    //      cache the sweep that first saw them filled. ----
    let mut by_key: HashMap<String, Vec<Bin>> = HashMap::new();
    let mut missing_old = 0usize;
    {
        let mut push = |uuid: &str, key: &str, bid: f64, attrs: &ItemAttributes| {
            let price = (bid - idx.minor_feature_value(attrs)).max(bid * 0.5);
            by_key.entry(key.to_string()).or_default().push(Bin {
                uuid: uuid.to_string(),
                price,
            });
        };
        for d in &all_decoded {
            push(&d.a.uuid, &d.key, d.a.starting_bid, &d.attrs);
        }
        for (uuid, bid) in &old_bins {
            match decode_cache.get(uuid) {
                None => missing_old += 1,
                Some(attrs) => {
                    let key = key_cache
                        .get(uuid)
                        .cloned()
                        .unwrap_or_else(|| idx.final_key(attrs));
                    push(uuid, &key, *bid, attrs);
                }
            }
        }
    }
    if missing_old > 50 {
        eprintln!("warn: {missing_old} carried-over auctions missing from decode cache");
    }
    for l in by_key.values_mut() {
        l.sort_by(|x, y| x.price.partial_cmp(&y.price).unwrap());
    }

    if alerting {
        // Dominance first: a real sold price beats the lbin finder's live floor.
        for f in eval_dominance_flips(&lbin_c, &by_key, seen, relist, last_updated, idx, nm) {
            emit_count(&f);
        }
        for f in eval_lbin_flips(&lbin_c, &by_key, seen, relist, last_updated, idx, nm) {
            emit_count(&f);
        }
    } else if *primed {
        eprintln!(
            "warn: carried-over state too small ({}) — rebuilt silently, no alerts",
            prev_live_uuids.len()
        );
    }

    // ---- Roll state to this dump (index.ts:863-879) ----
    seen.retain(|id| live_bins.contains(id));
    decode_cache.retain(|id, _| live_bins.contains(id));
    key_cache.retain(|id, _| live_bins.contains(id));
    *prev_by_key = by_key;
    *prev_live_uuids = live_bins;
    *primed = true;
    // Ship the rolled uuid set to the detect lanes so the next dump's new-BIN
    // screen is correct from its first byte (index.ts:871-874).
    dsh.publish_live(prev_live_uuids.clone());

    if !new_listings.is_empty() {
        if let Err(e) = store.lock().unwrap().record_listings(&new_listings) {
            eprintln!("recordListings failed: {e}");
        }
    }

    // Link the relists this sweep spotted back to what we paid, and mark them as
    // ours so the self-buy guard can never buy our own listing back.
    //
    // Both the query and the match run HERE rather than at the top of the sweep --
    // see the note at the `unlisted_purchases` comment above. `all_decoded` holds
    // every new BIN from page 0 and the deep pages, which is exactly the set the
    // old per-BIN closure ran on.
    {
        let unlisted_purchases = match store.lock().unwrap().unlisted_purchases_by_item_uuid() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("self-listing link: query failed: {e}");
                HashMap::new()
            }
        };
        let mut relists: Vec<(String, String, String, String)> = Vec::new();
        if !unlisted_purchases.is_empty() {
            for d in all_decoded.iter() {
                let Some(iu) = d.attrs.item_uuid.as_deref() else {
                    continue;
                };
                let Some(flip_uuid) = unlisted_purchases.get(iu) else {
                    continue;
                };
                // ⚠️ `unlisted_purchases` selects on `bought_at IS NOT NULL`, and
                // `bought_at` means the flagged auction ENDED -- bought by ANYONE,
                // not necessarily us (see `unlisted_purchases_by_item_uuid`). So a
                // stranger who buys a flip we flagged and relists it matches here
                // and gets recorded as OUR listing.
                //
                // Measured 2026-08-07: `listing_uuids` held 20,206 rows over 14
                // days against 4,083 actual purchases, i.e. **87% of it was other
                // people's listings**. That silently invalidates any "how do our
                // listings perform" analysis built on the table.
                //
                // A cost basis is the proof of purchase, so require one. It cannot
                // false-negative on a real relist: `price_inventory` already needs
                // the cost basis to compute `cost_floor`, so an item we own without
                // one could not have been priced for listing in the first place.
                if !shared.have_cost_basis(iu) {
                    continue;
                }
                relists.push((
                    d.a.uuid.clone(),
                    iu.to_string(),
                    flip_uuid.clone(),
                    d.a.item_name.clone(),
                ));
            }
        }
        if !relists.is_empty() {
            let st = store.lock().unwrap();
            for (listing_uuid, item_uuid, flip_uuid, item_name) in relists.iter() {
                if let Err(e) = st.record_listing_uuid(listing_uuid, flip_uuid, Some(item_name)) {
                    eprintln!("self-listing link: recordListingUuid failed: {e}");
                    continue;
                }
                shared.record_own_listing(Some(listing_uuid), Some(item_uuid));
                eprintln!("self-listing link: {item_name} listed as {listing_uuid} -> flip {flip_uuid} (from dump, no API call)");
            }
        }
    }

    // firstFlipMs is the number to compare against prod (median 44-55ms). Its
    // parts are attributable: detectLagMs = the shared CF floor, firstNewDecMs =
    // when the data was actually in hand, drainMs = how long the body took.
    // `take` also resets the accumulators, so each line covers one sweep.
    let (eval_prof_take_snipe, eval_prof_take_median, eval_prof_take_emit) = eval_prof_take();
    eprintln!(
        "SWEEP: lane {} lu {} | new p0 {} (+{} deep) old {} | {} pushed ({} delivered) \
         | FIRSTFLIPMS {} | p0 {:.0}ms full {:.0}ms | detectLagMs {} firstNewDecMs {} drainMs {} decCpuMs {:.1} \
         | P0SPLIT wait {:.0} key {:.0} eval {:.0} (snipe {:.0} median {:.0} emit {:.0}) book {:.0} oldmerge {:.0} ms | tts {}",
        start.lane,
        start.last_updated,
        n_new_p0,
        n_new_deep,
        old_bins.len(),
        pushed,
        delivered,
        first_flip_ms,
        p0_ms,
        t_pipe.elapsed().as_secs_f64() * 1000.0,
        if start.last_updated > 0 { start.detect_at - start.last_updated } else { -1 },
        end.first_new_dec_ms,
        end.drain_ms,
        end.dec_cpu_ms,
        t_wait_us as f64 / 1000.0,
        t_key_us as f64 / 1000.0,
        t_eval_us as f64 / 1000.0,
        eval_prof_take_snipe,
        eval_prof_take_median,
        eval_prof_take_emit,
        t_book_us as f64 / 1000.0,
        t_oldmerge_us as f64 / 1000.0,
        new_listings.len(),
    );
    // Median rejection funnel: where candidates died vs emitted. `volume` is the
    // volume-floor kills (the volume→TTS lever); tune from it. Emitted here counts
    // median/model flips only (snipes emit before eval_median_flip). Skip logging
    // on a silent priming sweep (no eval ran).
    let rej = finder_core::sniper::reject_take();
    let seen_any = rej.emitted
        + rej.below_margin
        + rej.below_volume
        + rej.below_confidence
        + rej.below_profit
        + rej.falling
        + rej.undercuts
        + rej.relist_blocked
        + rej.not_priceable;
    if seen_any > 0 {
        eprintln!(
            "FUNNEL: emitted {} | margin {} volume {} confidence {} profit {} falling {} undercuts {} relist {} notpriceable {}",
            rej.emitted, rej.below_margin, rej.below_volume, rej.below_confidence, rej.below_profit,
            rej.falling, rej.undercuts, rej.relist_blocked, rej.not_priceable,
        );
    }
    // Filter-layer funnel: of the flips the finder emitted, which the push filter
    // dropped and why (the Infinileap `roi<100%` class shows up under `roi`).
    let ff = filter_funnel.borrow();
    let ff_total = ff.global
        + ff.roi
        + ff.profit
        + ff.volume
        + ff.confidence
        + ff.tts
        + ff.guard
        + ff.blacklist
        + ff.flood
        + ff.routing
        + ff.other;
    if ff_total > 0 {
        eprintln!(
            "FUNNEL-FILTER: rejected {ff_total} | global {} roi {} profit {} volume {} confidence {} tts {} guard {} blacklist {} flood {} routing {} other {}",
            ff.global, ff.roi, ff.profit, ff.volume, ff.confidence, ff.tts,
            ff.guard, ff.blacklist, ff.flood, ff.routing, ff.other,
        );
    }
}

/// Full single-box pipeline over the decoded dump → flips. Used by the one-shot
/// SWEEP/COMPARE modes, which have no previous dump: they evaluate against the
/// current dump's own byKey. The SERVE path does NOT use this — see handle_dump,
/// which carries prod's real prevByKey/seen/relist state across sweeps.
fn run_pipeline(
    decoded: &[DecodedAuction],
    idx: &PriceIndex,
    model: &ModifierModel,
    now_ms: i64,
    last_updated: f64,
) -> Vec<(Flip, f64)> {
    // Timer starts at pipeline entry (byKey build counts as find work); each flip
    // is stamped with the ms elapsed when it was found, so we can report per-flip
    // find latency (Rust vs TS) rather than just the dump age.
    let start = Instant::now();
    let el = || start.elapsed().as_secs_f64() * 1000.0;
    let mut bykey: HashMap<String, Vec<Bin>> = HashMap::new();
    for d in decoded {
        let price =
            (d.a.starting_bid - idx.minor_feature_value(&d.attrs)).max(d.a.starting_bid * 0.5);
        bykey.entry(d.key.clone()).or_default().push(Bin {
            uuid: d.a.uuid.clone(),
            price,
        });
    }
    for l in bykey.values_mut() {
        l.sort_by(|x, y| x.price.partial_cmp(&y.price).unwrap());
    }
    let mut seen = HashSet::new();
    let mut relist = RelistTracker::default();
    let mut flips: Vec<(Flip, f64)> = Vec::new();
    let mut lbin_c: Vec<DecodedAuction> = Vec::new();
    let nm = now_ms as f64;
    for d in decoded {
        // No screen_auction() pre-pass: its verdict was discarded and the full
        // eval below redoes the identical lookups (decision-identical, less CPU).
        if let Some(f) = eval_clean_snipe(d, idx, &bykey, &mut seen, &mut relist, last_updated, nm)
        {
            flips.push((f, el()));
            continue;
        }
        let (flip, priceable) = eval_median_flip(
            d,
            idx,
            model,
            &bykey,
            &mut seen,
            &mut relist,
            last_updated,
            nm,
        );
        if let Some(f) = flip {
            flips.push((f, el()));
            continue;
        }
        if !priceable {
            lbin_c.push(d.clone());
        }
    }
    for f in eval_dominance_flips(
        &lbin_c,
        &bykey,
        &mut seen,
        &mut relist,
        last_updated,
        idx,
        nm,
    ) {
        flips.push((f, el()));
    }
    for f in eval_lbin_flips(
        &lbin_c,
        &bykey,
        &mut seen,
        &mut relist,
        last_updated,
        idx,
        nm,
    ) {
        flips.push((f, el()));
    }
    flips
}

/// Port of `capRam` (index.ts). Hard memory guard: age-pruning bounds the ref
/// pool by TIME, but a high-volume burst can push it past a memory-safe COUNT
/// within that window, and an unbounded spike = OOM kill of the whole flipper.
/// Sheds the OLDEST refs down to REF_MAX_RAM; recency is what pricing weights
/// anyway. Prod runs REF_MAX_RAM=1800000 on the RAM-bound box.
///
/// Deviation from TS (structural, same invariant): TS appends refs incrementally
/// and calls capRam after each append, scheduling a rebuild when it fires. This
/// port has no incremental append yet — it reloads the pool in full on every
/// rebuild — so the cap is applied at load time instead. The pool feeding the
/// index still never exceeds the cap.
fn cap_ram(refs: &mut Vec<finder_core::price_index::Reference>) {
    let cap = *finder_core::config::REF_MAX_RAM;
    if cap == 0 || refs.len() <= cap {
        return;
    }
    let before = refs.len();
    // Newest first, then drop the oldest tail.
    refs.sort_unstable_by(|a, b| b.sold_at.total_cmp(&a.sold_at));
    refs.truncate(cap);
    eprintln!(
        "RAM ref cap hit — shed {} oldest references (refsInRam {}, cap {}) [spike guard]",
        before - refs.len(),
        refs.len(),
        cap
    );
}

/// collectEnded: persist recently-sold BINs + reconcile posted flips.
/// Returns (fetched, stored, resolved, bought). Grows the store (the hourly-ish
/// rebuild reloads from it — no incremental append_refs in this port yet).
fn collect_ended(
    store: &std::sync::Arc<std::sync::Mutex<store::Store>>,
) -> (usize, usize, usize, usize) {
    use store::{EndedRow, SoldRow};
    let ended = hypixel::fetch_ended();
    let mut rows: Vec<SoldRow> = Vec::new();
    let mut item_uuid: HashMap<String, Option<String>> = HashMap::new();
    for e in &ended {
        if !e.bin {
            continue;
        }
        let Some(attrs) = decode_item_bytes(&e.item_bytes) else {
            continue;
        };
        item_uuid.insert(e.auction_id.clone(), attrs.item_uuid.clone());
        rows.push(SoldRow {
            auction_id: e.auction_id.clone(),
            price: e.price,
            bin: e.bin,
            sold_at: (e.timestamp / 1000.0) as i64,
            seller: e.seller(),
            buyer: e.buyer(),
            attrs,
        });
    }
    let mut st = store.lock().unwrap();
    let stored = st.insert_sold(&rows).map(|s| s.len()).unwrap_or(0);
    let ended_rows: Vec<EndedRow> = ended
        .iter()
        .map(|e| EndedRow {
            uuid: e.auction_id.clone(),
            price: e.price,
            sold_at: (e.timestamp / 1000.0) as i64,
            item_uuid: item_uuid.get(&e.auction_id).cloned().flatten(),
        })
        .collect();
    let (resolved, bought) = st.reconcile_posted(&ended_rows).unwrap_or_default();
    (ended.len(), stored, resolved.len(), bought)
}

/// `attrPricer` (index.ts 1191): price a decoded item's attrs for the inventory
/// RPC. stats ?? model ?? live-lbin-fallback (with a manipulation guard). Runs on
/// the loop thread (owns idx/model/prevByKey); minor-adjusted median pools credit
/// the item's own minor features back (the model prices modifiers itself → none).
fn est_attr_pricer(
    attrs: &ItemAttributes,
    idx: &PriceIndex,
    model: &ModifierModel,
    prev_by_key: &HashMap<String, Vec<Bin>>,
) -> Option<ListingEstimate> {
    let key = idx.final_key(attrs);
    let list = prev_by_key.get(&key);
    // What the item actually trades at, on the item+star group rather than the
    // narrow final key: a key like FIGSTONE_AXE*5#ench:absorb6+gems:2+reforge:
    // moonglade holds 12 sales while the item itself does 751 a week. Liquidity
    // and market level are properties of the ITEM; only the price is keyed narrow.
    let market_median = {
        let bk = base_key(attrs);
        let m = idx.base_value_for(&bk);
        if m > 0.0 && idx.sold_count_for_base(&bk) >= *LIST_MARKET_MIN_REFS {
            Some(m)
        } else {
            None
        }
    };
    // The base pool only describes this item when no significant feature forked
    // the key. `recombobulated` alone is a 20x-114x premium on talismans.
    //
    // Reuses `key` rather than re-deriving it: `final_key` walks every candidate
    // feature and does a `feature_value` lookup per feature, so calling it twice
    // for one comparison doubled the cost of this function for nothing.
    let variant_priced = key != base_key(attrs);
    let lbin = list.and_then(|l| l.iter().find(|x| !x.uuid.is_empty()).map(|x| x.price));
    if let Some(s) = idx.price_for(attrs) {
        let credit = (idx.minor_feature_value(attrs) * 0.5).min(s.target * 0.3);
        return Some(ListingEstimate {
            target: (s.target + credit).round(),
            lbin,
            volume_per_day: Some((s.volume_per_day * 100.0).round() / 100.0),
            confidence: (s.confidence * 1000.0).round() / 1000.0,
            samples: s.samples,
            key,
            basis: Some("refs".to_string()),
            market_median,
            variant_priced,
        });
    }
    // Fragmented exact key: the same ladder the flip detect walks (`KEY_LADDER`).
    // A held variant priced off its coarser pool is under-priced at worst — the
    // coarser pool is the cheap majority — and the cost floor still refuses to
    // list under `paid * 1.05`, so the downside is "lists high, sits", which is
    // exactly what `noprice` already guarantees. Confidence arrives pre-degraded
    // by dropped-feature count, so it lands in the LIST_LOW_CONF lane.
    if let Some((s, _depth)) = idx.ladder_price(attrs, &key) {
        let credit = (idx.minor_feature_value(attrs) * 0.5).min(s.target * 0.3);
        return Some(ListingEstimate {
            target: (s.target + credit).round(),
            lbin,
            volume_per_day: Some((s.volume_per_day * 100.0).round() / 100.0),
            confidence: (s.confidence * 1000.0).round() / 1000.0,
            samples: s.samples,
            key,
            basis: Some("ladder".to_string()),
            market_median,
            variant_priced,
        });
    }
    if let Some(m) = model.estimate_for(attrs, idx) {
        return Some(ListingEstimate {
            target: m.target.round(),
            lbin,
            volume_per_day: Some((m.volume_per_day * 100.0).round() / 100.0),
            confidence: (m.confidence * 1000.0).round() / 1000.0,
            samples: m.samples,
            key,
            basis: Some("model".to_string()),
            market_median,
            variant_priced,
        });
    }
    // No sold-history/model price → live lowest-BIN fallback, unless it's absurdly
    // off any clean base value we did collect (manipulation guard).
    let lb = lbin?;
    if lb <= 0.0 {
        return None;
    }
    let base = idx.base_value_for(&base_key(attrs));
    if base > 0.0 && (lb > base * 5.0 || lb < base * 0.2) {
        return None;
    }
    // A lone competing listing is an ASK, not a market ([[finder-lbin-is-one-
    // listing-not-a-market]]), and here it becomes our own listing target. The
    // `base * 5.0` manipulation guard above is far too loose to catch that: on a
    // Crown of Avarice (base 421,000,000) it permits 2.105B, so the live wall of
    // 1.88-1.93B crown asks set our target to 1,904,423,496 and we opened at
    // `* LIST_OPEN_FACTOR` = **1,999,644,670** — on a crown whose own 36-sale
    // pool medians 700,000,000. It never sold, and each 6h cycle billed 2.5% of
    // the inflated ask.
    //
    // Clamp rather than reject: `None` here means the item is never listed and
    // holds a slot indefinitely, which is strictly worse than listing it at a
    // defensible price. Strictly lowers the target, so it cannot create a flip.
    let target = match *LBIN_FALLBACK_CAP {
        cap if cap > 0.0 && base > 0.0 => lb.min((base * cap).floor()),
        _ => lb,
    };
    Some(ListingEstimate {
        target,
        lbin: Some(lb),
        volume_per_day: None,
        confidence: 0.35,
        samples: list.map(|l| l.len() as i64).unwrap_or(0),
        key,
        basis: Some("lbin".to_string()),
        market_median,
        variant_priced,
    })
}

/// `estimator` (index.ts 1223): the `estimate` RPC — price a live auction by uuid
/// from the last sweep's decodeCache. stats, else the live lowest-BIN (excluding
/// the auction itself).
fn est_estimator(
    uuid: &str,
    idx: &PriceIndex,
    prev_by_key: &HashMap<String, Vec<Bin>>,
    decode_cache: &HashMap<String, ItemAttributes>,
) -> Option<ListingEstimate> {
    let attrs = decode_cache.get(uuid)?;
    let key = idx.final_key(attrs);
    let stats = idx.price_for(attrs);
    // What the item actually trades at, on the item+star group rather than the
    // narrow final key: a key like FIGSTONE_AXE*5#ench:absorb6+gems:2+reforge:
    // moonglade holds 12 sales while the item itself does 751 a week. Liquidity
    // and market level are properties of the ITEM; only the price is keyed narrow.
    let market_median = {
        let bk = base_key(attrs);
        let m = idx.base_value_for(&bk);
        if m > 0.0 && idx.sold_count_for_base(&bk) >= *LIST_MARKET_MIN_REFS {
            Some(m)
        } else {
            None
        }
    };
    // The base pool only describes this item when no significant feature forked
    // the key. `recombobulated` alone is a 20x-114x premium on talismans.
    //
    // Reuses `key` rather than re-deriving it: `final_key` walks every candidate
    // feature and does a `feature_value` lookup per feature, so calling it twice
    // for one comparison doubled the cost of this function for nothing.
    let variant_priced = key != base_key(attrs);
    let list = prev_by_key.get(&key);
    let lbin = list.and_then(|l| l.iter().find(|x| x.uuid != uuid).map(|x| x.price));
    if stats.is_none() && lbin.is_none() {
        return None;
    }
    if let Some(s) = stats {
        let credit = (idx.minor_feature_value(attrs) * 0.5).min(s.target * 0.3);
        return Some(ListingEstimate {
            target: (s.target + credit).round(),
            lbin,
            volume_per_day: Some((s.volume_per_day * 100.0).round() / 100.0),
            confidence: (s.confidence * 1000.0).round() / 1000.0,
            samples: s.samples,
            key,
            basis: Some("refs".to_string()),
            market_median,
            variant_priced,
        });
    }
    let lb = lbin.unwrap();
    Some(ListingEstimate {
        target: lb.round(),
        lbin,
        volume_per_day: None,
        confidence: 0.3,
        samples: list.map(|l| l.len() as i64).unwrap_or(0),
        key,
        basis: Some("lbin".to_string()),
        market_median,
        variant_priced,
    })
}

/// Lets the bazaar finder reach the bots without knowing what a websocket is.
/// The bazaar orders ride the SAME socket the auction flips do, in COFL's own
/// `bazaarFlip` envelope, which the mod has parsed since before this existed —
/// so nothing on the mod side has to change to receive them.
struct BazaarFleet(std::sync::Arc<ws_server::WsShared>);

impl bazaar_finder::Fleet for BazaarFleet {
    fn targets(&self) -> Vec<(String, f64, i64)> {
        self.0.bazaar_targets()
    }

    fn holders(&self) -> Vec<(String, f64, i64)> {
        self.0.bazaar_holders()
    }

    fn send(&self, bot: &str, payload: String) -> bool {
        self.0.send_to_bot(bot, payload)
    }
}

/// SERVE mode: start the ws flip feed + run the continuous orchestration loop
/// (sweep → pipeline → push_flip → persist), rebuilding the index on a cadence.
/// Read-only-safe for a shadow: webhooks off, no bot actions. The async ws server
/// runs on a tokio runtime; this loop drives the sync/blocking sweep on the
/// caller thread (the index stays single-threaded, never shared across threads).
fn serve_loop(
    db: String,
    store: std::sync::Arc<std::sync::Mutex<store::Store>>,
    mut idx: PriceIndex,
    mut model: ModifierModel,
    // NOTE: this used to also take the startup `refs` Vec. It was never read
    // (`_refs`) yet was moved in and held for the whole process lifetime, a
    // second full copy of every reference -- ~4GB at a 14-day window. Rebuilds
    // load their own refs, so it is simply gone now.
    bazaar_map: HashMap<String, f64>,
) {
    use std::time::Duration;
    let ws_cfg_path =
        std::env::var("WS_CONFIG_PATH").unwrap_or_else(|_| "./data/ws-config.json".to_string());
    let cost_path =
        std::env::var("COST_BASIS_PATH").unwrap_or_else(|_| "./data/cost-basis.json".to_string());
    let max_held: i64 = std::env::var("MAX_HELD_PER_BASE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let rebuild_ms: u64 = std::env::var("REBUILD_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600_000);
    let ended_ms: u64 = std::env::var("ENDED_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(55_000);
    let page_concurrency: usize = std::env::var("PAGE_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);

    let mut cfg_store = ws_config::WsConfigStore::load(&ws_cfg_path);
    let cost_basis = cost_basis::CostBasis::load(&cost_path);
    // Pricing RPCs (estimate/inventory) from the async ws threads are answered
    // here on the loop thread, which owns the index/model/last-sweep state.
    let (rpc_tx, mut rpc_rx) = tokio::sync::mpsc::unbounded_channel::<RpcRequest>();
    // Cloned before the move into WsShared: the public feed answers the baf mod's
    // inventory pricing RPC on this same loop-thread channel, behind a per-key
    // capability and a single global permit.
    let rpc_tx_public = rpc_tx.clone();
    let shared = ws_server::WsShared::new(
        cfg_store.filters.clone(),
        cost_basis,
        store.clone(),
        max_held,
        rpc_tx,
    );

    // BinMaster multi-tier filter: when present it drives the push decision (and
    // the competitive listing price via scale_price) instead of the ws-config
    // single-threshold gate. Alongside ws-config by default; hot-reloaded below.
    let bin_path = std::env::var("BINMASTER_FILTER_PATH").unwrap_or_else(|_| {
        std::path::Path::new(&ws_cfg_path)
            .parent()
            .map(|p| {
                p.join("binmaster-filter.json")
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_else(|| "./data/binmaster-filter.json".to_string())
    });
    let mut bin_store = ws_config::BinFilterStore::new(&bin_path);
    // Flip API (:15100): the BinMaster editor UI + /recent-flips for baf-backend.
    let recent_path = std::env::var("RECENT_FLIPS_PATH")
        .unwrap_or_else(|_| "./data/recent-flips.json".to_string());
    let recent = std::sync::Arc::new(std::sync::Mutex::new(recent_flips::RecentFlips::load(
        &recent_path,
    )));
    eprintln!(
        "recent-flips loaded: {} flips",
        recent.lock().unwrap().len()
    );
    if let Some(f) = bin_store.poll() {
        eprintln!(
            "BinMaster filter {} ({bin_path})",
            if f.is_some() {
                "loaded — drives push decision"
            } else {
                "absent — using ws-config thresholds"
            }
        );
        shared.set_bin_filter(f);
    } else {
        eprintln!("BinMaster filter absent ({bin_path}) — using ws-config thresholds");
    }

    // Async ws feed on a tokio runtime (background). The orchestration loop below
    // runs on this thread.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio rt");
    {
        let s = shared.clone();
        rt.spawn(async move {
            if let Err(e) = ws_server::start_ws_server(s).await {
                eprintln!("ws server error: {e}");
            }
        });
    }
    // Public leftover feed: a SECOND listener carrying only the flips push_flip
    // declined. Dormant unless PUBLIC_WS=1, and even then it accepts nobody
    // without a key file. Attached before the first sweep so no flip is missed.
    if let Some(hub) = public_ws::spawn(&rt, rpc_tx_public) {
        // The filter UI shares the hub: same keys, same constant-time auth, and a
        // save is pushed straight to that key's open connections.
        let h = hub.clone();
        rt.spawn(async move {
            if let Err(e) = public_api::serve(h).await {
                eprintln!("public filter UI error: {e}");
            }
        });
        shared.set_public_hub(hub);
    }
    let discord = discord::Discord::new(std::env::var("DISCORD_WEBHOOK_URL").unwrap_or_default());
    discord.spawn(&rt);
    eprintln!(
        "discord webhook: {}",
        if discord.enabled() {
            "enabled"
        } else {
            "DISABLED (DISCORD_WEBHOOK_URL unset)"
        }
    );
    {
        let admin_password = std::env::var("ADMIN_PASSWORD").unwrap_or_default();
        eprintln!(
            "flip API filter editor: {}",
            if admin_password.is_empty() {
                "⚠️  UNAUTHENTICATED (ADMIN_PASSWORD unset — /filter is open; set it to the baf-backend admin password)"
            } else {
                "password-protected (HTTP Basic, ADMIN_PASSWORD)"
            }
        );
        let st = std::sync::Arc::new(flip_api::FlipApiState {
            recent: recent.clone(),
            filter_path: bin_path.clone(),
            edit_token: std::env::var("FILTER_EDIT_TOKEN").unwrap_or_default(),
            store: store.clone(),
            cofl_compare_webhook: std::env::var("COFL_COMPARE_WEBHOOK_URL")
                .or_else(|_| std::env::var("DISCORD_WEBHOOK_URL"))
                .unwrap_or_default(),
            admin_password,
        });
        rt.spawn(async move {
            if let Err(e) = flip_api::serve(st).await {
                eprintln!("flip API error: {e}");
            }
        });
    }
    // Bazaar snapshot collector (own thread, own sqlite file, read-only on the
    // public bazaar endpoint) gathers price/volume history for a future bazaar
    // finder. Fully isolated from the flip path; disable with BAZAAR_COLLECT_OFF=1.
    bazaar_collect::spawn(&db);
    // Bazaar finder: ranks bazaar products by coins-per-slot-hour and pushes
    // buy/sell orders to the bots over this same socket. Dormant unless
    // BZ_FINDER=1, and BZ_FINDER_DRY=1 logs decisions without emitting. Own
    // thread and own db handle, like the collector.
    bazaar_finder::spawn(
        &bazaar_collect::db_path(&db),
        std::sync::Arc::new(BazaarFleet(shared.clone())),
    );
    // Seller-follow (liquidation catcher) worker: when a flip triggers, it pulls
    // that seller's whole AH via the player-auction API off the hot path and hands
    // the still-buyable BINs back here to price through the normal push path.
    // Dormant unless `sellerFollow` is set in ws-config AND `API_KEY` is in the env.
    let follow = seller_follow::spawn(shared.clone());
    // ---- Detect lanes: the low-latency half (detect.rs, merged from the Phase-1
    //      shadow). Lanes race page 0, one wins the single-streamer claim, decodes
    //      NEW BINs as the body streams, and hands the dump to this thread. ----
    let (dump_tx, dump_rx) = std::sync::mpsc::channel::<detect::DumpMsg>();
    let dsh = detect::Shared::new(dump_tx);
    // Detection on its OWN runtime, so its ~600 per-chunk wakeups per dump are
    // never queued behind the ws feed, the flip API, discord or seller-follow.
    //
    // Measured: draining this exact body standalone takes 19.3ms (p50, 596
    // chunks) and curl does it in ~14ms, but prod `drainMs` is 125ms. The ~105ms
    // gap is not the network, not gzip, not reqwest and not the streaming
    // pattern -- `drain_bench` clears all four -- it is scheduler contention
    // inside our own process. That matters because 76.7% of all flip profit is
    // in auctions gone within 1 second of us finding them.
    //
    // DETECT_RUNTIME_THREADS=0 or unset keeps the old shared-runtime behaviour.
    let detect_rt = match std::env::var("DETECT_RUNTIME_THREADS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        Some(n) => {
            let r = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(n)
                .thread_name("detect")
                .enable_all()
                .build()
                .expect("detect rt");
            eprintln!("DETECT: dedicated runtime with {n} worker thread(s)");
            Some(r)
        }
        None => {
            eprintln!("DETECT: sharing the main runtime (set DETECT_RUNTIME_THREADS=N to isolate)");
            None
        }
    };
    detect::spawn_lanes(detect_rt.as_ref().unwrap_or(&rt), dsh.clone());
    eprintln!(
        "SERVE: ws feed up; dump-driven orchestration (rebuild {rebuild_ms}ms, ended {ended_ms}ms)"
    );

    // Boot maintenance, in prod's order (index.ts:1171-1172).
    {
        let mut st = store.lock().unwrap();
        match st.prune_old() {
            Ok(n) if n > 0 => eprintln!("pruneOld: dropped {n} refs past retention"),
            Err(e) => eprintln!("pruneOld failed: {e}"),
            _ => {}
        }
        match st.prune_listings() {
            Ok(n) if n > 0 => eprintln!("pruneListings: censored {n} expired unsold listings"),
            Err(e) => eprintln!("pruneListings failed: {e}"),
            _ => {}
        }
    }

    let mut mem = SweepMemory::default();
    let mut last_ended = Instant::now()
        .checked_sub(Duration::from_secs(3600))
        .unwrap_or_else(Instant::now);
    let mut last_rebuild = Instant::now();
    let mut last_flush = Instant::now();
    let mut last_prune = Instant::now();
    // Background index rebuild. The full PriceIndex + ModifierModel build over
    // ~1.66M refs takes ~23s; run inline it blocked dump pickup for its whole
    // duration, so a dump detected during a rebuild waited up to ~23s to be
    // served — the FIRSTFLIPMS 9-25s tail, i.e. the finder went blind for ~23s
    // every 10 min. Build off-thread from a read-only DB snapshot (WAL → no lock
    // held) and hot-swap when ready; the loop keeps serving on the old index,
    // which is already up to REBUILD_MS old, so seconds of handoff staleness is
    // nothing. None = the background load failed; just retry next interval.
    let (rebuild_tx, rebuild_rx) =
        std::sync::mpsc::channel::<Option<(PriceIndex, ModifierModel)>>();
    let mut rebuild_in_flight = false;
    // Bed flips held back until just before they become buyable. See
    // BED_PUSH_LEAD_MS. Tiny by construction: only live beds, only from
    // seller-follow, and each entry lives at most one grace window.
    let mut bed_queue: Vec<(f64, Flip)> = Vec::new();
    loop {
        // ---- Wait for a real dump. Maintenance and pricing RPCs run in the gap
        //      between dumps (~60s), never on the latency path. ----
        let dump = loop {
            // Release any held bed whose lift is now imminent. This inner loop
            // ticks every 50ms (dump_rx.recv_timeout below), which is ample
            // granularity against a 1.5s lead.
            if !bed_queue.is_empty() {
                let now_b = now_ms_real() as f64;
                let mut i = 0;
                while i < bed_queue.len() {
                    if bed_queue[i].0 <= now_b {
                        let (_, f) = bed_queue.remove(i);
                        eprintln!(
                            "BED: releasing {} — lifts in {:.0}ms",
                            f.uuid,
                            shared
                                .purchase_at_ms(&f.uuid)
                                .map(|t| t - now_b)
                                .unwrap_or(0.0)
                        );
                        post_flip(&f, &mem.prev_by_key, &idx, &shared, &recent, &discord);
                    } else {
                        i += 1;
                    }
                }
            }
            while let Ok(req) = rpc_rx.try_recv() {
                match req {
                    RpcRequest::Estimate { uuid, reply } => {
                        let _ = reply.send(est_estimator(
                            &uuid,
                            &idx,
                            &mem.prev_by_key,
                            &mem.decode_cache,
                        ));
                    }
                    RpcRequest::PriceAttrs { attrs, reply } => {
                        let out = attrs
                            .iter()
                            .map(|a| est_attr_pricer(a, &idx, &model, &mem.prev_by_key))
                            .collect();
                        let _ = reply.send(out);
                    }
                    RpcRequest::PriceAndResolve { items, reply } => {
                        // Price each crawled auction, then recover its real auction
                        // uuid from the live BIN set. `by_key` stores minor-adjusted
                        // prices, so adjust the observed bin the same way and match
                        // the closest live listing of this key.
                        let out: Vec<AhResolved> = items
                            .iter()
                            .map(|(a, bin)| {
                                let est = est_attr_pricer(a, &idx, &model, &mem.prev_by_key);
                                let key = idx.final_key(a);
                                let adj = (bin - idx.minor_feature_value(a)).max(bin * 0.5);
                                let uuid = mem.prev_by_key.get(&key).and_then(|list| {
                                    list.iter()
                                        .filter(|b| (b.price - adj).abs() <= (adj * 0.001).max(1.0))
                                        .min_by(|x, y| x.price.partial_cmp(&y.price).unwrap())
                                        .map(|b| b.uuid.clone())
                                });
                                AhResolved { est, uuid }
                            })
                            .collect();
                        let _ = reply.send(out);
                    }
                }
            }
            // ---- Seller-follow: price a watched seller's freshly-pulled listings
            //      here, off the hot path, through the SAME eval + push as a sweep
            //      flip (this thread owns idx/model/mem). Own throwaway lbin scratch;
            //      shares mem.seen/relist so dedup + relist-spam stay consistent, so
            //      the sweep won't re-push what the follow already sent. ----
            while let Ok(batch) = follow.auc_rx.try_recv() {
                let mut scratch_lbin: Vec<DecodedAuction> = Vec::new();
                let mut evaluated = 0usize;
                let mut posted = 0usize;
                let now = now_ms_real() as f64;
                let SweepMemory {
                    seen,
                    relist,
                    prev_by_key,
                    ..
                } = &mut mem;
                for a in &batch.auctions {
                    if a.starting_bid <= 0.0 || a.item_bytes.is_empty() || seen.contains(&a.uuid) {
                        continue;
                    }
                    let Some(attrs) = decode_item_bytes(&a.item_bytes) else {
                        continue;
                    };
                    // NetherAPI's per-player lookup serves a BIN 3-7s after
                    // listing, ~17s before the paginated dump does. Record
                    // `start` so the pushed flip carries `purchaseAt` and the bot
                    // waits for the exact instant the bed lifts instead of
                    // blind-clicking through the grace period.
                    shared.record_auction_start(&a.uuid, a.start, now, true);
                    // DIAGNOSTIC: seller-follow is the only path that can see an
                    // auction mid-bed. If this never fires, follow flips are not
                    // beds and the bots' grace storms come from somewhere else.
                    // Cheap: the follow path handles ~2k listings/night, not the
                    // ~200k/night the dump path does.
                    let age_ms = now - a.start;
                    if a.start > 0.0 && age_ms < *finder_core::config::BED_GRACE_MS {
                        eprintln!(
                            "BED: follow saw {} at age {:.0}ms — buyable in {:.0}ms",
                            a.uuid,
                            age_ms,
                            *finder_core::config::BED_GRACE_MS - age_ms
                        );
                    }
                    let key = idx.final_key(&attrs);
                    let d = DecodedAuction {
                        a: ActiveAuction {
                            uuid: a.uuid.clone(),
                            starting_bid: a.starting_bid,
                            auctioneer: a.auctioneer.clone(),
                            item_name: a.item_name.clone(),
                        },
                        attrs,
                        key,
                    };
                    evaluated += 1;
                    eval_new(
                        d,
                        &idx,
                        &model,
                        prev_by_key,
                        seen,
                        relist,
                        now,
                        now,
                        &mut scratch_lbin,
                        &mut |f: &Flip| {
                            // A bed is not buyable by ANYONE until it lifts, so
                            // there is no race to win by pushing early — and a bot
                            // handed it 15s early just holds the auction window
                            // open doing nothing (downtime + ban surface). Hold it
                            // and release it shortly before the lift instead.
                            if let Some(t) = shared.purchase_at_ms(&f.uuid) {
                                let due = t - *finder_core::config::BED_PUSH_LEAD_MS;
                                if due > now {
                                    eprintln!(
                                        "BED: holding {} for {:.0}ms (lifts in {:.0}ms)",
                                        f.uuid,
                                        due - now,
                                        t - now
                                    );
                                    bed_queue.push((due, f.clone()));
                                    return;
                                }
                            }
                            if post_flip(f, prev_by_key, &idx, &shared, &recent, &discord)
                                .mismatch
                                .is_none()
                            {
                                posted += 1;
                            }
                        },
                    );
                }
                if evaluated > 0 {
                    eprintln!(
                        "seller-follow: {} priced {evaluated} listing(s), {posted} pushed",
                        batch.seller
                    );
                }
            }
            if cfg_store.reload_if_changed() {
                *shared.filters.write().unwrap() = cfg_store.filters.clone();
                eprintln!("ws-config reloaded");
            }
            if let Some(f) = bin_store.poll() {
                eprintln!(
                    "BinMaster filter {}",
                    if f.is_some() {
                        "reloaded"
                    } else {
                        "removed → ws-config thresholds"
                    }
                );
                shared.set_bin_filter(f);
            }
            if last_ended.elapsed() >= Duration::from_millis(ended_ms) {
                let (fetched, stored, resolved, bought) = collect_ended(&store);
                eprintln!("collectEnded: fetched {fetched}, stored {stored}, resold {resolved}, bought {bought}");
                last_ended = Instant::now();
            }
            // Swap in a finished background rebuild (never blocks; see above).
            if let Ok(res) = rebuild_rx.try_recv() {
                rebuild_in_flight = false;
                last_rebuild = Instant::now();
                if let Some((ni, nmodel)) = res {
                    // ⚠️ Do NOT write `idx = ni`. That assignment DROPS the old
                    // PriceIndex in place, on the loop thread, and the old index is
                    // ~43k keys over millions of refs -- freeing it is millions of
                    // deallocations while dumps are waiting in the channel.
                    //
                    // Measured over 1325 sweeps (2026-08-08): the sweep immediately
                    // after a swap had FIRSTFLIPMS p99 **1605ms** and max 1695ms
                    // against p99 107ms for every other sweep, and 4 of the 7
                    // sweeps in the whole log above 300ms sat right after a swap.
                    // A 1.6s stall is a whole sweep gone blind, and 76.7% of flip
                    // profit dies inside 1s ([[finder-latency-budget-2026-07-29]]).
                    //
                    // So hand the corpses to a detached thread. RSS is unaffected:
                    // both indexes were already alive together for the 41s build,
                    // this only moves WHERE the free is paid.
                    let old_idx = std::mem::replace(&mut idx, ni);
                    let old_model = std::mem::replace(&mut model, nmodel);
                    std::thread::spawn(move || {
                        drop(old_idx);
                        drop(old_model);
                    });
                    eprintln!(
                        "rebuild swapped in: {} keys + {} models (RSS {:.0}MB)",
                        idx.key_count(),
                        model.model_count(),
                        rss_mb()
                    );
                }
            }
            if !rebuild_in_flight && last_rebuild.elapsed() >= Duration::from_millis(rebuild_ms) {
                rebuild_in_flight = true;
                let (tx, dbp, bm) = (rebuild_tx.clone(), db.clone(), bazaar_map.clone());
                std::thread::spawn(move || {
                    let t = Instant::now();
                    let nm = now_ms_real();
                    // ONE own read-only connection for both loads: WAL lets it
                    // scan `sold` without holding the store Mutex, so handle_dump
                    // keeps running. Opening twice would re-run the schema DDL and
                    // migrations on every rebuild for nothing.
                    let conn = match store::Store::open(&dbp, false) {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!("bg rebuild: store open failed: {e}");
                            let _ = tx.send(None);
                            return;
                        }
                    };
                    let refs = match conn.load_references(nm) {
                        Ok(mut r) => {
                            cap_ram(&mut r);
                            r
                        }
                        Err(e) => {
                            eprintln!("bg rebuild: load_references failed: {e}");
                            let _ = tx.send(None);
                            return;
                        }
                    };
                    // An empty map (table missing, or a censor sweep that has not
                    // run yet) makes this identical to `PriceIndex::build`, so a
                    // failure here costs the sell-through numbers and nothing else.
                    let censored = conn.load_censored(nm).unwrap_or_else(|e| {
                        eprintln!("bg rebuild: load_censored failed, sell-through disabled this cycle: {e}");
                        Default::default()
                    });
                    let bazaar = Bazaar::from_prices(bm, nm);
                    let idx = PriceIndex::build_with_survival(refs, bazaar, nm, censored);
                    let model = ModifierModel::rebuild(idx.references(), &idx, nm);
                    eprintln!(
                        "bg rebuild built {} keys in {:.0}ms — swapping in",
                        idx.key_count(),
                        t.elapsed().as_secs_f64() * 1000.0
                    );
                    let _ = tx.send(Some((idx, model)));
                });
            }
            if last_flush.elapsed() >= Duration::from_secs(30) {
                shared.flush_cost_basis();
                // index.ts:1180 persists on the same 30s cadence, so a restart
                // doesn't lose flips a bot just bought (baf-backend's cross-match
                // retries would otherwise never hit).
                if let Err(e) = recent.lock().unwrap().save() {
                    eprintln!("recent-flips save failed: {e}");
                }
                last_flush = Instant::now();
            }
            // prod runs both prunes every 6h (index.ts:1267-1268).
            if last_prune.elapsed() >= Duration::from_secs(6 * 3600) {
                let mut st = store.lock().unwrap();
                match st.prune_old() {
                    Ok(n) if n > 0 => eprintln!("pruneOld: dropped {n} refs past retention"),
                    Err(e) => eprintln!("pruneOld failed: {e}"),
                    _ => {}
                }
                match st.prune_listings() {
                    Ok(n) if n > 0 => {
                        eprintln!("pruneListings: censored {n} expired unsold listings")
                    }
                    Err(e) => eprintln!("pruneListings failed: {e}"),
                    _ => {}
                }
                drop(st);
                last_prune = Instant::now();
            }
            // Blocks only 50ms at a time so maintenance above keeps ticking, but
            // a real dump wakes this instantly — the Start lands before any bins.
            match dump_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(detect::DumpMsg::Start(s)) => break s,
                // Leftovers from a sweep we abandoned (stream abort): drop them
                // rather than mixing them into the next dump.
                Ok(_) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    eprintln!("detect lanes gone — exiting serve loop");
                    return;
                }
            }
        };
        handle_dump(
            dump,
            &dump_rx,
            &mut mem,
            &idx,
            &model,
            &shared,
            &store,
            &dsh,
            page_concurrency,
            &recent,
            &discord,
            &follow.follow_tx,
        );
    }
}

fn main() {
    let db = std::env::var("DB_PATH")
        .unwrap_or_else(|_| "./data/auctions.sqlite".to_string());

    // Standalone bazaar collector: record bazaar snapshots into their own sqlite
    // file and nothing else. Never touches the finder DB, index, or ws feed.
    if std::env::var("BAZAAR_COLLECT_ONLY").as_deref() == Ok("1") {
        eprintln!("finder-rs: BAZAAR_COLLECT_ONLY, running bazaar collector only");
        bazaar_collect::run_from_env(&db);
        return;
    }

    // Print what the bazaar finder would do right now, then exit. Read-only: it
    // touches the collector's db and the public bazaar endpoint, nothing else.
    if std::env::var("BZ_REPORT").as_deref() == Ok("1") {
        bazaar_finder::report(&bazaar_collect::db_path(&db));
        return;
    }

    let now_ms: i64 = std::env::var("NOW_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(now_ms_real);
    let sweep = std::env::var("SWEEP").map(|v| v == "1").unwrap_or(false);
    let compare = std::env::var("COMPARE").map(|v| v == "1").unwrap_or(false);
    let serve = std::env::var("SERVE").map(|v| v == "1").unwrap_or(false);
    eprintln!(
        "finder-rs: DB_PATH={db} NOW_MS={now_ms} sweep={sweep} compare={compare} serve={serve}"
    );

    // Live bazaar (saved for the TS side in compare mode; kept for serve rebuilds).
    let bazaar_map = if std::env::var("NO_BAZAAR").is_ok() {
        HashMap::new()
    } else {
        hypixel::fetch_bazaar()
    };
    eprintln!("bazaar products: {}", bazaar_map.len());
    if compare {
        std::fs::write(
            "/tmp/cmp-bazaar.json",
            serde_json::to_string(&bazaar_map).unwrap(),
        )
        .unwrap();
    }
    let bazaar = Bazaar::from_prices(bazaar_map.clone(), now_ms);

    let store = std::sync::Arc::new(std::sync::Mutex::new(
        store::Store::open(&db, true).expect("open store"),
    ));
    eprintln!(
        "sold rows in file: {}",
        store.lock().unwrap().sold_count().unwrap_or(0)
    );
    let t = Instant::now();
    let mut refs = store
        .lock()
        .unwrap()
        .load_references(now_ms)
        .expect("load references");
    eprintln!(
        "loaded {} refs in {:.0} ms",
        refs.len(),
        t.elapsed().as_secs_f64() * 1000.0
    );
    cap_ram(&mut refs);
    let censored = store
        .lock()
        .unwrap()
        .load_censored(now_ms)
        .unwrap_or_else(|e| {
            eprintln!("load_censored failed, sell-through disabled: {e}");
            Default::default()
        });
    eprintln!("loaded censored listings for {} items", censored.len());
    let t = Instant::now();
    let idx = PriceIndex::build_with_survival(refs, bazaar, now_ms, censored);
    eprintln!(
        "price index: {} keys in {:.0} ms",
        idx.key_count(),
        t.elapsed().as_secs_f64() * 1000.0
    );
    let t = Instant::now();
    // Read the refs back out of the index rather than keeping a second copy:
    // `idx.references()` and `&idx` are both shared borrows. At a 14-day window
    // the clone this replaces was ~4GB.
    let model = ModifierModel::rebuild(idx.references(), &idx, now_ms);
    eprintln!(
        "modifier model: {} models in {:.0} ms (RSS {:.0} MB)",
        model.model_count(),
        t.elapsed().as_secs_f64() * 1000.0,
        rss_mb()
    );

    if serve {
        serve_loop(db, store, idx, model, bazaar_map);
        return;
    }
    if !sweep && !compare {
        eprintln!("BOOT OK. Set SWEEP=1 (log flips), COMPARE=1 (Rust-vs-TS), or SERVE=1 (ws feed + loop).");
        return;
    }

    // Fetch live auctions (saved in compare mode).
    let (auctions, last_updated) = hypixel::fetch_all_auctions();
    if compare {
        std::fs::write(
            "/tmp/cmp-auctions.json",
            serde_json::to_string(&auctions).unwrap(),
        )
        .unwrap();
        std::fs::write(
            "/tmp/cmp-meta.json",
            serde_json::json!({"lastUpdated": last_updated}).to_string(),
        )
        .unwrap();
    }

    let dump_age_ms = now_ms as f64 - last_updated;
    let (decoded, decode_ms) = decode_all(&auctions, &idx);
    eprintln!(
        "decoded {} BIN auctions in {:.0} ms",
        decoded.len(),
        decode_ms
    );
    let t = Instant::now();
    let flips = run_pipeline(&decoded, &idx, &model, now_ms, last_updated);
    let find_ms = t.elapsed().as_secs_f64() * 1000.0 + decode_ms;
    // Per-flip found-after-dump-release = dumpAge + decode + pipeline-elapsed-when-found.
    let flip_outs: Vec<FlipOut> = flips
        .iter()
        .map(|(f, pipe_ms)| flip_out(f, dump_age_ms + decode_ms + pipe_ms))
        .collect();
    let mut lanes: HashMap<&str, usize> = HashMap::new();
    for (f, _) in &flips {
        *lanes.entry(f.finder.as_str()).or_insert(0) += 1;
    }
    eprintln!(
        "RUST: {} candidates → {} flips in {:.0} ms (decode {:.0}ms + eval {:.0}ms)  lanes={:?}",
        decoded.len(),
        flips.len(),
        find_ms,
        decode_ms,
        find_ms - decode_ms,
        lanes
    );

    if sweep {
        for fo in &flip_outs {
            // COMPARE/SWEEP path: TTS is a live-serve measurement only, keep this
            // output byte-identical to what goldens compare.
            log_flip_out(fo, now_ms, None, None, None);
        }
        return;
    }

    // ---- COMPARE: run the TS finder on the identical dump, then post ----
    let rust_result = RunResult {
        engine: "rust".into(),
        find_ms,
        decode_ms,
        dump_age_ms,
        candidates: decoded.len(),
        count: flips.len(),
        flips: flip_outs,
    };
    std::fs::write(
        "/tmp/cmp-rust.json",
        serde_json::to_string(&rust_result).unwrap(),
    )
    .unwrap();

    eprintln!("running TS finder on the identical dump…");
    let ts_oracle = std::env::var("TS_ORACLE")
        .unwrap_or_else(|_| "./ts-oracle".to_string());
    let status = std::process::Command::new("node")
        .arg(format!("{ts_oracle}/dist/tools/goldens/livefind.js"))
        .arg("/tmp/cmp-bazaar.json")
        .arg("/tmp/cmp-auctions.json")
        .arg("/tmp/cmp-meta.json")
        .arg("/tmp/cmp-ts.json")
        .env("DB_PATH", &db)
        .env("NOW_MS", now_ms.to_string())
        .env("NODE_OPTIONS", "--max-old-space-size=6144")
        // Forward the webhook so livefind posts the found flips (each with the
        // 🦀 Rust-vs-TS speed field added to the discord.ts embed).
        .env(
            "DISCORD_WEBHOOK_URL",
            std::env::var("DISCORD_WEBHOOK_URL").unwrap_or_default(),
        )
        .status()
        .expect("run node livefind");
    if !status.success() {
        eprintln!("TS finder failed; posting Rust-only result");
    }
    let ts_result: Option<RunResult> = std::fs::read_to_string("/tmp/cmp-ts.json")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());

    // Comparison summary to stderr. Webhook posting is done by the TS livefind
    // (it posts the top flips, each with the 🦀 Rust field added to the embed).
    if let Some(ts) = &ts_result {
        let ru: std::collections::HashSet<&str> =
            rust_result.flips.iter().map(|f| f.uuid.as_str()).collect();
        let overlap = ts
            .flips
            .iter()
            .filter(|f| ru.contains(f.uuid.as_str()))
            .count();
        let speedup = if rust_result.find_ms > 0.0 {
            ts.find_ms / rust_result.find_ms
        } else {
            0.0
        };
        eprintln!(
            "COMPARISON: Rust {:.0}ms ({} flips) vs TS {:.0}ms ({} flips) -> {:.1}x faster; agreement {}/{}",
            rust_result.find_ms, rust_result.count, ts.find_ms, ts.count, speedup, overlap, rust_result.count
        );
    }
}
