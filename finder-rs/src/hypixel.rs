//! Port of the read half of `baf-flip-finder/src/bazaar.ts` (live refresh) and
//! `src/hypixel.ts` (auction page sweep) — the read-only client used by the live
//! shadow sweep. Blocking reqwest (single-shot sweep; the async detect/page lanes
//! from the Phase-1 shadow land in the orchestration chunk).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const BAZAAR_URL: &str = "https://api.hypixel.net/v2/skyblock/bazaar";
const AUCTIONS_URL: &str = "https://api.hypixel.net/v2/skyblock/auctions?page=";
const ENDED_URL: &str = "https://api.hypixel.net/v2/skyblock/auctions_ended";
const AUCTION_BY_PLAYER_URL: &str = "https://api.hypixel.net/v2/skyblock/auction?player=";

/// Base for the per-player auction lookup (seller-follow / pageflipper seller →
/// uuid resolution). Override with `SELLER_LOOKUP_BASE` to route it through a
/// proxy such as NetherAPI (`https://netherapi.com/api`), a shared pool of
/// Hypixel keys: same data, path, query and `API-Key` header, just a working key
/// and higher rate limits. Defaults to the official Hypixel API. Only the
/// per-player lookup moves; the free paginated dump stays on Hypixel.
fn auction_by_player_url() -> String {
    match std::env::var("SELLER_LOOKUP_BASE") {
        Ok(b) if !b.trim().is_empty() => {
            format!(
                "{}/v2/skyblock/auction?player=",
                b.trim().trim_end_matches('/')
            )
        }
        _ => AUCTION_BY_PLAYER_URL.to_string(),
    }
}

/// Shared, connection-pooled HTTP client — built ONCE and reused across sweeps.
///
/// The old `fn client()` built a fresh client (and thus a fresh, cold socket
/// pool) on every call, so each 60s deep-page sweep paid fresh TLS handshakes
/// and, worse, had no bound short of the 30s request timeout when a socket hung.
/// TS keeps 64 sockets warm for 90s for exactly this reason (hypixel.ts:9). Warm
/// pool + a 3s connect ceiling + a 12s request ceiling (down from 30s): the
/// per-request `.timeout()` in the deep-page fetch tightens that further.
fn client() -> &'static reqwest::blocking::Client {
    static C: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .user_agent(crate::USER_AGENT)
            .gzip(true)
            .pool_max_idle_per_host(16)
            .pool_idle_timeout(Duration::from_secs(300))
            .tcp_keepalive(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(12))
            .build()
            .expect("client")
    })
}

/// A second, fully separate blocking client for anything that is NOT the
/// millisecond-critical page-0 detection path: the bazaar collector, and the
/// per-player auction lookup (seller-follow / self-listing poll).
///
/// `reqwest::blocking::Client` drives its actual I/O on a small internal async
/// runtime; sharing ONE client instance across a background thread/task and the
/// detection path means they also share that hidden runtime, so a big or
/// oddly-timed background response can contend with an in-flight auction-dump
/// request on the exact same underlying executor. Measured for the bazaar
/// collector specifically: every ~6min poll (a 2.4MB/1933-product response)
/// lined up with a ~2x detectLagMs spike on whichever lane's sweep landed at
/// the same moment (7-8s baseline -> 14-15s), 100% correlated. The bazaar
/// collector module already claims to be "its own thread, no shared state, no
/// shared connection" — this was the one connection it was still sharing.
/// Per-player lookups are much smaller than the bazaar dump so the same-magnitude
/// spike hasn't been directly measured for them, but they share the identical
/// mechanism and fire on their own schedule (every 45s per listing bot, plus
/// on-demand for pageflipper's seller-follow), so they get the same isolation
/// on the same reasoning rather than waiting to prove it the hard way too. The
/// hot page-0 fetch and the ended-auctions poll keep `client()` untouched.
fn side_channel_client() -> &'static reqwest::blocking::Client {
    static C: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .gzip(true)
            .pool_max_idle_per_host(4)
            .pool_idle_timeout(Duration::from_secs(300))
            .tcp_keepalive(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(12))
            .build()
            .expect("side-channel client")
    })
}

/// Strip the two colour markups Hypixel's item dump mixes, leaving the plain
/// text a player actually sees (and therefore types).
///
/// The dump is not consistent: 22 names carry in-band `§<code>` and 9 carry a
/// `%%<colour>%%` marker instead (`%%red%%Volcanic Rock`, `%%green%%Elle's
/// Supplies`). Only `§` was handled at first, which put a literal
/// `%%red%%Volcanic Rock` into the bazaar finder's tradeable list — a string
/// that matches nothing in the search GUI, so the order could never be placed.
fn clean_display_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '§' {
            chars.next(); // drop the one-char colour code that follows
            continue;
        }
        if c == '%' && chars.peek() == Some(&'%') {
            chars.next(); // consume the second opening '%'
                          // Skip to the closing `%%`. An unterminated marker consumes the
                          // rest, which is right: it is markup, not a name.
            let mut prev_pct = false;
            for c2 in chars.by_ref() {
                if c2 == '%' {
                    if prev_pct {
                        break;
                    }
                    prev_pct = true;
                } else {
                    prev_pct = false;
                }
            }
            continue;
        }
        out.push(c);
    }
    out.trim().to_string()
}

/// item id → display name, from the public `resources/skyblock/items` dump.
///
/// The bazaar finder needs this because a bot places an order by TYPING the
/// item's display name into the bazaar search GUI: an order keyed by
/// `ENCHANTED_COCOA` would search for a string that doesn't exist in game.
pub fn fetch_item_names() -> HashMap<String, String> {
    #[derive(Deserialize)]
    struct Item {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
    }
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        success: bool,
        #[serde(default)]
        items: Vec<Item>,
    }
    let resp: Resp = match side_channel_client()
        .get("https://api.hypixel.net/v2/resources/skyblock/items")
        .send()
        .and_then(|r| r.json())
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("item-names fetch failed: {e}");
            return HashMap::new();
        }
    };
    if !resp.success {
        eprintln!("item-names: success=false");
        return HashMap::new();
    }
    let mut out = HashMap::with_capacity(resp.items.len());
    for it in resp.items {
        if it.id.is_empty() || it.name.is_empty() {
            continue;
        }
        let name = clean_display_name(&it.name);
        if !name.is_empty() {
            out.insert(it.id, name);
        }
    }
    out
}

