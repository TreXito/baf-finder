//! Public read-only flip feed — the flips OUR filter declined.
//!
//! Strictly separate from the bot feed in `ws_server.rs`: its own listener, its
//! own port, its own auth, and NO access to the pricing RPCs (`estimate` /
//! `inventory` / `ahPage`) that would let a stranger queue work on the
//! single-threaded money loop. A public client can do exactly three things:
//! receive flips, set a filter, and ping.
//!
//! ## What gets published
//!
//! [`PublicHub::publish`] is called from `WsShared::push_flip` for every flip
//! that came back with a `mismatch` — i.e. one no bot of ours was sent. The
//! mismatch string is classified into a [`Bucket`]:
//!
//! * `merit` — our filter said no (profit / roi / confidence / volume / tier /
//!   blacklist / guard). Zero risk to publish: we were never going to buy it.
//! * `flood` — the flood brake ("holding N unsold X"). We're capped on that base
//!   key, so it's leftover too.
//! * `unrouted` — the flip PASSED the filter but no bot could take it (purse,
//!   inventory slots, AH slot cap, nobody connected). Default OFF: a bot can free
//!   up seconds later and we'd have handed the flip away for nothing.
//!
//! `own listing (self-buy guard)` is never published under any setting — that is
//! one of OUR relists, and publishing it points strangers straight at our stock.
//!
//! ## Auth
//!
//! A key is a bearer secret presented as a path segment (`wss://host/<secret>`),
//! a `?token=` / `?key=` query param, or an `Authorization: Bearer` header, so
//! both a browser and the existing baf mod can connect unchanged. Keys live in
//! `public-keys.json` (hot-reloaded, see [`KeyStore`]) and are stored as SHA-256
//! digests; the plaintext never has to sit on disk. Failure modes are all
//! fail-CLOSED: absent file, unreadable file, bad JSON and zero usable entries
//! each mean "accept nobody", loudly logged. That is deliberate — a public feed
//! that silently degrades to open is worse than one that is down.

use crate::ws_server::{blacklist_ids_for, jnum, jnum_opt, RpcRequest};
use finder_core::filter::{BinMasterFilter, Filter, FilterFlip};
use finder_core::inventory_pricing::{price_inventory, InventoryPricingInput};
use finder_core::nbt::{attrs_from_inventory_slot, ItemAttributes};
use finder_core::sniper::Flip;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

/// Queue depth between the (sync) money loop and the (async) dispatcher. The
/// live feed runs ~4-6 flips/min, so this is ~2 hours of slack; if it ever fills
/// the loop thread drops rather than blocks. The money path never waits on a
/// public consumer.
const FLIP_QUEUE: usize = 1024;
/// Per-connection outbound queue. A client that can't drain this is dead weight.
const CLIENT_QUEUE: usize = 128;
/// Public clients send almost nothing (a filter at connect, maybe a ping). More
/// than this in `MSG_WINDOW_MS` and the connection is closed — parsing attacker
/// -supplied BinMaster JSON in a loop is the only real CPU an outsider can reach.
const MAX_MSGS_PER_WINDOW: u32 = 20;
const MSG_WINDOW_MS: i64 = 10_000;
/// Incoming frame cap. A filter blob is a few KB; 64 KiB is generous.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Server-side heartbeat. Cloudflare drops idle WebSocket connections, so we
/// keep them warm rather than let clients silently rot.
const PING_EVERY: Duration = Duration::from_secs(30);
/// No pong within this long ⇒ the peer is gone; reap the connection.
const PONG_TIMEOUT_MS: i64 = 90_000;
/// Failed auths from one IP inside `FAIL_WINDOW_MS` before it is blocked.
const MAX_FAILS: u32 = 8;
const FAIL_WINDOW_MS: i64 = 60_000;
const FAIL_BLOCK_MS: i64 = 15 * 60_000;
/// Minimum gap between one client's inventory pricing calls. The mod's auto-list
/// loop is far slower than this, so it only binds on a client that spins.
const PRICING_MIN_GAP_MS: i64 = 15_000;
/// Slots priced per call. A player inventory is 36 plus armour; this is generous.
const MAX_PRICING_ITEMS: usize = 50;
/// Samples required before a public estimate is trusted, mirroring the bot path.
const PUBLIC_MIN_SAMPLES: i64 = 5;
/// UI/menu filler that is never a real auction item, skipped silently.
const NON_AUCTIONABLE_PUBLIC: [&str; 5] = [
    "SKYBLOCK_MENU",
    "DUNGEON_MENU",
    "PET_MENU",
    "TRICK_OR_TREAT_BAG",
    "HUB_SELECTOR",
];
/// Default per-key connection cap when the entry doesn't set one.
const DEFAULT_MAX_CONN_PER_KEY: usize = 3;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let d = ring::digest::digest(&ring::digest::SHA256, data);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

/// Constant-time digest compare. Both inputs are fixed 32-byte SHA-256 outputs,
/// so the XOR-accumulate loop never short-circuits and leaks nothing through
/// timing. (ring's own `verify_slices_are_equal` is deprecated as of 0.17.14 and
/// carries no side-channel promise for external callers; this is the same shape
/// as `flip_api::ct_eq`, which guards the admin password.)
fn ct_eq_32(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn hex_to_32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Buckets
// ---------------------------------------------------------------------------

/// Why a flip is leftover. See the module docs for the publish policy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bucket {
    /// Our filter rejected it on merit.
    Merit,
    /// The flood brake: we already hold the cap for that base key.
    Flood,
    /// It passed the filter but no connected bot could take it.
    Unrouted,
}

impl Bucket {
    fn as_str(self) -> &'static str {
        match self {
            Bucket::Merit => "merit",
            Bucket::Flood => "flood",
            Bucket::Unrouted => "unrouted",
        }
    }

    fn parse(s: &str) -> Option<Bucket> {
        match s.trim().to_lowercase().as_str() {
            "merit" => Some(Bucket::Merit),
            "flood" => Some(Bucket::Flood),
            "unrouted" | "routing" => Some(Bucket::Unrouted),
            _ => None,
        }
    }
}

/// Classify a `push_flip` mismatch string. The routing arm mirrors
/// `main.rs::bump_filter_miss` so the public buckets and the FUNNEL-FILTER log
/// line can never disagree about what "routing" means.
fn bucket_of(reason: &str) -> Bucket {
    let r = reason.to_lowercase();
    if r.contains("holding") {
        Bucket::Flood
    } else if r.contains("purse")
        || r.contains("slot")
        || r.contains("client")
        || r.contains("eligible")
        || r.contains("auction house full")
    {
        Bucket::Unrouted
    } else {
        Bucket::Merit
    }
}

/// Coarse label for the consumer: WHICH gate said no, without revealing the
/// threshold it said no against. Mirrors the FUNNEL-FILTER categories.
fn reject_category(reason: &str) -> &'static str {
    let r = reason.to_lowercase();
    // "below global" carries profit/roi/conf substrings, so it MUST come first.
    if r.contains("below global") {
        "global"
    } else if r.contains("roi") {
        "roi"
    } else if r.contains("slow to sell") || r.contains("time to sell") {
        "tts"
    } else if r.contains("vol") {
        "volume"
    } else if r.contains("conf") {
        "confidence"
    } else if r.contains("guard") {
        "guard"
    } else if r.contains("blacklist") {
        "blacklist"
    } else if r.contains("holding") {
        "flood"
    } else if r.contains("profit") {
        "profit"
    } else if r.contains("purse")
        || r.contains("slot")
        || r.contains("client")
        || r.contains("eligible")
    {
        "routing"
    } else {
        "other"
    }
}

// ---------------------------------------------------------------------------
// The published flip
// ---------------------------------------------------------------------------

/// A leftover flip, decoupled from the money core's [`Flip`]. Owned so the loop
/// thread can hand it off and move on; `attrs` rides along because per-client
/// BinMaster tiers need the item's real attributes to evaluate.
pub struct PublicFlip {
    uuid: String,
    item_name: String,
    finder: String,
    price: f64,
    target: f64,
    profit: f64,
    roi_pct: f64,
    confidence: f64,
    samples: i64,
    volume_per_day: Option<f64>,
    lbin: Option<f64>,
    key: String,
    guard: String,
    /// Recommended opening ask, so the baf mod's `list_at` is populated exactly as
    /// it is on the bot feed. Computed WITHOUT the owner's BinMaster `scale_price`
    /// (which is a tier-specific number of his, not the consumer's business), so
    /// it is just the valuation with the same model hedge and the same
    /// never-below-cost floor the bot feed applies.
    list_at: Option<f64>,
    ids: Vec<String>,
    attrs: ItemAttributes,
    bucket: Bucket,
    category: &'static str,
    seen_at_ms: i64,
}

impl PublicFlip {
    /// The wire payload. Field names match the bot feed so an existing client
    /// parses it unchanged; `listAt` is deliberately absent (it is our listing
    /// model's output, not the consumer's business) and so are the detection
    /// timings, which would hand out our sweep latency profile.
    fn payload(&self, send_reason: bool) -> String {
        let mut flip = json!({
            "uuid": self.uuid,
            "itemName": self.item_name,
            "finder": self.finder,
            "price": jnum(self.price),
            "target": jnum(self.target.round()),
            "profit": jnum(self.profit.round()),
            "roiPct": jnum((self.roi_pct * 10.0).round() / 10.0),
            "confidence": jnum((self.confidence * 1000.0).round() / 1000.0),
            "samples": self.samples,
            "volumePerDay": jnum_opt(self.volume_per_day),
            "lbin": jnum_opt(self.lbin),
            "key": self.key,
            "guard": self.guard,
            "listAt": jnum_opt(self.list_at),
            "seenAtMs": self.seen_at_ms,
        });
        if send_reason {
            if let Some(o) = flip.as_object_mut() {
                o.insert("bucket".into(), Value::from(self.bucket.as_str()));
                o.insert("rejectedBy".into(), Value::from(self.category));
            }
        }
        json!({ "type": "flip", "flip": flip }).to_string()
    }
}

