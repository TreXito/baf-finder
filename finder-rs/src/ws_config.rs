//! Port of the ws-config half of `wsServer.ts` — the hot-reloaded filter config.
//! Field names/defaults match `WsFilterConfig`/`DEFAULTS` exactly (camelCase on
//! disk). Missing keys fall back to defaults; unknown keys are dropped on the
//! next normalize-rewrite, exactly like the TS loader.

use finder_core::filter::{BinMasterFilter, Filter};
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WsFilterConfig {
    pub token: String,
    pub hard_min_profit: f64,
    pub min_profit: f64,
    pub min_roi_pct: f64,
    pub min_confidence: f64,
    pub min_volume_per_day: f64,
    pub min_profit_per_hour: f64,
    pub allow_lbin: bool,
    pub blocked_guards: Vec<String>,
    pub blacklist_ids: Vec<String>,
    pub assign_mode: String, // "single" | "all"
    /// Grind flips round-robin one small account at a time; "all" = old broadcast.
    /// Separate from assign_mode: broadcasting a grind flip makes every small
    /// account race for one item (wsServer.ts:155, shipped to prod TS 2026-07-15).
    #[serde(rename = "grindAssignMode", default = "default_grind_assign_mode")]
    pub grind_assign_mode: String,
    pub listing_recommendations: bool,
    pub max_spend_fraction: f64,
    pub max_purse: f64,
    pub grind_min_volume_per_day: f64,
    pub grind_max_volatility: f64,
    pub grind_min_confidence: f64,
    pub grind_min_profit: f64,
    pub min_free_inv_slots: f64,
    pub max_active_auctions: f64,
    /// Per-bot congestion throttle: when a bot's free inventory slots drop to
    /// `full_inv_free_threshold` or below (it can't list its buys fast enough, so
    /// items pile up), that bot only stays eligible for flips at/above
    /// `full_min_profit`. 0 = off. Applies to any account. Because eligibility is
    /// filtered before the assign broadcast, cheap flips fall to bots with room.
    pub full_min_profit: f64,
    /// Proxy trigger for bots that don't report the exact fields yet: congested when
    /// free inventory slots ≤ this.
    pub full_inv_free_threshold: f64,
    /// Exact trigger (when the bot reports `auctionAtLimit`+`invUsed`): congested when
    /// AH is at its slot cap AND at least this many inventory slots are used.
    pub full_inv_used_at_least: f64,
    /// Purse-scaled floor for NON-grind (big-purse) accounts: their min profit
    /// becomes `max(hardMinProfit, purse * min_profit_purse_fraction)`, so a whale
    /// never wastes a slot on a flip that's tiny relative to its purse (e.g. 0.01
    /// ⇒ a 1B purse demands 10M+). 0 = off. Grind accounts (purse ≤ maxPurse) are
    /// never affected, keeping their small-flip grind intact.
    pub min_profit_purse_fraction: f64,
    /// Seller-follow (liquidation catcher): when a flip clears the trigger profit,
    /// the seller is watched and their WHOLE auction house is pulled via the
    /// `/skyblock/auction?player=` API (needs `API_KEY`), so when someone lists off
    /// all their gear at once we vacuum the rest of their underpriced listings
    /// within seconds instead of waiting for the page sweep. Each of the seller's
    /// items runs through the exact same pricing/push path as a normal flip. Off by
    /// default; the fetch runs on a background worker so it never delays the flip
    /// that triggered it. Requires `API_KEY` set or it stays inert.
    pub seller_follow: bool,
    /// Min profit for a flip to START following its seller. 0 ⇒ use hardMinProfit
    /// (the "big flip" bar). A follow never triggers when the effective bar is 0.
    pub seller_follow_min_profit: f64,
    /// How long (seconds) to keep re-polling a watched seller's auction house after
    /// their last qualifying flip. A liquidating player lists their gear over a
    /// minute or two, so this window catches the pieces that appear after the first.
    pub seller_follow_window_secs: f64,
    /// Re-poll cadence (seconds) for each watched seller. Keep it conservative: the
    /// player-auction endpoint counts against the API key's rate budget.
    pub seller_follow_poll_secs: f64,
    /// Safety cap on how many sellers are watched at once (bounds API usage). New
    /// sellers past the cap are skipped until a watch expires.
    pub seller_follow_max_sellers: f64,
    // Note: the pageflipper no longer has its own min-profit floor. A NetherAPI
    // seller lookup is spent only when a crawled flip actually passes the buyers'
    // filter (WsShared::passes_filter — hardMinProfit → BinMaster/ws-config), so
    // hardMinProfit is the real gate. A leftover `pageflipperMinProfit` key in an
    // existing ws-config is harmless (unknown keys are dropped on rewrite).
}