/// Live bazaar: product_id → instant-sell value (sellPrice, else buyPrice).
pub fn fetch_bazaar() -> HashMap<String, f64> {
    #[derive(Deserialize)]
    struct Resp {
        success: bool,
        #[serde(default)]
        products: HashMap<String, Product>,
    }
    #[derive(Deserialize)]
    struct Product {
        quick_status: Qs,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Qs {
        sell_price: f64,
        buy_price: f64,
    }
    let mut out = HashMap::new();
    match client()
        .get(BAZAAR_URL)
        .send()
        .and_then(|r| r.json::<Resp>())
    {
        Ok(r) if r.success => {
            for (id, p) in r.products {
                let v = if p.quick_status.sell_price > 0.0 {
                    p.quick_status.sell_price
                } else {
                    p.quick_status.buy_price
                };
                if v > 0.0 {
                    out.insert(id, v);
                }
            }
        }
        Ok(_) => eprintln!("bazaar: success=false"),
        Err(e) => eprintln!("bazaar fetch failed: {e}"),
    }
    out
}

/// One product's full aggregate status from `/v2/skyblock/bazaar`: the whole
/// `quick_status`, plus the current top-of-book from the order summaries. The
/// `buy*` fields are the INSTA-BUY side and `sell*` the INSTA-SELL side, so
/// `margin = sell_price - buy_price` before the 1.25% bazaar tax. (Hypixel's
/// summaries are inverted vs intuition, verified empirically: `quick_status.buyPrice`
/// tracks `buy_summary[0]`, not `sell_summary[0]`.) This is the collector's richer
/// counterpart to `fetch_bazaar`, which only extracts one value for pricing.
#[derive(Debug, Clone)]
pub struct BazaarProduct {
    /// quick_status.buyPrice: weighted unit price to INSTA-BUY now (what you pay).
    pub buy_price: f64,
    /// quick_status.sellPrice: weighted unit price to INSTA-SELL now (what you receive).
    pub sell_price: f64,
    /// quick_status.buyVolume: insta-buy-side volume (raw API field).
    pub buy_volume: i64,
    /// quick_status.sellVolume: insta-sell-side volume (raw API field).
    pub sell_volume: i64,
    /// quick_status.buyMovingWeek: units moved on the buy side over the past week.
    pub buy_moving_week: i64,
    /// quick_status.sellMovingWeek: units moved on the sell side over the past week.
    pub sell_moving_week: i64,
    /// quick_status.buyOrders: distinct orders on the insta-buy side.
    pub buy_orders: i64,
    /// quick_status.sellOrders: distinct orders on the insta-sell side.
    pub sell_orders: i64,
    /// Current best insta-BUY unit price = top of `buy_summary` (matches buyPrice).
    /// 0.0 when that book side is empty.
    pub top_insta_buy: f64,
    /// Units resting AT `top_insta_buy`, and how many distinct offers make them
    /// up. This is the queue a sell offer of ours would have to get through (or
    /// jump, by undercutting) before it fills — the single thing the bazaar
    /// finder needs that the aggregate volumes cannot express.
    pub top_insta_buy_amount: i64,
    pub top_insta_buy_orders: i64,
    /// Current best insta-SELL unit price = top of `sell_summary` (matches sellPrice).
    /// 0.0 when that book side is empty.
    pub top_insta_sell: f64,
    /// Units and orders resting AT `top_insta_sell`: the queue ahead of a buy
    /// order of ours placed at that same price.
    pub top_insta_sell_amount: i64,
    pub top_insta_sell_orders: i64,
    /// The resting book beyond the top, as `(unit price, units)`.
    ///
    /// `ask_levels` is Hypixel's `buy_summary` (the sell offers, cheapest
    /// first) and `bid_levels` is its `sell_summary` (the buy orders, highest
    /// first). The names are swapped relative to the API on purpose: theirs are
    /// named for what a *taker* does, which reads backwards for anything
    /// reasoning about where a resting order sits.
    ///
    /// Needed because the top of book alone cannot say whether a price is a
    /// real level or one thin order: `FIG_LOG` shows an ask of 8.1 backed by
    /// 707 units in ONE order, with the actual supply far below it. Only the
    /// levels behind the top reveal that, and they arrive free in every poll.
    pub ask_levels: Vec<(f64, i64)>,
    pub bid_levels: Vec<(f64, i64)>,
}

/// A whole-bazaar snapshot: the API's `lastUpdated` (unix ms) and every product.
pub struct BazaarFull {
    pub last_updated: i64,
    pub products: HashMap<String, BazaarProduct>,
}

/// Fetch the FULL bazaar (order-book depth top + volumes + moving-week) for the
/// snapshot collector. `None` on any transport/parse error or `success:false`.
pub fn fetch_bazaar_full() -> Option<BazaarFull> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase")]
    struct Qs {
        #[serde(default)]
        buy_price: f64,
        #[serde(default)]
        sell_price: f64,
        #[serde(default)]
        buy_volume: f64,
        #[serde(default)]
        sell_volume: f64,
        #[serde(default)]
        buy_moving_week: f64,
        #[serde(default)]
        sell_moving_week: f64,
        #[serde(default)]
        buy_orders: f64,
        #[serde(default)]
        sell_orders: f64,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Order {
        #[serde(default)]
        price_per_unit: f64,
        #[serde(default)]
        amount: f64,
        #[serde(default)]
        orders: f64,
    }
    #[derive(Deserialize)]
    struct Product {
        #[serde(default)]
        quick_status: Qs,
        #[serde(default)]
        sell_summary: Vec<Order>,
        #[serde(default)]
        buy_summary: Vec<Order>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Resp {
        #[serde(default)]
        success: bool,
        #[serde(default)]
        last_updated: f64,
        #[serde(default)]
        products: HashMap<String, Product>,
    }
    let resp: Resp = match side_channel_client()
        .get(BAZAAR_URL)
        .send()
        .and_then(|r| r.json())
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("bazaar-full fetch failed: {e}");
            return None;
        }
    };
    if !resp.success {
        eprintln!("bazaar-full: success=false");
        return None;
    }
    let mut products = HashMap::with_capacity(resp.products.len());
    for (id, p) in resp.products {
        let qs = p.quick_status;
        products.insert(
            id,
            BazaarProduct {
                buy_price: qs.buy_price,
                sell_price: qs.sell_price,
                buy_volume: qs.buy_volume.round() as i64,
                sell_volume: qs.sell_volume.round() as i64,
                buy_moving_week: qs.buy_moving_week.round() as i64,
                sell_moving_week: qs.sell_moving_week.round() as i64,
                buy_orders: qs.buy_orders.round() as i64,
                sell_orders: qs.sell_orders.round() as i64,
                top_insta_buy: p
                    .buy_summary
                    .first()
                    .map(|o| o.price_per_unit)
                    .unwrap_or(0.0),
                top_insta_buy_amount: p
                    .buy_summary
                    .first()
                    .map(|o| o.amount.round() as i64)
                    .unwrap_or(0),
                top_insta_buy_orders: p
                    .buy_summary
                    .first()
                    .map(|o| o.orders.round() as i64)
                    .unwrap_or(0),
                top_insta_sell: p
                    .sell_summary
                    .first()
                    .map(|o| o.price_per_unit)
                    .unwrap_or(0.0),
                top_insta_sell_amount: p
                    .sell_summary
                    .first()
                    .map(|o| o.amount.round() as i64)
                    .unwrap_or(0),
                top_insta_sell_orders: p
                    .sell_summary
                    .first()
                    .map(|o| o.orders.round() as i64)
                    .unwrap_or(0),
                // Hypixel returns at most 30 levels a side; keep them as sent.
                ask_levels: p
                    .buy_summary
                    .iter()
                    .map(|o| (o.price_per_unit, o.amount.round() as i64))
                    .collect(),
                bid_levels: p
                    .sell_summary
                    .iter()
                    .map(|o| (o.price_per_unit, o.amount.round() as i64))
                    .collect(),
            },
        );
    }
    Some(BazaarFull {
        last_updated: resp.last_updated.round() as i64,
        products,
    })
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct RawAuction {
    pub uuid: String,
    #[serde(default)]
    pub auctioneer: Option<String>,
    #[serde(default)]
    pub starting_bid: f64,
    #[serde(default)]
    pub item_name: String,
    #[serde(default)]
    pub bin: bool,
    #[serde(default)]
    pub item_bytes: String,
    /// Listing start (epoch ms). Feeds the B1 TTS capture (index.ts:534).
    #[serde(default)]
    pub start: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AucPage {
    #[serde(default)]
    total_pages: i64,
    #[serde(default)]
    last_updated: f64,
    #[serde(default)]
    auctions: Vec<RawAuction>,
}

/// Fetch every auction page. Returns (auctions, lastUpdated).
pub fn fetch_all_auctions() -> (Vec<RawAuction>, f64) {
    let c = client();
    let mut all: Vec<RawAuction> = Vec::new();
    let first: AucPage = match c
        .get(format!("{AUCTIONS_URL}0"))
        .send()
        .and_then(|r| r.json())
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("auctions page 0 failed: {e}");
            return (all, 0.0);
        }
    };
    let total_pages = first.total_pages.max(1);
    let last_updated = first.last_updated;
    all.extend(first.auctions);
    for page in 1..total_pages {
        match c
            .get(format!("{AUCTIONS_URL}{page}"))
            .send()
            .and_then(|r| r.json::<AucPage>())
        {
            Ok(p) => all.extend(p.auctions),
            Err(e) => eprintln!("auctions page {page} failed: {e}"),
        }
    }
    eprintln!("fetched {} auctions over {total_pages} pages", all.len());
    (all, last_updated)
}