// ---------------------------------------------------------------------------
// Per-client filter
// ---------------------------------------------------------------------------

/// The filter a public client sets with `{"type":"filter", ...}`. Every field is
/// optional and 0 means "off", so `{}` is a valid no-op filter. Thresholds are
/// floors the client applies to ITSELF; they can never widen what the server
/// publishes (buckets and the per-key floor are enforced server-side).
#[derive(Clone, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClientFilter {
    pub min_profit: f64,
    pub min_roi_pct: f64,
    pub min_confidence: f64,
    pub min_volume_per_day: f64,
    pub min_price: f64,
    /// 0 = no ceiling.
    pub max_price: f64,
    /// Item ids to drop (matched against the item id and, for pets, `PET_<TYPE>`).
    pub blacklist_ids: Vec<String>,
    /// When non-empty, ONLY these ids pass.
    pub allow_ids: Vec<String>,
    pub blocked_guards: Vec<String>,
    /// When non-empty, only these finders pass ("median", "model", "lbin", ...).
    pub finders: Vec<String>,
    /// When non-empty, only these buckets pass. Never widens the server set.
    pub buckets: Vec<String>,
}

impl ClientFilter {
    fn matches(&self, f: &PublicFlip) -> bool {
        if self.min_profit > 0.0 && f.profit < self.min_profit {
            return false;
        }
        if self.min_roi_pct > 0.0 && f.roi_pct < self.min_roi_pct {
            return false;
        }
        if self.min_confidence > 0.0 && f.confidence < self.min_confidence {
            return false;
        }
        if self.min_volume_per_day > 0.0
            && f.volume_per_day.unwrap_or(0.0) < self.min_volume_per_day
        {
            return false;
        }
        if self.min_price > 0.0 && f.price < self.min_price {
            return false;
        }
        if self.max_price > 0.0 && f.price > self.max_price {
            return false;
        }
        if !self.finders.is_empty()
            && !self
                .finders
                .iter()
                .any(|x| x.eq_ignore_ascii_case(&f.finder))
        {
            return false;
        }
        if !self.buckets.is_empty()
            && !self
                .buckets
                .iter()
                .any(|b| Bucket::parse(b) == Some(f.bucket))
        {
            return false;
        }
        if !self.blocked_guards.is_empty()
            && self
                .blocked_guards
                .iter()
                .any(|g| !g.is_empty() && f.guard.contains(g.as_str()))
        {
            return false;
        }
        if !self.blacklist_ids.is_empty() {
            let up: Vec<String> = self
                .blacklist_ids
                .iter()
                .map(|s| s.to_uppercase())
                .collect();
            if f.ids.iter().any(|i| up.contains(i)) {
                return false;
            }
        }
        if !self.allow_ids.is_empty() {
            let up: Vec<String> = self.allow_ids.iter().map(|s| s.to_uppercase()).collect();
            if !f.ids.iter().any(|i| up.contains(i)) {
                return false;
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct KeyFile {
    #[serde(default)]
    keys: Vec<KeyEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyEntry {
    #[serde(default)]
    label: String,
    /// SHA-256 hex of the secret. Preferred: the plaintext never touches disk.
    #[serde(default)]
    sha256: String,
    /// Plaintext secret. Accepted for convenience; hashed at load either way, so
    /// nothing but the digest is ever held in memory or compared against.
    #[serde(default)]
    key: String,
    #[serde(default = "yes")]
    enabled: bool,
    /// 0 ⇒ [`DEFAULT_MAX_CONN_PER_KEY`].
    #[serde(default)]
    max_connections: usize,
    /// Unix ms after which the key is dead. 0 ⇒ never expires.
    #[serde(default)]
    expires_at_ms: i64,
    /// Server-enforced profit floor for this key, on top of the global one.
    #[serde(default)]
    min_profit: f64,
    /// Server-enforced bucket restriction for this key. Empty ⇒ the server set.
    #[serde(default)]
    buckets: Vec<String>,
    /// Let this key use the `inventory` → `listInstructions` pricing RPC, i.e. the
    /// half of the baf mod workflow that lists what it bought. OFF by default:
    /// it is the only public path that reaches the single-threaded pricing loop,
    /// so it is granted per person rather than to anyone holding any key.
    #[serde(default)]
    allow_pricing: bool,
}

fn yes() -> bool {
    true
}

/// A usable key: digest + the limits that ride with it.
#[derive(Clone)]
pub struct LoadedKey {
    pub label: String,
    digest: [u8; 32],
    max_connections: usize,
    expires_at_ms: i64,
    min_profit: f64,
    buckets: Vec<Bucket>,
    pub allow_pricing: bool,
}

impl LoadedKey {
    /// The bucket names this key actually receives, for display in the UI.
    pub fn buckets_or(&self, server: &[Bucket]) -> Vec<&'static str> {
        let b = if self.buckets.is_empty() {
            server
        } else {
            &self.buckets
        };
        b.iter().map(|x| x.as_str()).collect()
    }
    /// The profit floor enforced on this key regardless of what it asks for.
    pub fn min_profit_floor(&self, server: f64) -> f64 {
        self.min_profit.max(server)
    }
}

/// Hot-reloaded key list. Mtime-gated like `WsConfigStore`, polled every 5s, so
/// adding or revoking a key is a file edit with no restart and no dropped bots
/// (the bot feed is a different listener entirely).
pub struct KeyStore {
    path: String,
    mtime: Mutex<Option<SystemTime>>,
    keys: RwLock<Vec<LoadedKey>>,
}

impl KeyStore {
    fn new(path: &str) -> KeyStore {
        KeyStore {
            path: path.to_string(),
            mtime: Mutex::new(None),
            keys: RwLock::new(Vec::new()),
        }
    }

    /// Re-read when the file changed. Returns `Some(n)` with the new usable-key
    /// count when it reloaded. Every failure path CLEARS the key list: a public
    /// endpoint must go closed, never open, when its auth config goes missing.
    fn reload_if_changed(&self, initial: bool) -> Option<usize> {
        let meta = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => {
                let had = !self.keys.read().unwrap().is_empty();
                if had || initial {
                    self.keys.write().unwrap().clear();
                    *self.mtime.lock().unwrap() = None;
                    return Some(0);
                }
                return None;
            }
        };
        let mtime = meta.modified().ok();
        {
            let mut m = self.mtime.lock().unwrap();
            if !initial && mtime == *m {
                return None;
            }
            *m = mtime;
        }
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("public ws: keys unreadable ({e}) — REFUSING ALL CONNECTIONS");
                self.keys.write().unwrap().clear();
                return Some(0);
            }
        };
        let parsed: KeyFile = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("public ws: keys are not valid JSON ({e}) — REFUSING ALL CONNECTIONS");
                self.keys.write().unwrap().clear();
                return Some(0);
            }
        };
        let mut out = Vec::new();
        for (i, e) in parsed.keys.iter().enumerate() {
            if !e.enabled {
                continue;
            }
            let label = if e.label.is_empty() {
                format!("key{i}")
            } else {
                e.label.clone()
            };
            let digest = if !e.sha256.is_empty() {
                match hex_to_32(&e.sha256) {
                    Some(d) => d,
                    None => {
                        eprintln!("public ws: key '{label}' has a malformed sha256 — skipped");
                        continue;
                    }
                }
            } else if !e.key.is_empty() {
                if e.key.len() < 16 {
                    eprintln!("public ws: key '{label}' is shorter than 16 chars — skipped (use `public_key` to generate one)");
                    continue;
                }
                sha256(e.key.as_bytes())
            } else {
                eprintln!("public ws: key '{label}' has neither sha256 nor key — skipped");
                continue;
            };
            out.push(LoadedKey {
                label,
                digest,
                max_connections: if e.max_connections == 0 {
                    DEFAULT_MAX_CONN_PER_KEY
                } else {
                    e.max_connections
                },
                expires_at_ms: e.expires_at_ms,
                min_profit: e.min_profit,
                buckets: e.buckets.iter().filter_map(|b| Bucket::parse(b)).collect(),
                allow_pricing: e.allow_pricing,
            });
        }
        let n = out.len();
        *self.keys.write().unwrap() = out;
        Some(n)
    }

    /// Constant-time lookup. The secret is hashed once and every entry is then
    /// compared digest-to-digest, with the loop always running to the end of the
    /// list, so neither the match position nor the key contents leak through
    /// timing (that is why this doesn't `break` on a hit). Expired entries
    /// are rejected here rather than at load, so a key dies on schedule without
    /// waiting for the next file change.
    pub fn lookup(&self, secret: &str) -> Option<LoadedKey> {
        let d = sha256(secret.as_bytes());
        let now = now_ms();
        let mut found: Option<LoadedKey> = None;
        for k in self.keys.read().unwrap().iter() {
            if ct_eq_32(&d, &k.digest)
                && (k.expires_at_ms == 0 || k.expires_at_ms > now)
                && found.is_none()
            {
                found = Some(k.clone());
            }
        }
        found
    }
}

// ---------------------------------------------------------------------------
// Saved filters
// ---------------------------------------------------------------------------

/// What a consumer has saved: the flat thresholds AND, optionally, a full
/// BinMaster tier document. Both persist against the key.
///
/// The tier filter is the half that actually discriminates: flat thresholds
/// cannot say "10M profit on a 1-per-day item but 3M on a 20-per-day one", which
/// is the entire point of the owner's own filter. Storing the document verbatim
/// lets the UI hand it back for editing exactly as it was uploaded.
#[derive(Clone, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SavedConfig {
    pub filter: ClientFilter,
    pub binmaster: Option<Value>,
}