impl Default for WsFilterConfig {
    fn default() -> Self {
        WsFilterConfig {
            token: String::new(),
            hard_min_profit: 0.0,
            min_profit: 3_000_000.0,
            min_roi_pct: 10.0,
            min_confidence: 0.7,
            min_volume_per_day: 3.0,
            min_profit_per_hour: 0.0,
            allow_lbin: true,
            blocked_guards: vec!["manipulated".to_string()],
            blacklist_ids: vec![],
            assign_mode: "single".to_string(),
            grind_assign_mode: default_grind_assign_mode(),
            listing_recommendations: true,
            max_spend_fraction: 0.9,
            max_purse: 0.0,
            grind_min_volume_per_day: 40.0,
            grind_max_volatility: 0.1,
            grind_min_confidence: 0.8,
            grind_min_profit: 100_000.0,
            min_free_inv_slots: 3.0,
            max_active_auctions: 0.0,
            full_min_profit: 0.0,
            full_inv_free_threshold: 5.0,
            full_inv_used_at_least: 5.0,
            min_profit_purse_fraction: 0.0,
            seller_follow: false,
            seller_follow_min_profit: 0.0,
            seller_follow_window_secs: 180.0,
            seller_follow_poll_secs: 6.0,
            seller_follow_max_sellers: 8.0,
        }
    }
}

/// Loads + hot-reloads the ws-config file. Poll `reload_if_changed()` on a
/// cadence (mtime-gated) like the TS 5s poll + fs.watch.
pub struct WsConfigStore {
    path: String,
    mtime: Option<SystemTime>,
    pub filters: WsFilterConfig,
}

impl WsConfigStore {
    /// Load initially, creating the file with defaults if absent.
    pub fn load(path: &str) -> Self {
        let mut s = WsConfigStore {
            path: path.to_string(),
            mtime: None,
            filters: WsFilterConfig::default(),
        };
        if !std::path::Path::new(path).exists() {
            if let Some(parent) = std::path::Path::new(path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(
                path,
                serde_json::to_string_pretty(&WsFilterConfig::default()).unwrap(),
            );
        }
        s.read(true);
        s
    }

    /// Re-read if the file changed on disk. Returns true when filters updated.
    pub fn reload_if_changed(&mut self) -> bool {
        self.read(false)
    }

    fn read(&mut self, initial: bool) -> bool {
        let meta = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => return false,
        };
        let mtime = meta.modified().ok();
        if !initial && mtime == self.mtime {
            return false;
        }
        self.mtime = mtime;
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(_) => return false,
        };
        // Deserialize with per-field defaults (missing keys → default; unknown
        // keys ignored). Then re-serialize to normalize the file to the current
        // schema (strip legacy keys / add new ones), matching the TS rewrite.
        match serde_json::from_str::<WsFilterConfig>(&raw) {
            Ok(cfg) => {
                // DO NOT rewrite this file. TS only ever READS ws-config.json — it has
                // no writeFileSync for it — so rewriting is behavior the clone must not
                // add. Worse, serde_json round-tripping DROPS any key WsFilterConfig
                // does not declare: on 2026-07-15 this silently deleted the user's
                // `grindAssignMode` (shipped to prod TS that morning) and rewrote every
                // integer as a float (3000000 -> 3000000.0). Missing keys already fall
                // back to their #[serde(default)], which is exactly what TS does, so the
                // rewrite bought nothing and cost a live setting.
                let _ = &raw;
                self.filters = cfg;
                true
            }
            Err(_) => false, // keep previous filters on parse error
        }
    }
}

/// Loads + hot-reloads the BinMaster multi-tier filter file (port of the
/// `loadFilter`/`watchFilter` half of `filter.ts`). Absent file → the ws-config
/// single-threshold path is used instead (exactly like the TS loader). `poll`
/// yields the new filter ONLY when the file (dis)appeared or changed, so the
/// caller can `set_bin_filter` on a cadence without churn.
fn default_grind_assign_mode() -> String {
    "single".to_string()
}

pub struct BinFilterStore {
    path: String,
    mtime: Option<SystemTime>,
    loaded: bool,
}

impl BinFilterStore {
    pub fn new(path: &str) -> Self {
        BinFilterStore {
            path: path.to_string(),
            mtime: None,
            loaded: false,
        }
    }

    /// `Some(new filter option)` when the file changed (Some(filter) present /
    /// None removed); `None` when nothing changed. Parse errors keep the previous.
    pub fn poll(&mut self) -> Option<Option<Filter>> {
        let meta = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => {
                // Absent → no BinMaster filter (fall back to ws-config thresholds).
                if self.loaded {
                    self.loaded = false;
                    self.mtime = None;
                    return Some(None);
                }
                return None;
            }
        };
        let mtime = meta.modified().ok();
        if self.loaded && mtime == self.mtime {
            return None;
        }
        self.mtime = mtime;
        match std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str::<BinMasterFilter>(&s).ok())
        {
            Some(cfg) => {
                self.loaded = true;
                Some(Some(Filter::new(Some(cfg))))
            }
            // Parse error: keep the previous filter (mtime already advanced so we
            // don't spin on the same bad file).
            None => None,
        }
    }
}
