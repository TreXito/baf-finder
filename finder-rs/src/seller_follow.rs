//! Seller-follow (liquidation catcher): when a flip clears the trigger profit,
//! its seller is watched for a window and their WHOLE auction house is pulled via
//! `/skyblock/auction?player=` on a tight cadence. When someone lists off all
//! their gear at once, the page sweep catches the first item and this vacuums the
//! rest of their underpriced listings within seconds, before the deep-page sweep
//! gets to them.
//!
//! Split of labour: this background worker does ONLY the slow half (the API
//! fetch), so it never sits in front of the flip that triggered it. It hands the
//! seller's still-buyable BINs back to the serve loop, which decodes + prices +
//! pushes them through the exact same path as a page-sweep flip (the loop thread
//! owns the price index, so eval must happen there).
//!
//! Fully dormant unless `sellerFollow` is set in ws-config AND `API_KEY` is in the
//! env: the player-auction endpoint needs a key. Missing key ⇒ logged once, inert.

use crate::hypixel::{self, RawAuction};
use crate::ws_server::WsShared;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Wakes the worker at least this often to run a poll pass, even when no new
/// seller arrives (so watches keep re-polling on cadence).
const TICK: Duration = Duration::from_millis(300);
/// Minimum gap between two player-auction requests, so a burst of watched sellers
/// can never hammer the API key's rate budget however many are due at once.
const REQUEST_SPACING: Duration = Duration::from_millis(300);

/// Handles returned to the serve loop: it `follow_tx.send(seller)` on a big flip
/// and drains `auc_rx` in its maintenance gap to evaluate the seller's listings.
pub struct FollowHandle {
    pub follow_tx: Sender<String>,
    pub auc_rx: Receiver<SellerBatch>,
}

/// One seller's freshly-seen, still-buyable BINs (only auctions not sent before
/// for this watch, so the loop never re-evaluates the same listing).
pub struct SellerBatch {
    pub seller: String,
    pub auctions: Vec<RawAuction>,
}

struct Watch {
    /// Stop re-polling after this (ms); refreshed each time the seller re-triggers.
    until_ms: i64,
    /// Next time this seller is due for a poll (ms).
    next_poll_ms: i64,
    /// Auction uuids already handed to the loop, so re-polls only send new ones.
    seen: HashSet<String>,
    /// When the watch opened (ms), for the close-out line.
    opened_ms: i64,
    /// How many times the seller re-tripped the trigger while being watched.
    triggers: u32,
    /// Poll passes actually issued for this seller.
    polls: u32,
    /// Listings handed to the serve loop across the whole watch.
    pushed: u32,
}

