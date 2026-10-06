//! Port of costBasis.ts — purchase-price memory + self-listing guard.
//!
//! Behavioral clone: recorded when a flip is pushed (the pushed BIN price IS
//! the buy price), consulted when the same item shows up in the bot's
//! inventory for listing so the lister can floor the resale at break-even.
//! On-disk JSON shape is byte-compatible with the TS file (`{uuid:{paid,at}}`)
//! so a cross-port restart between buy and list keeps the floor.

use finder_core::config::SELF_BUY_GUARD_H;
use indexmap::IndexSet;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const TTL_MS: i64 = 14 * 24 * 3_600_000; // two weeks (covers slow relist cycles)
const MAX_ENTRIES: usize = 20_000;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[derive(Serialize, Deserialize, Clone, Copy)]
struct Entry {
    paid: f64,
    at: i64,
}

pub struct CostBasis {
    file: String,
    paid_by_uuid: HashMap<String, Entry>,
    // Self-listing guard: auction UUIDs (and per-item sets) the bot has listed,
    // so the finder never pushes a flip for the bot's OWN listing.
    own_listings: IndexSet<String>,
    own_listings_by_item: HashMap<String, HashSet<String>>,
    dirty: bool,
}

impl CostBasis {
    /// Load from `path` (COST_BASIS_PATH), pruning expired/invalid rows exactly
    /// like the TS loader: keep only `paid > 0 && at > now - TTL`.
    pub fn load(path: &str) -> Self {
        let mut paid_by_uuid = HashMap::new();
        if let Ok(s) = std::fs::read_to_string(path) {
            if let Ok(raw) = serde_json::from_str::<HashMap<String, Entry>>(&s) {
                let cutoff = now_ms() - TTL_MS;
                for (k, v) in raw {
                    if v.paid > 0.0 && v.at > cutoff {
                        paid_by_uuid.insert(k, v);
                    }
                }
            }
        }
        CostBasis {
            file: path.to_string(),
            paid_by_uuid,
            own_listings: IndexSet::new(),
            own_listings_by_item: HashMap::new(),
            dirty: false,
        }
    }

    pub fn entries(&self) -> usize {
        self.paid_by_uuid.len()
    }

    /// Remember what a pushed flip costs. No-op for empty uuid or non-positive
    /// paid (unbought flips leave a harmless orphan entry that TTLs out).
    #[allow(clippy::neg_cmp_op_on_partial_ord)] // !(paid>0) keeps NaN-paid a no-op, matching TS
    pub fn record_cost(&mut self, item_uuid: &str, paid: f64) {
        if item_uuid.is_empty() || !(paid > 0.0) {
            return;
        }
        self.paid_by_uuid
            .insert(item_uuid.to_string(), Entry { paid, at: now_ms() });
        self.dirty = true;
    }

    /// Purchase price for an inventory item, or None when never pushed.
    pub fn cost_for(&self, item_uuid: Option<&str>) -> Option<f64> {
        let u = item_uuid.filter(|s| !s.is_empty())?;
        self.paid_by_uuid.get(u).map(|e| e.paid)
    }

    /// Original purchase time (authoritative age for clearance pricing; survives restarts).
    pub fn cost_recorded_at_for(&self, item_uuid: Option<&str>) -> Option<i64> {
        let u = item_uuid.filter(|s| !s.is_empty())?;
        self.paid_by_uuid.get(u).map(|e| e.at)
    }

    /// Record that the bot listed an auction (so we never buy it back). When the
    /// auction UUID isn't known yet, track by item UUID; a later call updates it.
    pub fn record_own_listing(&mut self, auction_uuid: Option<&str>, item_uuid: Option<&str>) {
        if let Some(a) = auction_uuid.filter(|s| !s.is_empty()) {
            self.own_listings.insert(a.to_string());
        }
        if let Some(i) = item_uuid.filter(|s| !s.is_empty()) {
            let by_item = self.own_listings_by_item.entry(i.to_string()).or_default();
            if let Some(a) = auction_uuid.filter(|s| !s.is_empty()) {
                by_item.insert(a.to_string());
            }
        }
        // Coarse expiry past 5k auction uuids.
        //
        // ⚠️ This used to `clear()` BOTH maps outright, which does not bound
        // memory any better than evicting and does blind the self-buy guard
        // completely every time it fires. At ~2,300 buys/day plus listings we
        // cross 5,000 constantly, so the guard spent much of its life empty.
        // Drop the oldest quarter instead; insertion order is why `own_listings`
        // is an IndexSet.
        if self.own_listings.len() > 5_000 {
            let drop_n = self.own_listings.len() / 4;
            let evicted: Vec<String> = self.own_listings.iter().take(drop_n).cloned().collect();
            for a in &evicted {
                self.own_listings.shift_remove(a);
            }
            let evicted: HashSet<&String> = evicted.iter().collect();
            self.own_listings_by_item
                .retain(|_, auctions| !auctions.iter().all(|a| evicted.contains(a)));
        }
    }