/// Fetch pages `start..total` concurrently and return their auctions.
///
/// The detect lane already streamed page 0, so the dump-driven sweep only needs
/// the deep pages. Prod fetches these with `config.pageConcurrency` in flight;
/// fetching them serially would add seconds to every sweep, so mirror the
/// concurrency here. Page order is not preserved (byKey is order-insensitive;
/// per-key lists get sorted by price afterwards).
pub fn fetch_pages_from(start: i64, total: i64, concurrency: usize) -> Vec<RawAuction> {
    use std::sync::mpsc;
    let mut all: Vec<RawAuction> = Vec::new();
    if start >= total {
        return all;
    }
    let pages: Vec<i64> = (start..total).collect();
    let next = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel::<Vec<RawAuction>>();
    let c = client().clone(); // cheap Arc clone of the warm shared pool
                              // Deep pages carry ~0 new flips (all new BINs land on page 0) and only feed
                              // the full-dump byKey/state roll, which self-heals next sweep. So a slow or
                              // hung page must NEVER hold the single loop thread: bound each request to 6s
                              // and stop pulling new pages past DEEP_DEADLINE_MS. Before this, one stalled
                              // page could hold the whole sweep for the 30s client timeout (observed
                              // full_ms up to 30.7s), starving the "found" feed and the estimate RPCs.
    let deadline_ms: u64 = std::env::var("DEEP_DEADLINE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8000);
    let deadline = std::time::Instant::now() + Duration::from_millis(deadline_ms);
    std::thread::scope(|s| {
        for _ in 0..concurrency.clamp(1, 16) {
            let (next, tx, c, pages) = (next.clone(), tx.clone(), c.clone(), &pages);
            s.spawn(move || loop {
                if std::time::Instant::now() >= deadline {
                    break; // past deadline: stop pulling, in-flight bounded by the 6s per-request timeout
                }
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(&page) = pages.get(i) else { break };
                match c
                    .get(format!("{AUCTIONS_URL}{page}"))
                    .timeout(Duration::from_secs(6))
                    .send()
                    .and_then(|r| r.json::<AucPage>())
                {
                    Ok(p) => {
                        let _ = tx.send(p.auctions);
                    }
                    Err(e) => eprintln!("auctions page {page} failed: {e}"),
                }
            });
        }
        drop(tx);
        let mut pages_got = 0usize;
        for batch in rx {
            all.extend(batch);
            pages_got += 1;
        }
        // Honest signal: fewer pages than requested means the deadline tripped or
        // a page errored. The missing carried-over BINs rebuild on the next sweep.
        if pages_got < pages.len() {
            eprintln!(
                "deep-page fetch: {pages_got}/{} pages (deadline {deadline_ms}ms) — stragglers dropped, self-heal next sweep",
                pages.len()
            );
        }
    });
    all
}

/// A genuinely-new deep-page BIN, screened and NBT-decoded on the fetch worker
/// thread (decode touches no shared pricing state, so it is safe off the loop
/// thread — the exact same split the page-0 detect lanes use). The loop thread
/// only does keying / eval / dedup, which need the PriceIndex.
pub struct DeepNew {
    pub uuid: String,
    pub starting_bid: f64,
    pub auctioneer: Option<String>,
    pub item_name: String,
    pub attrs: finder_core::nbt::ItemAttributes,
    pub start: f64,
}

/// One deep page's screened result, streamed to the loop thread as the page lands.
pub enum DeepMsg {
    /// New BINs (passed the prev-live screen + decoded here).
    New(Vec<DeepNew>),
    /// Carried-over BINs: (uuid, starting_bid). Attrs come from the loop thread's
    /// decode cache, exactly like page 0's `old_bins` — never re-decoded.
    Old(Vec<(String, f64)>),
}

/// Per-auction screen verdict, factored out of the worker so it is unit-testable
/// without a network. Mirrors the OLD deep for-loop's per-auction branching
/// (main.rs), just moved off the loop thread: non-BIN / invalid → Skip,
/// carried-over → Old, genuinely-new-and-decodable → New. `live` is the PREVIOUS
/// full sweep's live-BIN set (`prev_live_uuids`); `primed` is false only on the
/// priming sweep, where everything counts as new (and gets decoded to seed caches).
pub enum DeepScreen {
    // Boxed: `DeepNew` carries the full decoded `ItemAttributes` (~640B) while the
    // other variants are tiny, so an unboxed enum would bloat every screen verdict.
    New(Box<DeepNew>),
    Old((String, f64)),
    Skip,
}

pub fn screen_deep_auction(
    a: RawAuction,
    primed: bool,
    live: &std::collections::HashSet<String>,
) -> DeepScreen {
    if !a.bin {
        return DeepScreen::Skip;
    }
    // Carried-over: shipped as (uuid, bid) only, attrs reused from cache. Checked
    // BEFORE the validity gate so a carried BIN with a zero/absent bid still rolls
    // into old_bins, exactly as the old loop did.
    if primed && live.contains(&a.uuid) {
        return DeepScreen::Old((a.uuid, a.starting_bid));
    }
    if a.starting_bid <= 0.0 || a.item_bytes.is_empty() {
        return DeepScreen::Skip;
    }
    match finder_core::nbt::decode_item_bytes(&a.item_bytes) {
        Some(attrs) => DeepScreen::New(Box::new(DeepNew {
            uuid: a.uuid,
            starting_bid: a.starting_bid,
            auctioneer: a.auctioneer,
            item_name: a.item_name,
            attrs,
            start: a.start,
        })),
        None => DeepScreen::Skip,
    }
}

/// Streaming twin of `fetch_pages_from`: fetch deep pages `start..total`
/// concurrently on detached worker threads and push each page's screened+decoded
/// result over a channel **as it lands**, instead of collecting every page before
/// returning. This lets the caller evaluate a flip on page 1 the moment page 1
/// arrives rather than waiting for the slowest of ~46 pages — the whole point of
/// the change (a deep flip went from ~1.5s-after-page-0 to ~its-page's-arrival).
///
/// Returns `(rx, pages_got)`. The `rx` iterator ends once every worker thread
/// finishes (all senders dropped); `pages_got` then holds how many pages actually
/// landed (< expected ⇒ the `DEEP_DEADLINE_MS` deadline tripped or a page errored,
/// which self-heals on the next sweep just like `fetch_pages_from`).
///
/// Money-safety: screening + NBT decode run on the workers (no shared pricing
/// state); the caller keeps keying / eval / pricing / dedup on the one loop thread.
pub fn fetch_pages_streaming(
    start: i64,
    total: i64,
    concurrency: usize,
    live: std::sync::Arc<std::collections::HashSet<String>>,
    primed: bool,
) -> (
    std::sync::mpsc::Receiver<DeepMsg>,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};
    let (tx, rx) = mpsc::channel::<DeepMsg>();
    let pages_got = Arc::new(AtomicUsize::new(0));
    if start >= total {
        return (rx, pages_got);
    }
    let pages: Arc<Vec<i64>> = Arc::new((start..total).collect());
    let next = Arc::new(AtomicUsize::new(0));
    let deadline_ms: u64 = std::env::var("DEEP_DEADLINE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8000);
    let deadline = std::time::Instant::now() + Duration::from_millis(deadline_ms);
    for _ in 0..concurrency.clamp(1, 16) {
        let (next, tx, pages, live, pages_got) = (
            next.clone(),
            tx.clone(),
            pages.clone(),
            live.clone(),
            pages_got.clone(),
        );
        std::thread::spawn(move || {
            let c = client();
            loop {
                if Instant::now() >= deadline {
                    break; // past deadline: stop pulling; in-flight bounded by the 6s per-request timeout
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(&page) = pages.get(i) else { break };
                let p: AucPage = match c
                    .get(format!("{AUCTIONS_URL}{page}"))
                    .timeout(Duration::from_secs(6))
                    .send()
                    .and_then(|r| r.json::<AucPage>())
                {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("auctions page {page} failed: {e}");
                        continue;
                    }
                };
                let mut new_batch: Vec<DeepNew> = Vec::new();
                let mut old_batch: Vec<(String, f64)> = Vec::new();
                for a in p.auctions {
                    match screen_deep_auction(a, primed, &live) {
                        DeepScreen::New(n) => new_batch.push(*n),
                        DeepScreen::Old(o) => old_batch.push(o),
                        DeepScreen::Skip => {}
                    }
                }
                pages_got.fetch_add(1, Ordering::Relaxed);
                // Old before New so carried-over dedup registers first; order within
                // a sweep is immaterial to decisions (per-BIN evals are independent;
                // dominance/lbin run once on the complete byKey).
                if !old_batch.is_empty() {
                    let _ = tx.send(DeepMsg::Old(old_batch));
                }
                if !new_batch.is_empty() {
                    let _ = tx.send(DeepMsg::New(new_batch));
                }
            }
        });
    }
    // `tx` dropped here; `rx` ends when the last worker thread's clone drops.
    (rx, pages_got)
}

/// Fetch ONE player's auctions via `/skyblock/auction?player=<uuid>` (needs an API
/// key) and return only the still-buyable BINs, mapped into `RawAuction` so they
/// decode/price through the exact same pipeline as a page-sweep listing. This is
/// the seller-follow (liquidation catcher) fetch: seller-scoped and immediate,
/// unlike the page sweep which only sees a seller's items once they bubble up
/// across pages. `None` on transport/parse error or `success:false`.
///
/// "Still buyable" = a BIN whose window hasn't ended and that nobody has bought
/// yet (`highest_bid_amount == 0`, not `claimed`); anything else the seller can no
/// longer sell us, so it would just be a dead flip.
pub fn fetch_player_auctions(
    player_uuid: &str,
    api_key: &str,
    now_ms: i64,
) -> Option<Vec<RawAuction>> {
    let url = format!("{}{player_uuid}", auction_by_player_url());
    let http = match side_channel_client()
        .get(&url)
        .header("API-Key", api_key)
        .timeout(Duration::from_secs(6))
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("seller-follow: fetch {player_uuid} failed: {e}");
            return None;
        }
    };
    // The status separates 401 (key never arrived) from 429 (over budget) from a
    // 200 carrying an error body. It is gone once the body is consumed, so take
    // it before reading it.
    let status = http.status().to_string();
    let body = match http.text() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("seller-follow: fetch {player_uuid} http {status} body unreadable: {e}");
            return None;
        }
    };
    parse_player_auctions(&body, player_uuid, now_ms, &status)
}