/// Spawn the worker thread. Returns immediately; the thread lives for the process.
pub fn spawn(shared: Arc<WsShared>) -> FollowHandle {
    let (follow_tx, follow_rx) = channel::<String>();
    let (auc_tx, auc_rx) = channel::<SellerBatch>();
    // Own key var first, so seller-follow can be keyed WITHOUT also making the
    // detect lanes send `API_KEY` on their page-0 polls; falls back to `API_KEY`.
    let api_key = std::env::var("SELLER_FOLLOW_API_KEY")
        .or_else(|_| std::env::var("API_KEY"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    std::thread::spawn(move || run_worker(shared, api_key, follow_rx, auc_tx));
    FollowHandle { follow_tx, auc_rx }
}

fn run_worker(
    shared: Arc<WsShared>,
    api_key: Option<String>,
    follow_rx: Receiver<String>,
    auc_tx: Sender<SellerBatch>,
) {
    let mut watch: HashMap<String, Watch> = HashMap::new();
    let mut warned_no_key = false;
    let mut totals = Totals::default();
    eprintln!(
        "seller-follow: worker started (api key {})",
        if api_key.is_some() {
            "present"
        } else {
            "MISSING — feature inert"
        }
    );
    loop {
        // Block until a seller arrives OR the tick elapses (so cadence re-polls
        // still fire when idle). A new seller wakes us immediately = "quick".
        match follow_rx.recv_timeout(TICK) {
            Ok(seller) => {
                add_or_refresh(
                    &shared,
                    &api_key,
                    &mut warned_no_key,
                    &mut watch,
                    seller,
                    &mut totals,
                );
                // Drain any others queued in the same instant without blocking.
                while let Ok(s) = follow_rx.try_recv() {
                    add_or_refresh(
                        &shared,
                        &api_key,
                        &mut warned_no_key,
                        &mut watch,
                        s,
                        &mut totals,
                    );
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return, // serve loop gone
        }

        let (enabled, poll_ms) = {
            let cfg = shared.filters.read().unwrap();
            (
                cfg.seller_follow,
                (cfg.seller_follow_poll_secs.max(1.0) * 1000.0) as i64,
            )
        };
        let now = now_ms();
        // Close out expired watches with a verdict. A watch that ends having
        // pushed nothing is the normal-but-invisible outcome: without this line
        // a fruitless night and a night where the feature never ran at all read
        // EXACTLY the same in the log.
        watch.retain(|seller, w| {
            if w.until_ms > now {
                return true;
            }
            totals.windows_closed += 1;
            if w.pushed == 0 {
                totals.windows_empty += 1;
            }
            eprintln!(
                "seller-follow: {seller} window closed after {}s — {} trigger(s), {} poll(s), \
                 {} distinct listing(s) seen, {} pushed to price",
                (now - w.opened_ms) / 1000,
                w.triggers,
                w.polls,
                w.seen.len(),
                w.pushed,
            );
            false
        });
        let Some(key) = api_key.as_deref() else {
            continue;
        };
        if !enabled {
            continue;
        }

        // Poll every due seller. Space requests so a full watch list can't burst.
        let due: Vec<String> = watch
            .iter()
            .filter(|(_, w)| w.next_poll_ms <= now)
            .map(|(s, _)| s.clone())
            .collect();
        for seller in due {
            let fetched = hypixel::fetch_player_auctions(&seller, key, now_ms());
            totals.polls += 1;
            if fetched.is_none() {
                totals.poll_failures += 1;
            }
            if let Some(w) = watch.get_mut(&seller) {
                w.next_poll_ms = now_ms() + poll_ms;
                w.polls += 1;
                if let Some(auctions) = fetched {
                    let returned = auctions.len();
                    let fresh: Vec<RawAuction> = auctions
                        .into_iter()
                        .filter(|a| w.seen.insert(a.uuid.clone()))
                        .collect();
                    if !fresh.is_empty() {
                        eprintln!(
                            "seller-follow: {seller} +{} new listing(s) to price",
                            fresh.len()
                        );
                        w.pushed += fresh.len() as u32;
                        totals.pushed += fresh.len() as u64;
                        let _ = auc_tx.send(SellerBatch {
                            seller: seller.clone(),
                            auctions: fresh,
                        });
                    } else if returned == 0 {
                        // The two most common outcomes, both previously silent.
                        // They mean opposite things, so never merge them.
                        eprintln!(
                            "seller-follow: {seller} nothing to price — no still-buyable BINs left"
                        );
                    } else {
                        eprintln!(
                            "seller-follow: {seller} nothing new — all {returned} buyable listing(s) already seen this watch"
                        );
                    }
                }
            }
            std::thread::sleep(REQUEST_SPACING);
        }

        totals.report_if_due();
    }
}

/// Process-lifetime counters, printed on a slow cadence so "is this feature
/// doing anything at all?" is answerable from a single grep instead of by
/// reconstructing it from thousands of per-seller lines.
#[derive(Default)]
struct Totals {
    triggers: u64,
    triggers_at_cap: u64,
    watches_opened: u64,
    windows_closed: u64,
    windows_empty: u64,
    polls: u64,
    poll_failures: u64,
    pushed: u64,
    last_report_ms: i64,
}

/// How often the rollup line prints (ms).
const REPORT_EVERY_MS: i64 = 15 * 60 * 1000;

impl Totals {
    fn report_if_due(&mut self) {
        let now = now_ms();
        if self.last_report_ms == 0 {
            self.last_report_ms = now;
            return;
        }
        if now - self.last_report_ms < REPORT_EVERY_MS {
            return;
        }
        self.last_report_ms = now;
        eprintln!(
            "seller-follow SUMMARY: {} trigger(s) ({} dropped at the max-sellers cap), \
             {} watch(es) opened, {} closed ({} found nothing), {} poll(s) ({} failed), \
             {} listing(s) pushed to price",
            self.triggers,
            self.triggers_at_cap,
            self.watches_opened,
            self.windows_closed,
            self.windows_empty,
            self.polls,
            self.poll_failures,
            self.pushed,
        );
    }
}

/// Add a new watched seller or extend an existing watch's window. Enforces the
/// enabled flag, key presence, and the max-sellers cap (evicting nothing: new
/// sellers past the cap are skipped until a watch expires, bounding API usage).
fn add_or_refresh(
    shared: &Arc<WsShared>,
    api_key: &Option<String>,
    warned_no_key: &mut bool,
    watch: &mut HashMap<String, Watch>,
    seller: String,
    totals: &mut Totals,
) {
    let (enabled, window_ms, max_sellers) = {
        let cfg = shared.filters.read().unwrap();
        (
            cfg.seller_follow,
            (cfg.seller_follow_window_secs.max(1.0) * 1000.0) as i64,
            cfg.seller_follow_max_sellers.max(1.0) as usize,
        )
    };
    totals.triggers += 1;
    if !enabled {
        return;
    }
    if api_key.is_none() {
        if !*warned_no_key {
            eprintln!("seller-follow: enabled but API_KEY is unset: feature inert (set API_KEY to follow sellers)");
            *warned_no_key = true;
        }
        return;
    }
    let now = now_ms();
    if let Some(w) = watch.get_mut(&seller) {
        w.until_ms = now + window_ms; // keep watching while they keep dumping
        w.triggers += 1;
        eprintln!(
            "seller-follow: {seller} re-triggered (trigger #{}) — window extended to {}s",
            w.triggers,
            window_ms / 1000
        );
        return;
    }
    // Purge expired before deciding the cap is really hit.
    watch.retain(|_, w| w.until_ms > now);
    if watch.len() >= max_sellers {
        // Silently skipping here meant a saturated watch list looked identical
        // to a quiet market. Say it, so the cap shows up as a tuning knob.
        totals.triggers_at_cap += 1;
        eprintln!(
            "seller-follow: {seller} NOT followed — already watching {max_sellers} seller(s) \
             (raise sellerFollowMaxSellers or shorten sellerFollowWindowSecs)"
        );
        return;
    }
    totals.watches_opened += 1;
    eprintln!(
        "seller-follow: now following {seller} for {}s ({} seller(s) watched)",
        window_ms / 1000,
        watch.len() + 1
    );
    watch.insert(
        seller,
        Watch {
            until_ms: now + window_ms,
            next_poll_ms: 0,
            seen: HashSet::new(),
            opened_ms: now,
            triggers: 1,
            polls: 0,
            pushed: 0,
        },
    );
}