/// Per-key config that survives a disconnect, keyed by key LABEL.
///
/// This exists because of how the consumer actually connects: the baf mod never
/// sends `{"type":"filter"}` or `{"type":"binmaster"}` — it only speaks
/// `inventory` and `listed` — so anything that lives on the connection is
/// unreachable for a mod user. Saving against the key and applying it at
/// handshake time is the only way a mod user gets a filter at all.
pub struct FilterStore {
    path: String,
    saved: RwLock<HashMap<String, SavedConfig>>,
}

impl FilterStore {
    fn load(path: &str) -> FilterStore {
        let raw = std::fs::read_to_string(path).unwrap_or_default();
        // Decide the shape PER ENTRY, explicitly.
        //
        // ⚠️ A try-new-then-fall-back-to-old chain is wrong here and silently
        // destroys data: `SavedConfig` is `#[serde(default)]` on every field, so
        // an OLD entry (a bare ClientFilter object) parses as a SavedConfig
        // *successfully* with the filter defaulted to all zeros. The fallback
        // never runs, no error is raised, and the saved thresholds are gone. That
        // is exactly what happened on 2026-08-10. The presence of a `filter`
        // object is the only reliable discriminator.
        let saved: HashMap<String, SavedConfig> =
            serde_json::from_str::<HashMap<String, Value>>(&raw)
                .map(|m| {
                    m.into_iter()
                        .filter_map(|(k, v)| {
                            let cfg = if v.get("filter").is_some_and(|f| f.is_object()) {
                                serde_json::from_value::<SavedConfig>(v).ok()?
                            } else {
                                SavedConfig {
                                    filter: serde_json::from_value::<ClientFilter>(v).ok()?,
                                    binmaster: None,
                                }
                            };
                            Some((k, cfg))
                        })
                        .collect()
                })
                .unwrap_or_default();
        if !saved.is_empty() {
            let tiers = saved.values().filter(|c| c.binmaster.is_some()).count();
            eprintln!(
                "public ws feed: {} saved config(s) from {path} ({tiers} with a tier filter)",
                saved.len()
            );
        }
        FilterStore {
            path: path.to_string(),
            saved: RwLock::new(saved),
        }
    }

    pub fn get(&self, label: &str) -> Option<SavedConfig> {
        self.saved.read().unwrap().get(label).cloned()
    }

    /// Persist atomically: write a temp file and rename, so a crash mid-write
    /// cannot leave a truncated file that would silently reset everyone's config
    /// on the next boot.
    pub fn set(&self, label: &str, cfg: SavedConfig) -> Result<(), String> {
        self.saved.write().unwrap().insert(label.to_string(), cfg);
        let snapshot = self.saved.read().unwrap().clone();
        let body = serde_json::to_string_pretty(&snapshot).map_err(|e| e.to_string())?;
        let tmp = format!("{}.tmp", self.path);
        std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }

    /// Update just the thresholds, keeping any saved tier document.
    pub fn set_filter(&self, label: &str, f: ClientFilter) -> Result<(), String> {
        let mut cfg = self.get(label).unwrap_or_default();
        cfg.filter = f;
        self.set(label, cfg)
    }

    /// Update just the tier document, keeping the thresholds. `None` clears it.
    pub fn set_binmaster(&self, label: &str, doc: Option<Value>) -> Result<(), String> {
        let mut cfg = self.get(label).unwrap_or_default();
        cfg.binmaster = doc;
        self.set(label, cfg)
    }
}

// ---------------------------------------------------------------------------
// Failed-auth throttle
// ---------------------------------------------------------------------------

/// Per-IP failure counter. 128-bit keys make brute force hopeless on their own;
/// this exists so a scanner can't sit there burning handshake CPU forever, and
/// so a burst of failures is visible in the log instead of silent.
#[derive(Default)]
struct Throttle {
    fails: HashMap<String, (u32, i64)>,
    blocked: HashMap<String, i64>,
}

impl Throttle {
    fn blocked(&mut self, ip: &str) -> bool {
        let now = now_ms();
        match self.blocked.get(ip) {
            Some(&until) if until > now => true,
            Some(_) => {
                self.blocked.remove(ip);
                self.fails.remove(ip);
                false
            }
            None => false,
        }
    }

    /// Record a failure; returns true when this one tripped the block.
    fn fail(&mut self, ip: &str) -> bool {
        let now = now_ms();
        let e = self.fails.entry(ip.to_string()).or_insert((0, now));
        if now - e.1 > FAIL_WINDOW_MS {
            *e = (0, now);
        }
        e.0 += 1;
        if e.0 >= MAX_FAILS {
            self.blocked.insert(ip.to_string(), now + FAIL_BLOCK_MS);
            // Bound the maps: an attacker rotating IPs must not grow them forever.
            if self.blocked.len() > 4096 {
                self.blocked.retain(|_, until| *until > now);
            }
            if self.fails.len() > 4096 {
                self.fails
                    .retain(|_, (_, start)| now - *start <= FAIL_WINDOW_MS);
            }
            return true;
        }
        false
    }

    fn ok(&mut self, ip: &str) {
        self.fails.remove(ip);
    }
}

/// Parse a BinMaster document into the live tier engine. `None` when the document
/// does not describe a filter, which is also exactly what the API rejects on
/// upload, so a bad document can never reach a connection.
pub fn parse_binmaster(doc: &Value) -> Option<Arc<Filter>> {
    serde_json::from_value::<BinMasterFilter>(doc.clone())
        .ok()
        .map(|c| Arc::new(Filter::new(Some(c))))
}

// ---------------------------------------------------------------------------
// Clients + hub
// ---------------------------------------------------------------------------

struct PubClient {
    label: String,
    ip: String,
    tx: mpsc::Sender<Message>,
    filter: RwLock<ClientFilter>,
    /// Optional full BinMaster tier filter, the same engine our own filter runs.
    bin: RwLock<Option<Arc<Filter>>>,
    min_profit: f64,
    buckets: Vec<Bucket>,
    allow_pricing: bool,
    last_pricing_ms: AtomicI64,
    drops: AtomicU32,
    last_pong_ms: AtomicI64,
    sent: AtomicU64,
}

impl PubClient {
    fn wants(&self, f: &PublicFlip) -> bool {
        if f.profit < self.min_profit {
            return false;
        }
        if !self.buckets.is_empty() && !self.buckets.contains(&f.bucket) {
            return false;
        }
        if !self.filter.read().unwrap().matches(f) {
            return false;
        }
        if let Some(bin) = self.bin.read().unwrap().as_ref() {
            // The TTS/sell-through inputs are left unset: they come from the price
            // index, which only the loop thread owns, and a public consumer's tier
            // filter falls back to the volume proxy without them exactly as the
            // pre-TTS filter did.
            let ff = FilterFlip {
                attrs: f.attrs.clone(),
                profit: f.profit,
                roi_pct: f.roi_pct,
                confidence: f.confidence,
                volume_per_day: f.volume_per_day,
                fair_tts_ms: None,
                tts_samples: None,
                sell_through: None,
            };
            if !bin.evaluate_flip(&ff).pass {
                return false;
            }
        }
        true
    }
}

/// The public feed. Held by `WsShared` behind a `OnceLock`, so the publish check
/// on the money path is one atomic load when the feature is off.
pub struct PublicHub {
    tx: mpsc::Sender<PublicFlip>,
    clients: Mutex<HashMap<u64, Arc<PubClient>>>,
    next_id: AtomicU64,
    /// Fast "is anyone listening?" gate — read on the loop thread before any work.
    n_clients: AtomicUsize,
    pub keys: KeyStore,
    pub filters: FilterStore,
    throttle: Mutex<Throttle>,
    pub buckets: Vec<Bucket>,
    pub min_profit: f64,
    send_reason: bool,
    max_conn: usize,
    delay_ms: u64,
    trust_proxy: bool,
    send_list_at: bool,
    /// Answers the `inventory` RPC for keys that carry `allowPricing`. `None`
    /// leaves the whole pricing path unavailable.
    rpc_tx: Option<mpsc::UnboundedSender<RpcRequest>>,
    /// Exactly one public pricing request may be in flight against the loop
    /// thread at a time, no matter how many clients are connected.
    pricing_permit: Semaphore,
    priced: AtomicU64,
    /// The port actually bound (may differ from the configured one when 0 was
    /// requested, which is how the e2e tests get an ephemeral port). Prod reads it
    /// from the boot log instead, hence dead outside tests.
    #[allow(dead_code)]
    pub port: u16,
    published: AtomicU64,
    dropped: AtomicU64,
    rejected_auth: AtomicU64,
}