/// Parse + buyable-filter half of [`fetch_player_auctions`], split out from the
/// HTTP so the wire field names are covered by a test rather than by hope. The
/// naming here was silently wrong for weeks (see the comment inside); a pure
/// function is the only way to pin it.
fn parse_player_auctions(
    body: &str,
    player_uuid: &str,
    now_ms: i64,
    status: &str,
) -> Option<Vec<RawAuction>> {
    // ⚠️ NO `rename_all = "camelCase"` here. The `?player=` endpoint (Hypixel
    // and NetherAPI alike) returns SNAKE_case: `starting_bid`, `item_bytes`,
    // `highest_bid_amount`. Because every field below is `#[serde(default)]`, a
    // camelCase rename does not error — it silently yields 0.0 / "" for exactly
    // those three, so the buyable filter's `starting_bid <= 0.0 ||
    // item_bytes.is_empty()` clause rejected EVERY auction as "not-a-live-BIN".
    // That made seller-follow return 0 buyable BINs on 3268/3268 lookups.
    // ⚠️ SECOND wire trap, hidden BEHIND the first one: on `?player=`,
    // `item_bytes` is an OBJECT `{"type":0,"data":"H4sI..."}`, not the bare
    // base64 string the paginated dump sends. While the camelCase rename was in
    // place serde never looked at the field, so fixing only the name turned a
    // silent zero into a hard `invalid type: map, expected a string` that failed
    // the WHOLE response. Accept both shapes.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ItemBytes {
        Plain(String),
        Wrapped { data: String },
    }
    impl Default for ItemBytes {
        fn default() -> Self {
            ItemBytes::Plain(String::new())
        }
    }
    impl ItemBytes {
        fn as_str(&self) -> &str {
            match self {
                ItemBytes::Plain(s) => s,
                ItemBytes::Wrapped { data } => data,
            }
        }
        fn into_data(self) -> String {
            match self {
                ItemBytes::Plain(s) => s,
                ItemBytes::Wrapped { data } => data,
            }
        }
    }
    #[derive(Deserialize)]
    struct PlayerAuc {
        #[serde(default)]
        uuid: String,
        #[serde(default)]
        auctioneer: Option<String>,
        #[serde(default)]
        starting_bid: f64,
        #[serde(default)]
        item_name: String,
        #[serde(default)]
        bin: bool,
        #[serde(default)]
        item_bytes: ItemBytes,
        #[serde(default)]
        start: f64,
        #[serde(default)]
        end: f64,
        #[serde(default)]
        claimed: bool,
        #[serde(default)]
        highest_bid_amount: f64,
    }
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        success: bool,
        #[serde(default)]
        auctions: Vec<PlayerAuc>,
        /// Hypixel names the failure `cause`, NetherAPI names it `error`. Read
        /// BOTH: reading only `cause` collapsed every NetherAPI failure mode
        /// (missing key, bad key, 429 throttle) into one indistinguishable
        /// `success=false (None)` line, which hid a 100% failure rate for days.
        #[serde(default)]
        cause: Option<String>,
        #[serde(default)]
        error: Option<String>,
        /// NetherAPI sets this on a rate-limit rejection.
        #[serde(default)]
        throttle: bool,
    }
    let resp: Resp = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("seller-follow: fetch {player_uuid} http {status} body was not JSON: {e}");
            return None;
        }
    };
    if !resp.success {
        let why = resp
            .error
            .as_deref()
            .or(resp.cause.as_deref())
            .unwrap_or("no reason given");
        eprintln!(
            "seller-follow: {player_uuid} FAILED http {status} reason={why}{}",
            if resp.throttle {
                " [throttled: over the key's rate budget]"
            } else {
                ""
            }
        );
        return None;
    }
    let now = now_ms as f64;
    let returned = resp.auctions.len();
    // Count each rejection separately. A silent `Some(vec![])` cannot tell
    // "this seller has nothing listed" apart from "they have 30 listings and our
    // buyable filter threw them all away" — opposite conclusions (nothing to
    // catch vs. a filter bug), and the difference was invisible in the log.
    let (mut not_bin, mut already_bid, mut claimed_or_over) = (0usize, 0usize, 0usize);
    let buyable: Vec<RawAuction> = resp
        .auctions
        .into_iter()
        .filter(|a| {
            if !a.bin || a.starting_bid <= 0.0 || a.item_bytes.as_str().is_empty() {
                not_bin += 1;
                return false;
            }
            if a.highest_bid_amount > 0.0 {
                already_bid += 1;
                return false;
            }
            if a.claimed || a.end <= now {
                claimed_or_over += 1;
                return false;
            }
            true
        })
        .map(|a| RawAuction {
            uuid: a.uuid,
            auctioneer: a.auctioneer.or_else(|| Some(player_uuid.to_string())),
            starting_bid: a.starting_bid,
            item_name: a.item_name,
            bin: a.bin,
            item_bytes: a.item_bytes.into_data(),
            start: a.start,
        })
        .collect();
    eprintln!(
        "seller-follow: {player_uuid} OK http {status} — {returned} listing(s), {} still-buyable \
         (skipped: {not_bin} not-a-live-BIN, {already_bid} already-bid, {claimed_or_over} claimed/ended)",
        buyable.len()
    );
    Some(buyable)
}