    /// Is this auction one of our own listings? Checks by auction UUID and/or
    /// item UUID (fallback for when the bot doesn't know the listing UUID yet).
    pub fn is_own_listing(&self, auction_uuid: Option<&str>, item_uuid: Option<&str>) -> bool {
        if let Some(a) = auction_uuid.filter(|s| !s.is_empty()) {
            if self.own_listings.contains(a) {
                return true;
            }
        }
        if let Some(i) = item_uuid.filter(|s| !s.is_empty()) {
            if self.own_listings_by_item.contains_key(i) {
                return true;
            }
            // Both checks above need the listing to have been SEEN, which takes
            // up to ~80s (20s BIN grace + the 60s publish metronome). Our own
            // bots re-buy each other at +60s and +119s, so by then it is over.
            //
            // The cost basis knows we hold the item the instant we pay for it,
            // which is the earliest any part of the system can know. Bounded by
            // hours rather than forever, so an item we genuinely sell on becomes
            // buyable again instead of being blacklisted for the 14-day TTL.
            if *SELF_BUY_GUARD_H > 0.0 {
                if let Some(e) = self.paid_by_uuid.get(i) {
                    if e.at > now_ms() - (*SELF_BUY_GUARD_H * 3_600_000.0) as i64 {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Update a listing's auction UUID once it becomes known.
    pub fn update_listing_uuid(&mut self, auction_uuid: &str, item_uuid: Option<&str>) {
        if auction_uuid.is_empty() {
            return;
        }
        if let Some(i) = item_uuid.filter(|s| !s.is_empty()) {
            if let Some(by_item) = self.own_listings_by_item.get_mut(i) {
                by_item.insert(auction_uuid.to_string());
            }
        }
        self.own_listings.insert(auction_uuid.to_string());
    }

    /// Flush to disk if mutated since the last flush. Prunes expired + oldest
    /// over MAX_ENTRIES before writing (same order as the TS save timer). The
    /// orchestration loop calls this on a cadence instead of the TS 5s debounce.
    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        let cutoff = now_ms() - TTL_MS;
        self.paid_by_uuid.retain(|_, v| v.at >= cutoff);
        if self.paid_by_uuid.len() > MAX_ENTRIES {
            let mut sorted: Vec<(String, i64)> = self
                .paid_by_uuid
                .iter()
                .map(|(k, v)| (k.clone(), v.at))
                .collect();
            sorted.sort_by_key(|(_, at)| *at);
            let excess = self.paid_by_uuid.len() - MAX_ENTRIES;
            for (k, _) in sorted.into_iter().take(excess) {
                self.paid_by_uuid.remove(&k);
            }
        }
        if let Some(parent) = Path::new(&self.file).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(s) = serde_json::to_string(&self.paid_by_uuid) {
            let _ = std::fs::write(&self.file, s);
        }
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_lookup() {
        let mut cb = CostBasis::load("/nonexistent/cost-basis.json");
        assert_eq!(cb.entries(), 0);
        cb.record_cost("", 5.0); // empty uuid ignored
        cb.record_cost("uuid-a", 0.0); // non-positive ignored
        cb.record_cost("uuid-a", 34_000_000.0);
        assert_eq!(cb.entries(), 1);
        assert_eq!(cb.cost_for(Some("uuid-a")), Some(34_000_000.0));
        assert_eq!(cb.cost_for(Some("nope")), None);
        assert_eq!(cb.cost_for(None), None);
        assert!(cb.cost_recorded_at_for(Some("uuid-a")).is_some());
    }

    #[test]
    fn self_listing_guard() {
        let mut cb = CostBasis::load("/nonexistent/cost-basis.json");
        cb.record_own_listing(None, Some("item-1")); // uuid unknown yet
        assert!(cb.is_own_listing(None, Some("item-1")));
        assert!(!cb.is_own_listing(Some("auc-1"), None));
        cb.update_listing_uuid("auc-1", Some("item-1"));
        assert!(cb.is_own_listing(Some("auc-1"), None));
    }

    #[test]
    fn roundtrip_disk_shape() {
        let dir = std::env::temp_dir().join(format!("cbtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cost-basis.json");
        let p = path.to_str().unwrap();
        {
            let mut cb = CostBasis::load(p);
            cb.record_cost("uuid-x", 12_500_000.0);
            cb.flush();
        }
        // Round-trips: reload sees the same entry, and the JSON is the TS shape.
        let raw = std::fs::read_to_string(p).unwrap();
        assert!(raw.contains("\"uuid-x\""));
        assert!(raw.contains("\"paid\""));
        assert!(raw.contains("\"at\""));
        let cb2 = CostBasis::load(p);
        assert_eq!(cb2.cost_for(Some("uuid-x")), Some(12_500_000.0));
        std::fs::remove_dir_all(&dir).ok();
    }
}