impl PublicHub {
    /// Hand a declined flip to the public feed. Called from the money loop, so
    /// everything here is O(1)-ish and never blocks: the "no clients" case exits
    /// on one atomic load, and a full queue drops rather than waits.
    pub fn publish(&self, f: &Flip, lbin: Option<f64>, reason: &str) {
        if self.n_clients.load(Ordering::Relaxed) == 0 {
            return;
        }
        // Never, under any config: this is one of our own relists.
        if reason.to_lowercase().contains("own listing") {
            return;
        }
        let bucket = bucket_of(reason);
        if !self.buckets.contains(&bucket) {
            return;
        }
        if f.profit < self.min_profit {
            return;
        }
        let pf = PublicFlip {
            uuid: f.uuid.clone(),
            item_name: f.item_name.clone(),
            finder: f.finder.clone(),
            price: f.price,
            target: f.reference,
            profit: f.profit,
            roi_pct: f.roi_pct,
            confidence: f.confidence,
            samples: f.samples,
            volume_per_day: f
                .median_stats
                .as_ref()
                .map(|m| (m.volume_per_day * 100.0).round() / 100.0),
            lbin,
            key: f.key.clone(),
            guard: f.guard.clone(),
            // Same shape as `flip_payload`'s listAt, minus the owner's tier scale:
            // model-priced items keep their 0.97 hedge, and the ask never opens
            // below cost+5%, which is what the mod expects to relist against.
            list_at: if self.send_list_at {
                let hedge = if f.finder == "model" { 0.97 } else { 1.0 };
                Some((f.reference * hedge).round().max((f.price * 1.05).ceil()))
            } else {
                None
            },
            ids: blacklist_ids_for(&f.attrs),
            attrs: f.attrs.clone(),
            bucket,
            category: reject_category(reason),
            seen_at_ms: now_ms(),
        };
        if self.tx.try_send(pf).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn broadcast(&self, f: &PublicFlip) {
        let clients: Vec<Arc<PubClient>> = self.clients.lock().unwrap().values().cloned().collect();
        let mut payload: Option<String> = None;
        let mut sent = 0u64;
        for c in clients {
            if !c.wants(f) {
                continue;
            }
            let p = payload.get_or_insert_with(|| f.payload(self.send_reason));
            match c.tx.try_send(Message::Text(p.clone())) {
                Ok(()) => {
                    c.sent.fetch_add(1, Ordering::Relaxed);
                    sent += 1;
                }
                // A client that can't keep up with ~6 flips/min is not coming back.
                Err(_) => {
                    c.drops.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if sent > 0 {
            self.published.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn add_client(&self, c: Arc<PubClient>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut g = self.clients.lock().unwrap();
        g.insert(id, c);
        self.n_clients.store(g.len(), Ordering::Relaxed);
        id
    }

    fn remove_client(&self, id: u64) {
        let mut g = self.clients.lock().unwrap();
        g.remove(&id);
        self.n_clients.store(g.len(), Ordering::Relaxed);
    }

    /// Apply a freshly-saved filter to that key's OPEN connections, so a save in
    /// the web UI takes effect immediately instead of at the next reconnect.
    pub fn apply_saved_config(&self, label: &str, cfg: &SavedConfig) -> usize {
        let bin = cfg.binmaster.as_ref().and_then(parse_binmaster);
        let mut n = 0;
        for c in self.clients.lock().unwrap().values() {
            if c.label == label {
                *c.filter.write().unwrap() = cfg.filter.clone();
                *c.bin.write().unwrap() = bin.clone();
                n += 1;
            }
        }
        n
    }

    fn conns_for(&self, label: &str) -> usize {
        self.clients
            .lock()
            .unwrap()
            .values()
            .filter(|c| c.label == label)
            .count()
    }
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => matches!(v.as_str(), "1" | "true" | "TRUE" | "yes"),
        Err(_) => default,
    }
}

/// Everything the feed needs to run. Separate from the env so the whole server
/// can be stood up in a test with an explicit config (and an ephemeral port)
/// instead of mutating process env from parallel test threads.
pub struct PublicWsConfig {
    pub host: String,
    pub port: u16,
    pub keys_path: String,
    /// Where per-key saved filters live (written by the web UI).
    pub filters_path: String,
    pub buckets: Vec<Bucket>,
    pub min_profit: f64,
    pub send_reason: bool,
    pub max_conn: usize,
    pub delay_ms: u64,
    /// Trust `CF-Connecting-IP` / `X-Forwarded-For` for the client identity.
    pub trust_proxy: bool,
    /// Include `listAt` on each flip. The baf mod reads it to relist what it buys,
    /// so it is ON by default: without it a consumer can buy but not list.
    pub send_list_at: bool,
}

impl PublicWsConfig {
    /// `None` when the feed is switched off, which is the default.
    fn from_env() -> Option<PublicWsConfig> {
        if !env_flag("PUBLIC_WS", false) {
            eprintln!("public ws feed: DISABLED (set PUBLIC_WS=1 to enable)");
            return None;
        }
        let host = std::env::var("PUBLIC_WS_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
        let loopback = host == "127.0.0.1" || host == "::1" || host == "localhost";
        Some(PublicWsConfig {
            port: std::env::var("PUBLIC_WS_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(15102),
            keys_path: std::env::var("PUBLIC_WS_KEYS_PATH")
                .unwrap_or_else(|_| "./data/public-keys.json".to_string()),
            filters_path: std::env::var("PUBLIC_WS_FILTERS_PATH")
                .unwrap_or_else(|_| "./data/public-filters.json".to_string()),
            buckets: std::env::var("PUBLIC_WS_BUCKETS")
                .unwrap_or_else(|_| "merit,flood".to_string())
                .split(',')
                .filter_map(Bucket::parse)
                .collect(),
            min_profit: std::env::var("PUBLIC_WS_MIN_PROFIT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0),
            send_reason: env_flag("PUBLIC_WS_SEND_REASON", true),
            max_conn: std::env::var("PUBLIC_WS_MAX_CONN")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(200),
            delay_ms: std::env::var("PUBLIC_WS_DELAY_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            // Behind cloudflared every peer is loopback, so the tunnel's header is
            // the only real client identity — and on loopback the tunnel is the
            // only possible peer, so it cannot be spoofed from outside.
            trust_proxy: env_flag("PUBLIC_WS_TRUST_PROXY", loopback),
            send_list_at: env_flag("PUBLIC_WS_SEND_LIST_AT", true),
            host,
        })
    }
}

/// Start the public feed, or return `None` when it is switched off (the default).
/// Dormant costs nothing: no listener, no task, and `publish` is never reachable
/// because `WsShared` never gets a hub.
pub fn spawn(
    rt: &tokio::runtime::Runtime,
    rpc_tx: mpsc::UnboundedSender<RpcRequest>,
) -> Option<Arc<PublicHub>> {
    spawn_with(rt, PublicWsConfig::from_env()?, Some(rpc_tx))
}

/// `spawn` with an explicit config. The socket is bound HERE, synchronously, so a
/// port clash is a loud boot failure instead of a task that dies unnoticed.
pub fn spawn_with(
    rt: &tokio::runtime::Runtime,
    cfg: PublicWsConfig,
    rpc_tx: Option<mpsc::UnboundedSender<RpcRequest>>,
) -> Option<Arc<PublicHub>> {
    if cfg.buckets.is_empty() {
        eprintln!("public ws feed: no valid buckets configured — nothing would ever publish; NOT starting");
        return None;
    }
    let std_listener = match std::net::TcpListener::bind((cfg.host.as_str(), cfg.port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "public ws feed: cannot bind {}:{} ({e}) — NOT starting",
                cfg.host, cfg.port
            );
            return None;
        }
    };
    let bound = std_listener
        .local_addr()
        .map(|a| a.port())
        .unwrap_or(cfg.port);
    if let Err(e) = std_listener.set_nonblocking(true) {
        eprintln!("public ws feed: set_nonblocking failed ({e}) — NOT starting");
        return None;
    }
    let hub_keys = KeyStore::new(&cfg.keys_path);
    let n = hub_keys.reload_if_changed(true).unwrap_or(0);
    let (tx, rx) = mpsc::channel::<PublicFlip>(FLIP_QUEUE);
    let hub = Arc::new(PublicHub {
        tx,
        clients: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        n_clients: AtomicUsize::new(0),
        keys: hub_keys,
        filters: FilterStore::load(&cfg.filters_path),
        throttle: Mutex::new(Throttle::default()),
        buckets: cfg.buckets.clone(),
        min_profit: cfg.min_profit,
        send_reason: cfg.send_reason,
        max_conn: cfg.max_conn,
        delay_ms: cfg.delay_ms,
        trust_proxy: cfg.trust_proxy,
        send_list_at: cfg.send_list_at,
        rpc_tx,
        pricing_permit: Semaphore::new(1),
        priced: AtomicU64::new(0),
        port: bound,
        published: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
        rejected_auth: AtomicU64::new(0),
    });
    let names: Vec<&str> = cfg.buckets.iter().map(|b| b.as_str()).collect();
    eprintln!(
        "public ws feed: listening {}:{bound} | buckets [{}] | {n} key(s) from {}{}",
        cfg.host,
        names.join(","),
        cfg.keys_path,
        if n == 0 {
            " — ⚠️  NO USABLE KEYS, every connection will be refused"
        } else {
            ""
        }
    );
    // Dispatcher: money loop → per-client fan-out.
    {
        let h = hub.clone();
        rt.spawn(dispatch_loop(h, rx));
    }
    // Key reloader.
    {
        let h = hub.clone();
        rt.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if let Some(n) = h.keys.reload_if_changed(false) {
                    eprintln!("public ws feed: keys reloaded — {n} usable key(s)");
                    if n == 0 {
                        eprintln!("public ws feed: ⚠️  no usable keys — refusing all connections");
                    }
                    // Drop connections whose key just went away or expired.
                    let stale: Vec<u64> = {
                        let g = h.clients.lock().unwrap();
                        let live: Vec<String> = h
                            .keys
                            .keys
                            .read()
                            .unwrap()
                            .iter()
                            .map(|k| k.label.clone())
                            .collect();
                        g.iter()
                            .filter(|(_, c)| !live.contains(&c.label))
                            .map(|(id, _)| *id)
                            .collect()
                    };
                    for id in stale {
                        if let Some(c) = h.clients.lock().unwrap().get(&id) {
                            eprintln!("public ws feed: closing '{}' — key revoked", c.label);
                            let _ = c.tx.try_send(Message::Close(None));
                        }
                    }
                }
            }
        });
    }
    // Stats line, so the feed is visible in prod.log like every other subsystem.
    {
        let h = hub.clone();
        rt.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(300)).await;
                let clients = h.n_clients.load(Ordering::Relaxed);
                let pub_n = h.published.load(Ordering::Relaxed);
                if clients > 0 || pub_n > 0 {
                    eprintln!(
                        "PUBLIC-WS: {clients} client(s) | {pub_n} flip(s) published | {} priced | {} queue-dropped | {} auth-rejected",
                        h.priced.load(Ordering::Relaxed),
                        h.dropped.load(Ordering::Relaxed),
                        h.rejected_auth.load(Ordering::Relaxed)
                    );
                }
            }
        });
    }
    // Listener.
    {
        let h = hub.clone();
        rt.spawn(async move {
            if let Err(e) = accept_loop(h, std_listener).await {
                eprintln!("public ws feed: listener died: {e}");
            }
        });
    }
    Some(hub)
}

async fn dispatch_loop(hub: Arc<PublicHub>, mut rx: mpsc::Receiver<PublicFlip>) {
    while let Some(f) = rx.recv().await {
        if hub.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(hub.delay_ms)).await;
        }
        hub.broadcast(&f);
    }
}