/// Resolve a Minecraft name → dashless uuid via Mojang (free, no key). Cached
/// forever: a name→uuid never changes for an existing account, so the seller
/// lookup only ever pays Mojang once per distinct seller. Returns the lowercase
/// dashless uuid that Hypixel/NetherAPI's `?player=` expects.
pub fn resolve_player_uuid(name: &str) -> Option<String> {
    static CACHE: OnceLock<std::sync::Mutex<HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let key = name.trim().to_lowercase();
    if key.is_empty() {
        return None;
    }
    if let Some(u) = cache.lock().unwrap().get(&key) {
        return Some(u.clone());
    }
    #[derive(Deserialize)]
    struct MojangProfile {
        #[serde(default)]
        id: String,
    }
    let url = format!(
        "https://api.mojang.com/users/profiles/minecraft/{}",
        name.trim()
    );
    let prof: MojangProfile = client()
        .get(&url)
        .timeout(Duration::from_secs(6))
        .send()
        .and_then(|r| r.json())
        .ok()?;
    if prof.id.is_empty() {
        return None;
    }
    let uuid = prof.id.to_lowercase();
    cache.lock().unwrap().insert(key, uuid.clone());
    Some(uuid)
}

#[cfg(test)]
mod player_auction_parse {
    //! Pins the WIRE FIELD NAMES of `/skyblock/auction?player=`. This is not a
    //! style test: `PlayerAuc` carried `rename_all = "camelCase"` while the API
    //! speaks snake_case, and because every field is `#[serde(default)]` serde
    //! reported no error — it just handed back `starting_bid: 0.0` and
    //! `item_bytes: ""`, which the buyable filter then rejected as
    //! "not-a-live-BIN". Seller-follow returned 0 buyable BINs on 3268 of 3268
    //! prod lookups and looked like "sellers never relist" instead of a bug.
    use super::*;

