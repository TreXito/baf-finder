//! Rolling buffer of recently-found flips (port of index.ts:142-170).
//!
//! Served over HTTP so baf-backend can match them against the Discord all-flips
//! channel. `attrs`/`median_stats` are stripped (bulky). Persisted to disk so a
//! finder restart doesn't lose flips a bot just bought, since the backend's
//! cross-match retries would otherwise never hit.

use finder_core::sniper::Flip;
use serde_json::Value;
use std::collections::VecDeque;

const CAP: usize = 500;
const MAX_AGE_MS: i64 = 30 * 60_000;

pub struct RecentFlips {
    flips: VecDeque<Value>,
    path: String,
}

impl RecentFlips {
    /// Loads any previously-persisted flips, mirroring the TS restart behavior.
    /// A missing/corrupt file is not an error: TS starts empty in that case too.
    pub fn load(path: &str) -> Self {
        let flips = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
            .map(VecDeque::from)
            .unwrap_or_default();
        Self {
            flips,
            path: path.to_string(),
        }
    }

    /// TS is `const { attrs, medianStats, ...rest } = f`, i.e. the raw Flip minus
    /// those two, with every remaining field UNROUNDED. Note this is NOT the ws
    /// `flip` payload shape (no target/listAt/lbin) — baf-backend cross-matches on
    /// these exact keys. Flip derives no Serialize and its fields are snake_case,
    /// so the camelCase wire names are spelled out here; adding a field to Flip
    /// means adding it here too.
    pub fn remember(&mut self, f: &Flip, now_ms: i64) {
        self.flips.push_back(serde_json::json!({
            "uuid": f.uuid,
            "itemName": f.item_name,
            "finder": f.finder,
            "price": f.price,
            "reference": f.reference,
            "profit": f.profit,
            "roiPct": f.roi_pct,
            "confidence": f.confidence,
            "samples": f.samples,
            "key": f.key,
            "guard": f.guard,
            "foundAfterRefreshMs": f.found_after_refresh_ms,
            "foundAtMs": f.found_at_ms,
        }));
        self.prune(now_ms);
    }

    /// Port of pruneRecentFlips: drop from the front while over CAP or the oldest
    /// is past MAX_AGE_MS. `foundAtMs` is the TS field name on the wire.
    fn prune(&mut self, now_ms: i64) {
        let cutoff = now_ms - MAX_AGE_MS;
        while self.flips.len() > CAP
            || self
                .flips
                .front()
                .and_then(|f| f.get("foundAtMs"))
                .and_then(|v| v.as_i64())
                .is_some_and(|t| t < cutoff)
        {
            if self.flips.pop_front().is_none() {
                break;
            }
        }
    }

    pub fn to_json(&self) -> Value {
        Value::Array(self.flips.iter().cloned().collect())
    }

    pub fn save(&self) -> Result<(), String> {
        let json = serde_json::to_string(&self.to_json()).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, json).map_err(|e| e.to_string())
    }

    /// Only used for the boot log line and tests; `is_empty` is deliberately
    /// absent since nothing calls it (clippy::len_without_is_empty is not
    /// triggered here because this is not a collection type).
    pub fn len(&self) -> usize {
        self.flips.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ring(now: i64, ages: &[i64]) -> RecentFlips {
        let mut r = RecentFlips {
            flips: VecDeque::new(),
            path: String::new(),
        };
        for a in ages {
            r.flips
                .push_back(json!({ "foundAtMs": now - a, "uuid": format!("age{a}") }));
        }
        r.prune(now);
        r
    }

    #[test]
    fn prunes_by_age_not_just_cap() {
        // 31min old is past the 30min window; 29min is inside it.
        let r = ring(1_000_000_000, &[31 * 60_000, 29 * 60_000]);
        assert_eq!(r.len(), 1);
        assert_eq!(r.flips[0]["uuid"], "age1740000");
    }

    #[test]
    fn prunes_by_cap() {
        let now = 1_000_000_000;
        let mut r = RecentFlips {
            flips: VecDeque::new(),
            path: String::new(),
        };
        for i in 0..CAP + 10 {
            r.flips.push_back(json!({ "foundAtMs": now, "uuid": i }));
        }
        r.prune(now);
        assert_eq!(r.len(), CAP);
        // Oldest dropped from the front, so the survivors start at 10.
        assert_eq!(r.flips[0]["uuid"], 10);
    }

    #[test]
    fn stops_at_empty_rather_than_spinning() {
        // Every entry is expired: the loop must terminate, not loop forever on an
        // empty deque (front() is None => is_some_and false => cap check ends it).
        let r = ring(1_000_000_000, &[60 * 60_000, 61 * 60_000]);
        assert_eq!(r.len(), 0);
    }
}