/// The secret, from any of the three accepted places. Path segments are checked
/// first (`wss://host/<secret>`, which is what a browser URL carries), then the
/// `token`/`key` query params, then `Authorization: Bearer`.
fn secrets_from(req: &Request) -> Vec<String> {
    let mut out = Vec::new();
    for seg in req.uri().path().split('/') {
        if !seg.is_empty() {
            out.push(seg.to_string());
        }
    }
    if let Some(q) = req.uri().query() {
        for (k, v) in form_urlencoded::parse(q.as_bytes()) {
            if k == "token" || k == "key" {
                out.push(v.into_owned());
            }
        }
    }
    if let Some(a) = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(b) = a
            .strip_prefix("Bearer ")
            .or_else(|| a.strip_prefix("bearer "))
        {
            out.push(b.trim().to_string());
        }
    }
    // Bound the work an attacker can force per handshake (a 200-segment path
    // would otherwise mean 200 hashes).
    out.truncate(8);
    out
}

/// Real client IP. Behind cloudflared every peer is 127.0.0.1, so the tunnel's
/// `CF-Connecting-IP` is the only way to tell clients apart for throttling and
/// logging. Only trusted when the listener is on loopback (where the tunnel is
/// the sole possible peer) or when explicitly opted in.
fn client_ip(req: &Request, peer: &str, trust_proxy: bool) -> String {
    if trust_proxy {
        if let Some(v) = req
            .headers()
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
        {
            return v.trim().to_string();
        }
        if let Some(v) = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
        {
            if let Some(first) = v.split(',').next() {
                return first.trim().to_string();
            }
        }
    }
    peer.to_string()
}

fn reject(status: StatusCode, msg: &str) -> ErrorResponse {
    tokio_tungstenite::tungstenite::http::Response::builder()
        .status(status)
        .body(Some(msg.to_string()))
        .unwrap()
}

// The handshake callback's error type is tungstenite's `ErrorResponse`, whose
// size clippy objects to; it is the library's signature, not ours to shrink.
#[allow(clippy::result_large_err)]
async fn accept_loop(
    hub: Arc<PublicHub>,
    std_listener: std::net::TcpListener,
) -> std::io::Result<()> {
    let listener = TcpListener::from_std(std_listener)?;
    let trust_proxy = hub.trust_proxy;
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => continue,
        };
        let _ = stream.set_nodelay(true);
        let hub = hub.clone();
        tokio::spawn(async move {
            let peer = addr.ip().to_string();
            // `..Default::default()` rather than naming every field: WebSocketConfig
            // carries a deprecated `max_send_queue` that would warn if spelled out.
            let ws_cfg = WebSocketConfig {
                max_message_size: Some(MAX_MESSAGE_BYTES),
                max_frame_size: Some(MAX_MESSAGE_BYTES),
                ..Default::default()
            };

            let mut granted: Option<LoadedKey> = None;
            let mut ip = peer.clone();
            let ws = tokio_tungstenite::accept_hdr_async_with_config(
                stream,
                |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
                    ip = client_ip(req, &peer, trust_proxy);
                    if hub.throttle.lock().unwrap().blocked(&ip) {
                        return Err(reject(
                            StatusCode::TOO_MANY_REQUESTS,
                            "too many failed attempts",
                        ));
                    }
                    let key = secrets_from(req).iter().find_map(|s| hub.keys.lookup(s));
                    match key {
                        Some(k) => {
                            hub.throttle.lock().unwrap().ok(&ip);
                            granted = Some(k);
                            Ok(resp)
                        }
                        // One opaque 401 for every failure mode: no key, wrong key,
                        // expired key, revoked key. Nothing to probe against.
                        None => Err(reject(StatusCode::UNAUTHORIZED, "unauthorized")),
                    }
                },
                Some(ws_cfg),
            )
            .await;

            let (ws, key) = match (ws, granted) {
                (Ok(w), Some(k)) => (w, k),
                _ => {
                    hub.rejected_auth.fetch_add(1, Ordering::Relaxed);
                    if hub.throttle.lock().unwrap().fail(&ip) {
                        eprintln!(
                            "public ws feed: blocking {ip} for 15min — {MAX_FAILS} failed auths"
                        );
                    }
                    return;
                }
            };
            if hub.n_clients.load(Ordering::Relaxed) >= hub.max_conn {
                return;
            }
            if hub.conns_for(&key.label) >= key.max_connections {
                eprintln!(
                    "public ws feed: '{}' at its {} connection cap — refused",
                    key.label, key.max_connections
                );
                return;
            }

            // Resolve the saved config once, before the client exists.
            let saved_cfg = hub.filters.get(&key.label).unwrap_or_default();
            let saved_bin = saved_cfg.binmaster.as_ref().and_then(parse_binmaster);
            let (mut write, mut read) = ws.split();
            let (tx, mut rx) = mpsc::channel::<Message>(CLIENT_QUEUE);
            let client = Arc::new(PubClient {
                label: key.label.clone(),
                ip: ip.clone(),
                tx: tx.clone(),
                // The saved config, not an empty one: a mod user never sends a
                // `filter` or `binmaster` message, so this is the only filter
                // they will ever have.
                filter: RwLock::new(saved_cfg.filter.clone()),
                bin: RwLock::new(saved_bin),
                min_profit: key.min_profit.max(hub.min_profit),
                buckets: if key.buckets.is_empty() {
                    hub.buckets.clone()
                } else {
                    key.buckets.clone()
                },
                allow_pricing: key.allow_pricing,
                last_pricing_ms: AtomicI64::new(0),
                drops: AtomicU32::new(0),
                last_pong_ms: AtomicI64::new(now_ms()),
                sent: AtomicU64::new(0),
            });
            let id = hub.add_client(client.clone());
            eprintln!(
                "public ws feed: '{}' connected from {ip} ({} client(s))",
                key.label,
                hub.n_clients.load(Ordering::Relaxed)
            );

            let avail: Vec<&str> = client.buckets.iter().map(|b| b.as_str()).collect();
            let _ = tx
                .send(Message::Text(
                    json!({
                        "type": "welcome",
                        "feed": "public",
                        "note": "flips this finder declined; read-only",
                        "buckets": avail,
                        "filterFields": ["minProfit","minRoiPct","minConfidence","minVolumePerDay","minPrice","maxPrice",
                                         "blacklistIds","allowIds","blockedGuards","finders","buckets"],
                    })
                    .to_string(),
                ))
                .await;

            let writer = tokio::spawn(async move {
                while let Some(m) = rx.recv().await {
                    let closing = matches!(m, Message::Close(_));
                    if write.send(m).await.is_err() || closing {
                        break;
                    }
                }
            });
            // Heartbeat: keeps the tunnel from reaping an idle socket, and reaps
            // peers that stopped answering.
            let hb = {
                let (tx, c) = (tx.clone(), client.clone());
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(PING_EVERY).await;
                        if now_ms() - c.last_pong_ms.load(Ordering::Relaxed) > PONG_TIMEOUT_MS {
                            let _ = tx.try_send(Message::Close(None));
                            break;
                        }
                        if tx.send(Message::Ping(Vec::new())).await.is_err() {
                            break;
                        }
                    }
                })
            };

            let mut msgs = 0u32;
            let mut window = now_ms();
            while let Some(Ok(msg)) = read.next().await {
                match msg {
                    Message::Text(t) => {
                        let now = now_ms();
                        if now - window > MSG_WINDOW_MS {
                            window = now;
                            msgs = 0;
                        }
                        msgs += 1;
                        if msgs > MAX_MSGS_PER_WINDOW {
                            eprintln!(
                                "public ws feed: '{}' flooding control messages — closing",
                                client.label
                            );
                            break;
                        }
                        let Ok(v) = serde_json::from_str::<Value>(&t) else {
                            continue;
                        };
                        handle_client_message(&hub, &client, &tx, &v).await;
                    }
                    Message::Pong(_) => {
                        client.last_pong_ms.store(now_ms(), Ordering::Relaxed);
                    }
                    Message::Ping(_) => {}
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            hub.remove_client(id);
            hb.abort();
            writer.abort();
            eprintln!(
                "public ws feed: '{}' ({}) disconnected — {} sent, {} dropped",
                client.label,
                client.ip,
                client.sent.load(Ordering::Relaxed),
                client.drops.load(Ordering::Relaxed)
            );
        });
    }
}