    /// Trimmed from a real NetherAPI response, field names and types verbatim.
    /// Two shapes are load-bearing here:
    ///   * `bin` is OMITTED on non-BIN auctions rather than sent as `false`;
    ///   * `item_bytes` is an OBJECT `{"type":0,"data":...}`, not a string;
    ///   * the numerics arrive as INTs, not floats.
    const BODY: &str = r#"{"success":true,"auctions":[
      {"uuid":"aaa","auctioneer":"seller1","starting_bid":42,"item_name":"Jaded Glossy Mineral Leggings",
       "bin":true,"item_bytes":{"type":0,"data":"H4sIAAA"},"start":1,"end":9000000000000,"claimed":false,"highest_bid_amount":0},
      {"uuid":"bbb","auctioneer":"seller1","starting_bid":1000,"item_name":"Bid Auction",
       "item_bytes":{"type":0,"data":"H4sIAAA"},"start":1,"end":9000000000000,"claimed":false,"highest_bid_amount":0},
      {"uuid":"ccc","auctioneer":"seller1","starting_bid":500,"item_name":"Already Bid",
       "bin":true,"item_bytes":{"type":0,"data":"H4sIAAA"},"start":1,"end":9000000000000,"claimed":false,"highest_bid_amount":700},
      {"uuid":"ddd","auctioneer":"seller1","starting_bid":500,"item_name":"Expired",
       "bin":true,"item_bytes":{"type":0,"data":"H4sIAAA"},"start":1,"end":1,"claimed":false,"highest_bid_amount":0}
    ]}"#;

    /// The paginated dump sends `item_bytes` as a bare base64 string. Both
    /// shapes must work through the same parse.
    #[test]
    fn item_bytes_accepts_the_bare_string_shape_too() {
        let body = r#"{"success":true,"auctions":[
          {"uuid":"aaa","auctioneer":"seller1","starting_bid":42,"item_name":"X","bin":true,
           "item_bytes":"H4sIAAA","start":1,"end":9000000000000,"claimed":false,"highest_bid_amount":0}]}"#;
        let got = parse_player_auctions(body, "seller1", 1_000_000, "200 OK").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].item_bytes, "H4sIAAA");
    }

    #[test]
    fn snake_case_body_yields_the_live_bin() {
        let got = parse_player_auctions(BODY, "seller1", 1_000_000, "200 OK")
            .expect("success:true must parse");
        // Exactly one of the four is still buyable: bbb is not a BIN, ccc has a
        // bid, ddd has ended.
        assert_eq!(got.len(), 1, "expected only the live BIN, got {got:?}");
        let a = &got[0];
        assert_eq!(a.uuid, "aaa");
        // The three fields the camelCase rename silently zeroed. If this
        // regresses, seller-follow goes back to reporting 0 still-buyable.
        assert_eq!(a.starting_bid, 42.0, "starting_bid must survive the parse");
        // Unwrapped from `{"type":0,"data":...}`. If this regresses to "" the
        // buyable filter drops every auction as "not-a-live-BIN".
        assert_eq!(a.item_bytes, "H4sIAAA", "item_bytes must survive the parse");
        assert!(a.bin);
    }

    #[test]
    fn missing_auctioneer_falls_back_to_the_queried_player() {
        let body = r#"{"success":true,"auctions":[
          {"uuid":"aaa","starting_bid":42,"item_name":"X","bin":true,
           "item_bytes":{"type":0,"data":"H4sIAAA"},"start":1,"end":9000000000000,"claimed":false,"highest_bid_amount":0}]}"#;
        let got = parse_player_auctions(body, "seller1", 1_000_000, "200 OK").unwrap();
        assert_eq!(got[0].auctioneer.as_deref(), Some("seller1"));
    }

    #[test]
    fn failure_body_returns_none() {
        let body = r#"{"success":false,"error":"Invalid API key","throttle":false}"#;
        assert!(parse_player_auctions(body, "seller1", 1_000_000, "403 Forbidden").is_none());
    }
}

#[cfg(test)]
mod nether_smoke {
    //! Live smoke test of the RUST seller-lookup path (Mojang name→uuid +
    //! NetherAPI per-player fetch through `SELLER_LOOKUP_BASE`). Ignored by
    //! default; run it explicitly:
    //!   NETHER_SMOKE=1 SELLER_LOOKUP_BASE=https://netherapi.com/api \
    //!   API_KEY=nether_... cargo test -p finder-rs nether_live -- --ignored --nocapture
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    #[ignore]
    fn nether_live() {
        if std::env::var("NETHER_SMOKE").as_deref() != Ok("1") {
            eprintln!("NETHER_SMOKE!=1, skipping");
            return;
        }
        let key = std::env::var("API_KEY").expect("API_KEY unset");
        // `SMOKE_SELLER_UUID` skips Mojang and pins a seller KNOWN to have stock.
        // Needed to prove the buyable filter: a seller with 0 listings exercises
        // nothing, and prod's followed sellers are usually empty by the time we
        // look ([[seller-follow]] triggers after they sold the piece we found).
        let (name, uuid) = match std::env::var("SMOKE_SELLER_UUID") {
            Ok(u) if !u.trim().is_empty() => (u.trim().to_string(), u.trim().to_string()),
            _ => {
                let name =
                    std::env::var("SMOKE_SELLER").unwrap_or_else(|_| "M0nkeyDRuffy".to_string());
                let uuid = resolve_player_uuid(&name).expect("Mojang name→uuid failed");
                eprintln!("resolve_player_uuid({name}) = {uuid}");
                (name, uuid)
            }
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let aucs = fetch_player_auctions(&uuid, &key, now)
            .expect("fetch_player_auctions returned None (transport/parse/auth failure)");
        eprintln!("fetched {} still-buyable BIN(s) for {name}", aucs.len());
        for a in aucs.iter().take(8) {
            eprintln!(
                "  bin={} price={} uuid={} name={}",
                a.bin, a.starting_bid, a.uuid, a.item_name
            );
        }
        // Success = the call authenticated + parsed. Zero buyable BINs is valid.
    }
}

#[cfg(test)]
mod display_name_tests {
    //! The bazaar bot TYPES this string into the search GUI, so any markup left
    //! in it is an order that can never be placed. Cases are real names from
    //! `resources/skyblock/items`.
    use super::clean_display_name;

    #[test]
    fn strips_percent_colour_markers() {
        // The one that actually reached the tradeable list.
        assert_eq!(clean_display_name("%%red%%Volcanic Rock"), "Volcanic Rock");
        assert_eq!(
            clean_display_name("%%green%%Elle's Supplies"),
            "Elle's Supplies"
        );
        assert_eq!(
            clean_display_name("%%yellow%%Wizard's Breadcrumbs"),
            "Wizard's Breadcrumbs"
        );
    }

    #[test]
    fn strips_a_marker_in_the_middle_too() {
        assert_eq!(
            clean_display_name("%%green%%Axe Preview %%gray%%(Right-Click)"),
            "Axe Preview (Right-Click)"
        );
    }

    #[test]
    fn strips_section_codes() {
        assert_eq!(clean_display_name("§5§oWither Skull"), "Wither Skull");
    }