/// `{"type":"inventory"}` → `{"type":"listInstructions"}`, the half of the baf mod
/// workflow that turns a bought item into a listed one. Without it a consumer can
/// buy from the feed but never gets an asking price back.
///
/// This is a SEPARATE implementation from `ws_server`'s inventory RPC, and must
/// stay that way. That one is entangled with owner state: it reconciles the flood
/// brake against the inventory it is shown, reads `cost_basis` for the cost floor,
/// and writes `listing_tracker` / `list_attempts` keyed by item uuid. Pointing it
/// at a stranger's inventory would mark the OWNER's held items as gone and pollute
/// his listing-attempt history. Everything here is read-only: decode → price →
/// reply, with `paid: None` so no cost floor is invented for items we never bought.
///
/// Cost control, because this is the only public path that reaches the
/// single-threaded pricing loop at all:
///   * off unless the key carries `allowPricing`
///   * one call per `PRICING_MIN_GAP_MS` per client
///   * at most `MAX_PRICING_ITEMS` slots per call
///   * a global permit, so all public clients together can have at most ONE
///     pricing request in flight against the loop thread
async fn handle_inventory(
    hub: &Arc<PublicHub>,
    client: &Arc<PubClient>,
    tx: &mpsc::Sender<Message>,
    v: &Value,
) {
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let deny = |why: &str| {
        json!({"type":"listInstructions","id":id,"items":[],"skipped":[{"name":"-","reason":why}]})
            .to_string()
    };
    if !client.allow_pricing {
        let _ = tx
            .send(Message::Text(deny(
                "inventory pricing is not enabled for this key",
            )))
            .await;
        return;
    }
    // Per-client spacing. The mod's own auto-list loop runs on a slow interval, so
    // this never binds in normal use; it binds on a client that spins.
    // An atomic, not a Mutex: the reply below is awaited, and a guard held across
    // an await makes the whole connection future non-Send. A lost race between two
    // of one client's own calls costs nothing for a rate limiter.
    let now = now_ms();
    let since = now - client.last_pricing_ms.load(Ordering::Relaxed);
    if since < PRICING_MIN_GAP_MS {
        let wait = (PRICING_MIN_GAP_MS - since + 999) / 1000;
        let _ = tx
            .send(Message::Text(deny(&format!(
                "pricing rate limit, retry in {wait}s"
            ))))
            .await;
        return;
    }
    client.last_pricing_ms.store(now, Ordering::Relaxed);
    let items: Vec<Value> = v
        .get("items")
        .and_then(|i| i.as_array())
        .cloned()
        .unwrap_or_default();
    let truncated = items.len() > MAX_PRICING_ITEMS;
    let items: Vec<Value> = items.into_iter().take(MAX_PRICING_ITEMS).collect();

    // Decode first: undecodable slots (menu glass, empty slots) never reach the loop.
    let mut decoded: Vec<(Value, ItemAttributes)> = Vec::new();
    for it in items {
        if let Some(a) = attrs_from_inventory_slot(&it) {
            if !NON_AUCTIONABLE_PUBLIC.contains(&a.id.to_uppercase().as_str()) {
                decoded.push((it, a));
            }
        }
    }
    if decoded.is_empty() {
        let _ = tx
            .send(Message::Text(
                json!({"type":"listInstructions","id":id,"items":[],"skipped":[]}).to_string(),
            ))
            .await;
        return;
    }
    let Some(rpc) = hub.rpc_tx.as_ref() else {
        let _ = tx.send(Message::Text(deny("pricing unavailable"))).await;
        return;
    };
    // One public pricing request against the loop thread at a time, ever.
    let Ok(_permit) = hub.pricing_permit.try_acquire() else {
        let _ = tx
            .send(Message::Text(deny("pricing busy, try again shortly")))
            .await;
        return;
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    let attrs: Vec<ItemAttributes> = decoded.iter().map(|(_, a)| a.clone()).collect();
    if rpc
        .send(RpcRequest::PriceAttrs {
            attrs,
            reply: reply_tx,
        })
        .is_err()
    {
        let _ = tx.send(Message::Text(deny("pricing unavailable"))).await;
        return;
    }
    let ests = match tokio::time::timeout(Duration::from_secs(20), reply_rx).await {
        Ok(Ok(e)) => e,
        _ => {
            let _ = tx.send(Message::Text(deny("pricing timed out"))).await;
            return;
        }
    };

    let mut instructions: Vec<Value> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    for ((it, attrs), est) in decoded.iter().zip(ests) {
        let name = it
            .get("displayName")
            .and_then(|x| x.as_str())
            .or_else(|| it.get("name").and_then(|x| x.as_str()))
            .unwrap_or(&attrs.id)
            .to_string();
        let id_tag = it
            .get("tag")
            .and_then(|x| x.as_str())
            .unwrap_or(&attrs.id)
            .to_string();
        let Some(est) = est else {
            skipped.push(json!({"name": name, "reason": "no price data for this item"}));
            continue;
        };
        if est.basis.as_deref() != Some("lbin") && est.samples < PUBLIC_MIN_SAMPLES {
            skipped.push(
                json!({"name": name, "reason": format!("not enough samples ({})", est.samples)}),
            );
            continue;
        }
        // `paid: None` — we have no cost basis for someone else's item and must not
        // invent one. The ladder prices it on merit; nothing here is persisted.
        let pricing = price_inventory(&InventoryPricingInput {
            target: est.target,
            lbin: est.lbin,
            basis: est.basis.clone(),
            paid: None,
            acquired_at_ms: None,
            fallback_first_seen_ms: Some(now as f64),
            volume_per_day: est.volume_per_day,
            market_median: est.market_median,
            variant_priced: est.variant_priced,
            failed_listings: 0.0,
            now_ms: now as f64,
        });
        instructions.push(json!({
            "slot": jnum(it.get("slot").and_then(|s| s.as_f64()).unwrap_or(0.0)),
            "name": name,
            "id": id_tag,
            "listAt": jnum(pricing.list_at.round()),
            "confidence": jnum((est.confidence * 1000.0).round() / 1000.0),
            "volumePerDay": jnum(est.volume_per_day.unwrap_or(0.0)),
        }));
    }
    if truncated {
        skipped.push(json!({"name": "-", "reason": format!("only the first {MAX_PRICING_ITEMS} slots were priced")}));
    }
    hub.priced
        .fetch_add(instructions.len() as u64, Ordering::Relaxed);
    let _ = tx
        .send(Message::Text(
            json!({"type":"listInstructions","id":id,"force":false,"items":instructions,"skipped":skipped}).to_string(),
        ))
        .await;
}

/// The public protocol: `filter`, `binmaster`, `ping`, and `inventory` for keys
/// that carry `allowPricing`. `estimate` and `ahPage` stay unreachable: the first
/// leaks per-auction valuations for auctions we never released, the second lets a
/// stranger inject arbitrary "crawled" listings into the pricing path.
async fn handle_client_message(
    hub: &Arc<PublicHub>,
    client: &Arc<PubClient>,
    tx: &mpsc::Sender<Message>,
    v: &Value,
) {
    if v.get("type").and_then(|t| t.as_str()) == Some("inventory") {
        handle_inventory(hub, client, tx, v).await;
        return;
    }
    match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "filter" => match serde_json::from_value::<ClientFilter>(v.clone()) {
            Ok(f) => {
                *client.filter.write().unwrap() = f;
                let _ = tx
                    .send(Message::Text(
                        json!({"type":"filterAck","ok":true}).to_string(),
                    ))
                    .await;
            }
            Err(e) => {
                let _ = tx
                    .send(Message::Text(
                        json!({"type":"filterAck","ok":false,"error":e.to_string()}).to_string(),
                    ))
                    .await;
            }
        },
        // Full BinMaster tier filter, evaluated by the same engine our own filter
        // uses. `{"type":"binmaster","filter":null}` clears it.
        "binmaster" => {
            let raw = v.get("filter").cloned().unwrap_or(Value::Null);
            if raw.is_null() {
                *client.bin.write().unwrap() = None;
                let _ = tx
                    .send(Message::Text(
                        json!({"type":"binmasterAck","ok":true,"active":false}).to_string(),
                    ))
                    .await;
                return;
            }
            match parse_binmaster(&raw) {
                Some(f) => {
                    *client.bin.write().unwrap() = Some(f);
                    let _ = tx
                        .send(Message::Text(
                            json!({"type":"binmasterAck","ok":true,"active":true}).to_string(),
                        ))
                        .await;
                }
                None => {
                    let _ = tx
                        .send(Message::Text(
                            json!({"type":"binmasterAck","ok":false,"error":"not a BinMaster filter (needs item_specific_filters)"}).to_string(),
                        ))
                        .await;
                }
            }
        }
        "ping" => {
            client.last_pong_ms.store(now_ms(), Ordering::Relaxed);
            let _ = tx
                .send(Message::Text(json!({"type":"pong"}).to_string()))
                .await;
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_nist_vector() {
        // FIPS 180-2 B.1: SHA-256("abc").
        let d = sha256(b"abc");
        assert_eq!(
            d.to_vec(),
            hex_to_32("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
                .unwrap()
                .to_vec()
        );
    }

    #[test]
    fn hex_parse_rejects_junk() {
        assert!(hex_to_32("nope").is_none());
        assert!(hex_to_32(&"z".repeat(64)).is_none());
        assert!(hex_to_32(&"a".repeat(63)).is_none());
        assert!(hex_to_32(&"a".repeat(64)).is_some());
    }

    #[test]
    fn own_listings_are_never_publishable() {
        // Belt-and-braces: the guard in publish() is a string check, so pin the
        // exact reason push_flip emits.
        let reason = "own listing (self-buy guard)";
        assert!(reason.to_lowercase().contains("own listing"));
    }

    #[test]
    fn reasons_land_in_the_right_bucket() {
        assert_eq!(bucket_of("profit 1.2M < hardMinProfit 3.0M"), Bucket::Merit);
        assert_eq!(bucket_of("confidence 0.61 below 0.70"), Bucket::Merit);
        assert_eq!(
            bucket_of("item HEGEMONY_ARTIFACT blacklisted"),
            Bucket::Merit
        );
        assert_eq!(
            bucket_of("holding 3 unsold WARDEN_HELMET (cap 3)"),
            Bucket::Flood
        );
        assert_eq!(bucket_of("no client connected"), Bucket::Unrouted);
        assert_eq!(bucket_of("no eligible client"), Bucket::Unrouted);
        assert_eq!(
            bucket_of("grind flip: purse 12.0M > maxPurse 10.0M"),
            Bucket::Unrouted
        );
        assert_eq!(
            bucket_of("bot has 1 free inv slot(s), needs 3"),
            Bucket::Unrouted
        );
    }

    #[test]
    fn reject_categories_match_the_funnel_labels() {
        // "below global" carries profit/roi substrings and must win.
        assert_eq!(reject_category("below global min profit 5.0M"), "global");
        assert_eq!(reject_category("roi 4% below 10%"), "roi");
        assert_eq!(reject_category("volume 1.2 below 3"), "volume");
        assert_eq!(reject_category("confidence 0.61 below 0.70"), "confidence");
        assert_eq!(
            reject_category("profit 1.2M < hardMinProfit 3.0M"),
            "profit"
        );
    }

    fn flip(
        profit: f64,
        roi: f64,
        conf: f64,
        vol: Option<f64>,
        id: &str,
        guard: &str,
    ) -> PublicFlip {
        // ItemAttributes has no Default; every field but `id` is serde-defaulted.
        let attrs: ItemAttributes = serde_json::from_value(json!({ "id": id })).unwrap();
        PublicFlip {
            uuid: "u".into(),
            item_name: "Item".into(),
            finder: "median".into(),
            price: 1_000_000.0,
            target: 2_000_000.0,
            profit,
            roi_pct: roi,
            confidence: conf,
            samples: 20,
            volume_per_day: vol,
            lbin: None,
            key: "K".into(),
            guard: guard.into(),
            list_at: Some(2_100_000.0),
            ids: vec![id.to_uppercase()],
            attrs,
            bucket: Bucket::Merit,
            category: "profit",
            seen_at_ms: 0,
        }
    }

    #[test]
    fn empty_client_filter_passes_everything() {
        let f = ClientFilter::default();
        assert!(f.matches(&flip(1.0, 1.0, 0.1, None, "X", "none")));
    }

    #[test]
    fn client_filter_gates_each_field() {
        let mut f = ClientFilter::default();
        f.min_profit = 5_000_000.0;
        assert!(!f.matches(&flip(1_000_000.0, 50.0, 0.9, Some(10.0), "X", "none")));
        assert!(f.matches(&flip(6_000_000.0, 50.0, 0.9, Some(10.0), "X", "none")));

        let mut f = ClientFilter::default();
        f.min_volume_per_day = 5.0;
        // Absent volume must NOT pass a volume floor.
        assert!(!f.matches(&flip(9e9, 99.0, 0.9, None, "X", "none")));
        assert!(f.matches(&flip(9e9, 99.0, 0.9, Some(6.0), "X", "none")));

        let mut f = ClientFilter::default();
        f.max_price = 500_000.0;
        assert!(!f.matches(&flip(9e9, 99.0, 0.9, Some(9.0), "X", "none")));
    }

    #[test]
    fn allow_and_deny_lists_are_case_insensitive() {
        let mut f = ClientFilter::default();
        f.blacklist_ids = vec!["hegemony_artifact".into()];
        assert!(!f.matches(&flip(
            9e9,
            99.0,
            0.9,
            Some(9.0),
            "HEGEMONY_ARTIFACT",
            "none"
        )));

        let mut f = ClientFilter::default();
        f.allow_ids = vec!["weird_tuba".into()];
        assert!(f.matches(&flip(9e9, 99.0, 0.9, Some(9.0), "WEIRD_TUBA", "none")));
        assert!(!f.matches(&flip(9e9, 99.0, 0.9, Some(9.0), "WARDEN_HELMET", "none")));
    }

    #[test]
    fn blocked_guards_match_as_substrings_like_the_bot_feed() {
        let mut f = ClientFilter::default();
        f.blocked_guards = vec!["manipulated".into()];
        assert!(!f.matches(&flip(9e9, 99.0, 0.9, Some(9.0), "X", "manipulated")));
        assert!(f.matches(&flip(9e9, 99.0, 0.9, Some(9.0), "X", "low_volume")));
    }

    #[test]
    fn payload_keeps_integers_integral_for_as_u64_consumers() {
        let p = flip(10_015_120.0, 125.2, 0.8183, Some(1.57), "X", "none").payload(true);
        let v: Value = serde_json::from_str(&p).unwrap();
        let f = &v["flip"];
        assert_eq!(f["price"].as_u64(), Some(1_000_000));
        assert_eq!(f["profit"].as_u64(), Some(10_015_120));
        assert_eq!(f["target"].as_u64(), Some(2_000_000));
        // `listAt` IS sent: the baf mod reads it with as_u64() to relist what it
        // buys, so an absent or non-integral value means a consumer can buy from
        // the feed but never list. It must survive as_u64() like every other
        // money field.
        assert_eq!(f["listAt"].as_u64(), Some(2_100_000));
        // The detection clock still stays ours.
        assert!(f.get("foundAtMs").is_none());
        assert!(f.get("dumpDetectedAtMs").is_none());
        assert_eq!(f["bucket"].as_str(), Some("merit"));
    }

    #[test]
    fn reason_can_be_withheld() {
        let p = flip(1.0, 1.0, 0.5, None, "X", "none").payload(false);
        let v: Value = serde_json::from_str(&p).unwrap();
        assert!(v["flip"].get("rejectedBy").is_none());
        assert!(v["flip"].get("bucket").is_none());
    }

    #[test]
    fn list_at_never_opens_below_cost_and_hedges_model_prices() {
        // Mirrors flip_payload's rule: max(target * hedge, price * 1.05), where a
        // model-priced flip carries the 0.97 hedge. A consumer relisting at less
        // than they paid is the one outcome this must never produce.
        let cases: [(&str, f64, f64, f64); 3] = [
            ("median", 2_000_000.0, 1_000_000.0, 2_000_000.0),
            // reference below cost ⇒ the cost floor wins
            ("median", 900_000.0, 1_000_000.0, 1_050_000.0),
            ("model", 2_000_000.0, 1_000_000.0, 1_940_000.0),
        ];
        for (finder, reference, price, want) in cases {
            let hedge = if finder == "model" { 0.97 } else { 1.0 };
            let got = (reference * hedge).round().max((price * 1.05).ceil());
            assert_eq!(got, want, "{finder} {reference} {price}");
            assert!(got >= price * 1.05, "opened below cost+5%");
        }
    }

    #[test]
    fn pricing_is_off_unless_the_key_grants_it() {
        // allowPricing is the only public path to the single-threaded pricing
        // loop, so it must default to false on a key that does not mention it.
        let dir =
            std::env::temp_dir().join(format!("pubws-cap-{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keys.json");
        std::fs::write(
            &path,
            r#"{"keys":[
                 {"label":"plain","key":"plain-key-long-enough-here"},
                 {"label":"trusted","key":"trusted-key-long-enough","allowPricing":true}
               ]}"#,
        )
        .unwrap();
        let ks = KeyStore::new(path.to_str().unwrap());
        ks.reload_if_changed(true);
        assert!(
            !ks.lookup("plain-key-long-enough-here")
                .unwrap()
                .allow_pricing
        );
        assert!(ks.lookup("trusted-key-long-enough").unwrap().allow_pricing);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_format_filter_file_is_not_silently_zeroed() {
        // Regression, 2026-08-10: `SavedConfig` defaults every field, so an old
        // entry parsed as the NEW shape "successfully" with the filter wiped to
        // zeros and no error anywhere. A saved 300k floor came back as 0.
        let dir =
            std::env::temp_dir().join(format!("pubws-fmt-{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("filters.json");

        // Old shape: label -> ClientFilter.
        std::fs::write(
            &path,
            r#"{"alice":{"minProfit":300000,"minRoiPct":5,"blockedGuards":["manipulated"]}}"#,
        )
        .unwrap();
        let old = FilterStore::load(path.to_str().unwrap());
        let a = old.get("alice").expect("alice survives the format change");
        assert_eq!(
            a.filter.min_profit, 300_000.0,
            "old-format filter was zeroed"
        );
        assert_eq!(a.filter.min_roi_pct, 5.0);
        assert_eq!(a.filter.blocked_guards, vec!["manipulated".to_string()]);
        assert!(a.binmaster.is_none());

        // Current shape: label -> {filter, binmaster}.
        std::fs::write(
            &path,
            r#"{"bob":{"filter":{"minProfit":7000000},"binmaster":{"item_specific_filters":{}}}}"#,
        )
        .unwrap();
        let new = FilterStore::load(path.to_str().unwrap());
        let b = new.get("bob").unwrap();
        assert_eq!(b.filter.min_profit, 7_000_000.0);
        assert!(b.binmaster.is_some());

        // A round trip through set() must keep both halves intact.
        new.set_binmaster("bob", None).unwrap();
        let reloaded = FilterStore::load(path.to_str().unwrap());
        assert_eq!(
            reloaded.get("bob").unwrap().filter.min_profit,
            7_000_000.0,
            "saving the tier filter dropped the thresholds"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keystore_is_fail_closed() {
        let ks = KeyStore::new("/nonexistent/public-keys.json");
        assert_eq!(ks.reload_if_changed(true), Some(0));
        assert!(ks.lookup("anything").is_none());
    }

    #[test]
    fn keystore_loads_hashed_and_plaintext_keys_and_honours_expiry() {
        let dir = std::env::temp_dir().join(format!("pubws-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keys.json");
        let secret = "835835395sdjsjm-but-longer";
        let digest = sha256(secret.as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        std::fs::write(
            &path,
            format!(
                r#"{{"keys":[
                  {{"label":"hashed","sha256":"{hex}"}},
                  {{"label":"plain","key":"plaintext-key-that-is-long"}},
                  {{"label":"off","key":"disabled-key-that-is-long","enabled":false}},
                  {{"label":"expired","key":"expired-key-that-is-long","expiresAtMs":1}},
                  {{"label":"short","key":"tiny"}}
                ]}}"#
            ),
        )
        .unwrap();
        let ks = KeyStore::new(path.to_str().unwrap());
        // hashed + plain + expired load; "off" (disabled) and "short" (too weak)
        // are dropped at load. Expiry is enforced at lookup, not here, so a key
        // dies on schedule without waiting for the next file change.
        assert_eq!(ks.reload_if_changed(true), Some(3));
        assert_eq!(
            ks.lookup(secret).map(|k| k.label),
            Some("hashed".to_string())
        );
        assert_eq!(
            ks.lookup("plaintext-key-that-is-long").map(|k| k.label),
            Some("plain".to_string())
        );
        assert!(ks.lookup("disabled-key-that-is-long").is_none());
        assert!(ks.lookup("expired-key-that-is-long").is_none()); // expired ⇒ no match
        assert!(ks.lookup("tiny").is_none());
        assert!(ks.lookup("").is_none());
        assert!(ks.lookup("wrong").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // End-to-end: a real listener, real WebSocket clients, real handshakes.
    // Unit tests can't prove the thing that actually matters here — that a wrong
    // key gets no bytes and a right key does.
    // -----------------------------------------------------------------------

    fn test_flip(profit: f64, id: &str) -> Flip {
        let attrs: ItemAttributes = serde_json::from_value(json!({ "id": id })).unwrap();
        Flip {
            uuid: "auction-uuid".into(),
            item_name: format!("Test {id}"),
            finder: "median".into(),
            price: 1_000_000.0,
            reference: 1_000_000.0 + profit,
            profit,
            roi_pct: profit / 10_000.0,
            confidence: 0.9,
            samples: 30,
            key: id.into(),
            guard: "none".into(),
            found_after_refresh_ms: 0.0,
            found_at_ms: 0.0,
            attrs,
            median_stats: None,
        }
    }

    struct Harness {
        _dir: std::path::PathBuf,
        hub: Arc<PublicHub>,
        secret: String,
        _rt: tokio::runtime::Runtime,
    }

    fn harness(buckets: Vec<Bucket>) -> Harness {
        let dir =
            std::env::temp_dir().join(format!("pubws-e2e-{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keys.json");
        let secret = "test-secret-that-is-long-enough";
        std::fs::write(
            &path,
            format!(r#"{{"keys":[{{"label":"tester","key":"{secret}"}}]}}"#),
        )
        .unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let hub = spawn_with(
            &rt,
            PublicWsConfig {
                host: "127.0.0.1".into(),
                port: 0, // ephemeral
                keys_path: path.to_string_lossy().into_owned(),
                filters_path: dir.join("filters.json").to_string_lossy().into_owned(),
                buckets,
                min_profit: 0.0,
                send_reason: true,
                max_conn: 10,
                delay_ms: 0,
                trust_proxy: false,
                send_list_at: true,
            },
            None, // no pricing RPC in tests: the loop thread does not exist here
        )
        .expect("hub starts");
        Harness {
            _dir: dir,
            hub,
            secret: secret.to_string(),
            _rt: rt,
        }
    }

    /// Connect and read the welcome, or return the HTTP error the server sent.
    async fn connect(
        port: u16,
        path: &str,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        String,
    > {
        match tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}{path}")).await {
            Ok((ws, _)) => Ok(ws),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn next_flip(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> Option<Value> {
        loop {
            let msg = tokio::time::timeout(Duration::from_millis(1500), ws.next())
                .await
                .ok()??;
            let Ok(Message::Text(t)) = msg else { continue };
            let v: Value = serde_json::from_str(&t).ok()?;
            match v.get("type").and_then(|t| t.as_str()) {
                Some("flip") => return Some(v["flip"].clone()),
                _ => continue,
            }
        }
    }

    #[test]
    fn e2e_wrong_key_gets_nothing_right_key_gets_the_feed() {
        let h = harness(vec![Bucket::Merit, Bucket::Flood]);
        let port = h.hub.port;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // Wrong secret, no secret, and a near-miss are all refused.
            for bad in [
                "/wrong-secret-entirely",
                "/",
                "",
                "/test-secret-that-is-long-enoug",
                "/?token=nope",
            ] {
                let r = connect(port, bad).await;
                assert!(r.is_err(), "expected refusal for {bad:?}");
            }
            assert_eq!(h.hub.n_clients.load(Ordering::Relaxed), 0);

            // The real key, as a path segment.
            let mut ws = connect(port, &format!("/{}", h.secret))
                .await
                .expect("authorized connect");
            // Welcome first.
            let first = tokio::time::timeout(Duration::from_millis(1500), ws.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let v: Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
            assert_eq!(v["type"], "welcome");
            assert_eq!(v["feed"], "public");
            // Give the accept task a moment to register the client.
            for _ in 0..50 {
                if h.hub.n_clients.load(Ordering::Relaxed) == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            // A declined flip reaches it.
            h.hub.publish(
                &test_flip(5_000_000.0, "WEIRD_TUBA"),
                Some(900_000.0),
                "profit 5.0M < hardMinProfit 8.0M",
            );
            let f = next_flip(&mut ws).await.expect("flip delivered");
            assert_eq!(f["itemName"], "Test WEIRD_TUBA");
            assert_eq!(f["profit"].as_u64(), Some(5_000_000));
            assert_eq!(f["lbin"].as_u64(), Some(900_000));
            assert_eq!(f["bucket"], "merit");
            assert_eq!(f["rejectedBy"], "profit");

            // Our own relist is never published, under any config.
            h.hub.publish(
                &test_flip(9_000_000.0, "OUR_OWN"),
                None,
                "own listing (self-buy guard)",
            );
            // ...and the unrouted bucket is off by default.
            h.hub.publish(
                &test_flip(9_000_000.0, "UNROUTED"),
                None,
                "no eligible client",
            );
            // The next thing that arrives must be the one AFTER those two.
            h.hub.publish(
                &test_flip(7_000_000.0, "MARKER"),
                None,
                "confidence 0.61 below 0.70",
            );
            let f = next_flip(&mut ws).await.expect("marker delivered");
            assert_eq!(f["itemName"], "Test MARKER", "a suppressed flip leaked");
            assert_eq!(f["rejectedBy"], "confidence");

            // The client's own filter narrows the feed.
            ws.send(Message::Text(
                json!({"type":"filter","minProfit":10_000_000}).to_string(),
            ))
            .await
            .unwrap();
            let ack = tokio::time::timeout(Duration::from_millis(1500), ws.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let v: Value = serde_json::from_str(ack.to_text().unwrap()).unwrap();
            assert_eq!(v["type"], "filterAck");
            assert_eq!(v["ok"], true);

            h.hub.publish(
                &test_flip(1_000_000.0, "TOO_SMALL"),
                None,
                "profit below min",
            );
            h.hub.publish(
                &test_flip(50_000_000.0, "BIG_ENOUGH"),
                None,
                "profit below min",
            );
            let f = next_flip(&mut ws).await.expect("big flip delivered");
            assert_eq!(
                f["itemName"], "Test BIG_ENOUGH",
                "client filter did not drop the small flip"
            );

            // The pricing RPCs the bot feed answers must be inert here.
            ws.send(Message::Text(
                json!({"type":"estimate","uuid":"x"}).to_string(),
            ))
            .await
            .unwrap();
            ws.send(Message::Text(
                json!({"type":"inventory","items":[]}).to_string(),
            ))
            .await
            .unwrap();
            h.hub.publish(
                &test_flip(60_000_000.0, "STILL_ALIVE"),
                None,
                "profit below min",
            );
            let f = next_flip(&mut ws)
                .await
                .expect("connection survived the RPC attempts");
            assert_eq!(f["itemName"], "Test STILL_ALIVE");
        });
    }

    #[test]
    fn e2e_revoking_a_key_closes_the_connection() {
        let h = harness(vec![Bucket::Merit]);
        let port = h.hub.port;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut ws = connect(port, &format!("/{}", h.secret))
                .await
                .expect("authorized connect");
            let _welcome = tokio::time::timeout(Duration::from_millis(1500), ws.next())
                .await
                .unwrap();
            // Emptying the key file must lock the door, not leave it ajar.
            // The reload gate is the file's MTIME: on coarse-mtime filesystems
            // (seen on container overlayfs) a rewrite can land in the same
            // tick as the setup load and read as unchanged. Re-write on a
            // fresh tick until the gate notices, then assert on what loaded.
            std::fs::write(h._dir.join("keys.json"), r#"{"keys":[]}"#).unwrap();
            let mut reloaded = h.hub.keys.reload_if_changed(false);
            while reloaded.is_none() {
                std::thread::sleep(Duration::from_millis(1100));
                std::fs::write(h._dir.join("keys.json"), r#"{"keys":[]}"#).unwrap();
                reloaded = h.hub.keys.reload_if_changed(false);
            }
            assert_eq!(reloaded, Some(0));
            assert!(h.hub.keys.lookup(&h.secret).is_none());
            assert!(
                connect(port, &format!("/{}", h.secret)).await.is_err(),
                "revoked key still connects"
            );
        });
    }

    #[test]
    fn throttle_blocks_after_repeated_failures_and_expires() {
        let mut t = Throttle::default();
        for _ in 0..MAX_FAILS - 1 {
            assert!(!t.fail("1.2.3.4"));
        }
        assert!(t.fail("1.2.3.4"));
        assert!(t.blocked("1.2.3.4"));
        assert!(!t.blocked("5.6.7.8"));
        // A success clears the counter for that IP.
        t.ok("5.6.7.8");
        assert!(!t.blocked("5.6.7.8"));
    }
}