    #[test]
    fn leaves_ordinary_names_alone() {
        // The overwhelming majority: must survive byte-for-byte.
        assert_eq!(
            clean_display_name("Enchanted Birch Log"),
            "Enchanted Birch Log"
        );
        assert_eq!(clean_display_name("Sublime Silk"), "Sublime Silk");
        // A lone '%' is not markup and must not eat the rest of the name.
        assert_eq!(clean_display_name("100% Cocoa"), "100% Cocoa");
    }
}

#[cfg(test)]
mod deep_screen_tests {
    //! Unit coverage for the deep-page streaming screen (`screen_deep_auction`),
    //! which the fetch workers apply per auction. These pin that the streaming
    //! path makes the SAME new/old/skip verdict the old blocking deep loop made,
    //! so DEEP_STREAM=1 and =0 stay decision-identical.
    use super::*;
    use std::collections::HashSet;

    /// A real, decodable BIN from the page-0 fixture, so the New branch runs
    /// end-to-end through the actual NBT decoder rather than a hand-rolled blob.
    fn a_real_bin() -> RawAuction {
        let data = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/fixture-page0.json"))
            .expect("fixture-page0.json present");
        let v: serde_json::Value = serde_json::from_slice(&data).unwrap();
        let a = v["auctions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| {
                a["bin"].as_bool().unwrap_or(false)
                    && !a["item_bytes"].as_str().unwrap_or("").is_empty()
                    && a["starting_bid"].as_f64().unwrap_or(0.0) > 0.0
            })
            .expect("fixture has a buyable BIN");
        serde_json::from_value(a.clone()).unwrap()
    }

    #[test]
    fn new_bin_decodes_and_carries_fields() {
        let a = a_real_bin();
        let (uuid, bid, start) = (a.uuid.clone(), a.starting_bid, a.start);
        match screen_deep_auction(a, true, &HashSet::new()) {
            DeepScreen::New(n) => {
                assert_eq!(n.uuid, uuid);
                assert_eq!(n.starting_bid, bid);
                assert_eq!(n.start, start);
                assert!(!n.attrs.id.is_empty(), "decoded item id present");
            }
            _ => panic!("a fresh, decodable BIN must screen as New"),
        }
    }

    #[test]
    fn carried_over_is_old_and_skips_decode() {
        let a = a_real_bin();
        let (uuid, bid) = (a.uuid.clone(), a.starting_bid);
        let mut live = HashSet::new();
        live.insert(uuid.clone());
        match screen_deep_auction(a, true, &live) {
            DeepScreen::Old((u, b)) => {
                assert_eq!(u, uuid);
                assert_eq!(b, bid);
            }
            _ => panic!("a BIN in the prev-live set must screen as Old"),
        }
    }

    #[test]
    fn unprimed_treats_everything_as_new() {
        // Priming sweep (primed=false): even a uuid in the (stale/empty) live set
        // must decode as new, matching the old loop's `*primed && contains` gate.
        let a = a_real_bin();
        let mut live = HashSet::new();
        live.insert(a.uuid.clone());
        assert!(matches!(
            screen_deep_auction(a, false, &live),
            DeepScreen::New(_)
        ));
    }

    #[test]
    fn non_bin_and_invalid_are_skipped() {
        let base = a_real_bin();
        let live = HashSet::new();

        let mut not_bin = base.clone();
        not_bin.bin = false;
        assert!(matches!(
            screen_deep_auction(not_bin, true, &live),
            DeepScreen::Skip
        ));

        let mut zero_bid = base.clone();
        zero_bid.starting_bid = 0.0;
        assert!(matches!(
            screen_deep_auction(zero_bid, true, &live),
            DeepScreen::Skip
        ));

        let mut empty_bytes = base.clone();
        empty_bytes.item_bytes = String::new();
        assert!(matches!(
            screen_deep_auction(empty_bytes, true, &live),
            DeepScreen::Skip
        ));

        let mut junk_bytes = base;
        junk_bytes.item_bytes = "not-valid-base64-nbt!!".to_string();
        assert!(matches!(
            screen_deep_auction(junk_bytes, true, &live),
            DeepScreen::Skip
        ));
    }

    #[test]
    fn carried_over_with_zero_bid_still_old() {
        // The carried-over check runs BEFORE the validity gate, so a carried BIN
        // with a zero bid still rolls into old_bins — byte-for-byte the old loop.
        let mut a = a_real_bin();
        a.starting_bid = 0.0;
        let mut live = HashSet::new();
        live.insert(a.uuid.clone());
        assert!(matches!(
            screen_deep_auction(a, true, &live),
            DeepScreen::Old(_)
        ));
    }
}

/// One recently-ended (sold) auction. `timestamp` is unix ms.
#[derive(Deserialize, Debug, Clone)]
pub struct EndedAuction {
    pub auction_id: String,
    #[serde(default)]
    pub item_bytes: String,
    #[serde(default)]
    pub price: f64,
    #[serde(default)]
    pub bin: bool,
    #[serde(default)]
    pub timestamp: f64,
    #[serde(default)]
    seller: Option<String>,
    #[serde(default)]
    seller_profile: Option<String>,
    /// WHO bought it. The only field that can tell our own purchases from COFL
    /// users' from a rival custom finder's, and it was being discarded.
    #[serde(default)]
    buyer: Option<String>,
    #[serde(default)]
    buyer_profile: Option<String>,
}
impl EndedAuction {
    /// seller, falling back to seller_profile then "" (matches hypixel.ts).
    pub fn seller(&self) -> String {
        self.seller
            .clone()
            .or_else(|| self.seller_profile.clone())
            .unwrap_or_default()
    }
    pub fn buyer(&self) -> String {
        self.buyer
            .clone()
            .or_else(|| self.buyer_profile.clone())
            .unwrap_or_default()
    }
}

/// `fetchEnded()` — the ~last-60s of BIN sales (the reference feed).
pub fn fetch_ended() -> Vec<EndedAuction> {
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        auctions: Vec<EndedAuction>,
    }
    match client()
        .get(ENDED_URL)
        .send()
        .and_then(|r| r.json::<Resp>())
    {
        Ok(r) => r.auctions,
        Err(e) => {
            eprintln!("ended fetch failed: {e}");
            Vec::new()
        }
    }
}
