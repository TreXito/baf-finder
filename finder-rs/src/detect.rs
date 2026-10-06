//! detect.rs — the low-latency detect half, merged from `finder-shadow-rs`
//! (Phase 1) into the real finder.
//!
//! This is the port of detectWorker.ts + pageWorker.ts's page-0 half: N poll
//! lanes race conditional GETs against page 0, one lane wins the single-streamer
//! claim (CAS) and drains the body, and NEW BINs are decoded **as the bytes
//! arrive** — the same overlap prod gets by decoding inside its page workers
//! while the fetch is still streaming (`ffDecAheadMs`).
//!
//! Money-safety boundary: lanes NEVER touch the PriceIndex or the model (they
//! aren't `Sync`, and pricing must stay on one thread). A lane only does work
//! that needs no index state: detect → stream → JSON extract → NBT decode.
//! Keying, screening, evaluation and pushing all happen on the loop thread,
//! which receives `Dump`s over a channel. This mirrors prod's split, where the
//! worker decodes and the main thread prices.
//!
//! The new-BIN screen uses the previous FULL sweep's live-BIN uuid set
//! (prod's `prevLiveUuids`, index.ts:608/867), published here by the loop
//! thread after each sweep — NOT page-0's own uuids, which would flag any BIN
//! that merely moved between pages as "new" and re-flip it every dump.

use finder_core::nbt::{decode_item_bytes, ItemAttributes};
use finder_core::sniper::ActiveAuction;
use std::collections::HashSet;
use std::io::Write;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BASE: &str = "https://api.hypixel.net/v2/skyblock/auctions?page=0";
const POLL_TIMEOUT_MS: u64 = 400;
const RATE_LIMIT_BACKOFF_MS: i64 = 2_000;
/// Backoff for a Cloudflare WAF block (403). Far longer than the 429 backoff:
/// a 429 is "slow down for a moment", a 403 is "you are blocked", and it is
/// served from the edge in ~20ms. Retrying that at the poll rate is a hundreds
/// -per-second request storm aimed at the thing that is already blocking us,
/// which is exactly what kept the 2026-07-27 block alive for 90 minutes.
const FORBIDDEN_BACKOFF_MS: i64 = 60_000;
/// Observed publish cadence of the auctions dump.
///
/// Measured 2026-07-27 from an unblocked host, 2640 conditional polls over 11
/// minutes: 11 consecutive publishes, interval min 60.0s / max 60.0s / spread
/// **0.0s**, every one landing at exactly **:31s past the minute**. It is a
/// metronome, not a rough cadence, which is what makes a narrow burst window
/// safe. Every publish we witness re-anchors it, so a clock shift corrects
/// itself within a cycle.
const PUBLISH_INTERVAL_MS: i64 = 60_000;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Startup phase offset for a poll lane.
///
/// Each lane's loop sleeps `poll_delay - elapsed`, so its period is EXACTLY
/// `poll_delay` and its phase is whatever it happened to be when the lane
/// started. `spawn_lanes` starts every lane in one tight loop, so the phases were
/// decided purely by how the initial DNS/TCP/TLS handshakes happened to land, and
/// then frozen for the life of the process.
///
/// That makes detection granularity a lottery between `poll_delay / lanes`
/// (perfectly spread) and `poll_delay` (all lanes in phase, i.e. 4 requests fired
/// at the same instant and then 100ms of silence). Nothing chose which.
///
/// Spreading the lanes deliberately makes the granularity DETERMINISTIC at
/// `poll_delay / lanes` for the same number of requests. It is free: no extra
/// polling, just a one-off offset before each lane's first request.
pub fn lane_stagger(lane: usize, lanes: usize, poll_delay: Duration) -> Duration {
    if lanes <= 1 {
        return Duration::ZERO;
    }
    (poll_delay * lane as u32) / lanes as u32
}

/// When to poll hard and when to coast.
///
/// Detection granularity is `poll_delay / lanes`, i.e. 1/request-rate — but ONLY
/// if the lanes are phase-spread, which they were not until `lane_stagger`
/// existed. Splitting the same rate across more lanes buys nothing on its own;
/// the only ways to see a publish sooner are more requests per second, or
/// spending the requests you already send evenly. Polling flat out is
/// what got the box Cloudflare-blocked on 2026-07-27 (~427 req/s).
///
/// But the dump does not publish at random: it lands on a ~60s clock, and every
/// publish we witness re-anchors the prediction. So we only need to be fast in a
/// short window around the next expected publish, and can coast the rest of the
/// minute. That keeps the granularity that matters while cutting total requests
/// by ~40x.
#[derive(Clone, Copy, Debug)]
pub struct PollSchedule {
    /// Per-lane delay inside the burst window.
    pub burst_ms: u64,
    /// Per-lane delay outside it.
    pub idle_ms: u64,
    /// Start bursting this long before the predicted publish.
    pub lead_ms: i64,
    /// Keep bursting this long after it, to absorb jitter.
    pub tail_ms: i64,
    /// If a publish is this overdue, stop bursting. Something is wrong (the API
    /// stalled, or we are blocked) and a stuck fast loop is exactly the storm
    /// this is meant to prevent.
    pub giveup_ms: i64,
}

impl Default for PollSchedule {
    fn default() -> Self {
        // 4 lanes at 8ms = 500 req/s inside the window: a 2ms granularity, i.e.
        // strictly FASTER than the ~427 req/s always-on schedule this replaces.
        // The point is not to be slower, it is to stop paying that rate for the
        // other 57 seconds of every minute.
        //
        // +/-1.5s of window against a measured jitter of 0.0s is a wide margin.
        // 3s of burst plus a 4s idle poll averages ~26 req/s, sixteen times under
        // the ~427 req/s that got every box IP blocked — and with LOCAL_IPS
        // rotation that is ~126 req/min per IP.
        PollSchedule {
            burst_ms: 8,
            idle_ms: 4_000,
            lead_ms: 1_500,
            tail_ms: 1_500,
            giveup_ms: 30_000,
        }
    }
}

/// Per-lane delay before the next poll, given the predicted publish time.
///
/// `next_publish == 0` means we have not witnessed a publish yet and do not know
/// the phase, so we burst until we do.
pub fn poll_delay_for(now_ms: i64, next_publish_ms: i64, s: &PollSchedule) -> Duration {
    if next_publish_ms == 0 {
        return Duration::from_millis(s.burst_ms);
    }
    let offset = now_ms - next_publish_ms;
    let in_window = offset >= -s.lead_ms && offset <= s.tail_ms;
    // Overdue but not yet hopeless: the publish is late, keep looking hard.
    let overdue = offset > s.tail_ms && offset <= s.giveup_ms;
    if in_window || overdue {
        Duration::from_millis(s.burst_ms)
    } else {
        Duration::from_millis(s.idle_ms)
    }
}

/// How long every lane should pause after this response status, if at all.
///
/// `None` means "keep polling": 200 is the normal path, and 304 is the cheap
/// not-modified answer we expect the overwhelming majority of the time.
fn backoff_ms_for_status(status: u16) -> Option<i64> {
    match status {
        429 => Some(RATE_LIMIT_BACKOFF_MS),
        // Cloudflare's WAF block. Long, because it does not clear while we
        // keep hitting it.
        403 => Some(FORBIDDEN_BACKOFF_MS),
        _ => None,
    }
}

/// Parse an HTTP-date Last-Modified header to epoch ms (0 on failure).
fn lm_to_ms(s: &str) -> i64 {
    httpdate::parse_http_date(s)
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Page-0 auction as it appears on the wire. Mirrors hypixel::RawAuction but is
/// deserialized from a span inside the streaming body rather than a whole page.
#[derive(serde::Deserialize)]
struct WireAuction {
    uuid: String,
    #[serde(default)]
    auctioneer: Option<String>,
    #[serde(default)]
    bin: bool,
    #[serde(default)]
    starting_bid: f64,
    #[serde(default)]
    item_name: String,
    #[serde(default)]
    item_bytes: String,
    /// Listing start (epoch ms). Feeds the B1 TTS capture (index.ts:534).
    #[serde(default)]
    start: f64,
}

/// A NEW BIN, decoded in-lane while the body was still streaming. The `key` is
/// NOT set here: keying needs the PriceIndex, which lives on the loop thread.
pub struct DetectedBin {
    pub a: ActiveAuction,
    pub attrs: ItemAttributes,
    /// Listing start (epoch ms), carried for the TTS capture.
    pub start: f64,
}

/// A page-0 dump, streamed to the loop thread in three parts.
///
/// The whole point of the split is WHEN eval starts. Prod evaluates new BINs as
/// the pages download (index.ts:433), so its first flip fires mid-body — that is
/// the entire reason prod's `firstFlipMs` is 44-55ms and not drain-length. Handing
/// over one finished `Dump` at stream end instead costs the full drain (~94ms
/// median) before eval can begin, even though `firstNewDecMs` proves the first new
/// BIN is decoded ~0-1ms after headers.
pub enum DumpMsg {
    /// Sent as soon as the body's header fields are parsed (first chunk), so the
    /// loop thread can set up the sweep before any bins arrive.
    Start(DumpStart),
    /// Decoded NEW BINs, pushed as their JSON spans complete mid-stream.
    Bins(Vec<DetectedBin>),
    /// Stream finished: carried-over bins + the dump's timings.
    End(DumpEnd),
}

pub struct DumpStart {
    pub lane: usize,
    /// Body `lastUpdated` (the dump's release stamp).
    pub last_updated: i64,
    /// Body `totalPages` — how many deep pages the loop thread must fetch.
    pub total_pages: i64,
    /// Epoch ms at response headers — prod's detection stamp (`fetchStart`).
    pub detect_at: i64,
}

pub struct DumpEnd {
    /// Carried-over BINs on page 0: (uuid, starting_bid). Attrs come from the
    /// loop thread's decode cache — prod's `oldBins` (index.ts:609). Only needed
    /// to build byKey at sweep end, so they ride along here rather than streaming.
    pub old_bins: Vec<(String, f64)>,
    pub drain_ms: i64,
    pub dec_cpu_ms: f64,
    /// First NEW BIN decoded, ms after headers (prod's ffDecAhead-class stamp).
    pub first_new_dec_ms: i64,
}

/// The loop thread's rolled sweep state, read by the lanes for the new-BIN
/// screen. `primed` is false until the first full sweep completes; until then
/// prod treats nothing as new (index.ts:608) and suppresses alerts.
struct SweepState {
    primed: bool,
    live: Arc<HashSet<String>>,
}

pub struct Shared {
    /// LM millis of the newest known dump (duplicate-200 filter).
    known_lm: AtomicI64,
    /// LM millis one lane is currently streaming; 0 = free (single-streamer claim).
    streaming: AtomicI64,
    /// 429 backoff deadline (epoch ms).
    backoff: AtomicI64,
    /// Epoch ms we expect the next dump to be published, re-anchored on every
    /// publish we witness. 0 = phase unknown, poll flat out until we learn it.
    next_publish: AtomicI64,
    state: RwLock<SweepState>,
    tx: std::sync::mpsc::Sender<DumpMsg>,
}

impl Shared {
    pub fn new(tx: std::sync::mpsc::Sender<DumpMsg>) -> Arc<Self> {
        Arc::new(Shared {
            known_lm: AtomicI64::new(0),
            streaming: AtomicI64::new(0),
            backoff: AtomicI64::new(0),
            next_publish: AtomicI64::new(0),
            state: RwLock::new(SweepState {
                primed: false,
                live: Arc::new(HashSet::new()),
            }),
            tx,
        })
    }

    /// Publish the rolled live-BIN uuid set after a full sweep (prod ships the
    /// same set to its page workers at index.ts:871-874).
    pub fn publish_live(&self, live: HashSet<String>) {
        let mut st = self.state.write().unwrap();
        st.live = Arc::new(live);
        st.primed = true;
    }
}

/// Incremental extractor: feed body chunks, yields complete auction JSON object
/// spans from inside `"auctions":[...]` plus the top-level `lastUpdated` field.
/// The internal buffer accumulates the whole body, so the `(start, end)` spans
/// it pushes stay valid for the life of the stream.
///
/// Ported verbatim from finder-shadow-rs (chunk-fuzz tested: byte-at-a-time,
/// fixed primes, random 1K-64K).
pub struct Extractor {
    buf: Vec<u8>,
    pos: usize,
    in_array: bool,
    last_updated: Option<i64>,
    total_pages: Option<i64>,
    depth: i32,
    obj_start: Option<usize>,
    in_str: bool,
    esc: bool,
}

impl Default for Extractor {
    fn default() -> Self {
        Self::new()
    }
}

impl Extractor {
    pub fn new() -> Self {
        Extractor {
            buf: Vec::with_capacity(3 << 20),
            pos: 0,
            in_array: false,
            last_updated: None,
            total_pages: None,
            depth: 0,
            obj_start: None,
            in_str: false,
            esc: false,
        }
    }

    /// Append `chunk`, then push any newly-completed auction object spans into
    /// `out`. Chunk-boundary safe: the buffer accumulates before every needle
    /// search, and the brace scanner carries string/escape/depth state across
    /// calls, so a split inside a needle, string, or `\"` escape survives.
    /// True once `"auctions":[` has been seen, i.e. the header fields above it
    /// (lastUpdated/totalPages) are final and the dump can be announced.
    pub fn header_ready(&self) -> bool {
        self.in_array
    }

    pub fn feed(&mut self, chunk: &[u8], out: &mut Vec<(usize, usize)>) {
        self.buf.extend_from_slice(chunk);
        if !self.in_array {
            if self.last_updated.is_none() {
                self.last_updated = scan_num(&self.buf, b"\"lastUpdated\":");
            }
            if self.total_pages.is_none() {
                self.total_pages = scan_num(&self.buf, b"\"totalPages\":");
            }
            match find(&self.buf, b"\"auctions\":[") {
                Some(i) => {
                    self.in_array = true;
                    self.pos = i + 12;
                }
                None => return,
            }
        }
        // Brace-depth scan with string/escape awareness.
        //
        // This used to step one byte at a time, which cost ~7ms (`feedMs`) for a
        // 2.4MB body — ~340MB/s — and made it the single biggest slice of our own
        // compute once decode was fixed. Most of those bytes are INSIDE strings
        // (the base64 `item_bytes` alone is over half the body) where nothing can
        // happen except a quote or an escape, so the scan now jumps between
        // interesting bytes with memchr (SIMD) instead of visiting each one.
        //
        // The three states are disjoint and each has its own small needle set:
        //   in_str      -> only `"` ends it and `\` escapes; skip everything else
        //   depth == 0  -> between auction objects: only `{` (next object) or
        //                  `]` (end of the array) can occur, never a string
        //   depth >= 1  -> inside an object: `"`, `{`, `}`
        // All cross-chunk state (in_str/esc/depth/obj_start) is carried exactly as
        // before, so a boundary landing inside a string or on a `\` still works.
        // `extractor_matches_full_parse_across_chunkings` pins this byte-for-byte.
        while self.pos < self.buf.len() {
            if self.in_str {
                // A `\` seen at the very end of the previous chunk: the byte it
                // escapes is the first one here, and must not be interpreted.
                if self.esc {
                    self.esc = false;
                    self.pos += 1;
                    continue;
                }
                match memchr::memchr2(b'"', b'\\', &self.buf[self.pos..]) {
                    Some(off) => {
                        let i = self.pos + off;
                        if self.buf[i] == b'\\' {
                            self.esc = true;
                        } else {
                            self.in_str = false;
                        }
                        self.pos = i + 1;
                    }
                    None => self.pos = self.buf.len(),
                }
            } else if self.depth == 0 {
                match memchr::memchr2(b'{', b']', &self.buf[self.pos..]) {
                    Some(off) => {
                        let i = self.pos + off;
                        if self.buf[i] == b']' {
                            self.pos = self.buf.len();
                            break; // end of the auctions array
                        }
                        self.obj_start = Some(i);
                        self.depth = 1;
                        self.pos = i + 1;
                    }
                    None => self.pos = self.buf.len(),
                }
            } else {
                match memchr::memchr3(b'"', b'{', b'}', &self.buf[self.pos..]) {
                    Some(off) => {
                        let i = self.pos + off;
                        match self.buf[i] {
                            b'"' => self.in_str = true,
                            b'{' => self.depth += 1,
                            _ => {
                                self.depth -= 1;
                                if self.depth == 0 {
                                    if let Some(s) = self.obj_start.take() {
                                        out.push((s, i + 1));
                                    }
                                }
                            }
                        }
                        self.pos = i + 1;
                    }
                    None => self.pos = self.buf.len(),
                }
            }
        }
    }
}

/// Read the integer that follows `needle` in `hay`. Returns None until a
/// non-digit terminator is present inside the buffer, so a number split across
/// a chunk boundary is never parsed truncated.
fn scan_num(hay: &[u8], needle: &[u8]) -> Option<i64> {
    let i = find(hay, needle)?;
    let tail = &hay[i + needle.len()..];
    let end = tail
        .iter()
        .position(|c| !c.is_ascii_digit())
        .unwrap_or(tail.len());
    if end == 0 || end >= tail.len() {
        return None;
    }
    std::str::from_utf8(&tail[..end]).ok()?.parse().ok()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn poll_loop(
    lane: usize,
    client: reqwest::Client,
    sh: Arc<Shared>,
    flat_delay: Duration,
    schedule: Option<PollSchedule>,
    api_key: Option<String>,
    stagger: Duration,
) {
    // Spread this lane's phase so the lanes tile the interval evenly instead of
    // landing wherever the startup handshakes put them. See `lane_stagger`.
    if !stagger.is_zero() {
        tokio::time::sleep(stagger).await;
    }
    // Per-lane conditional-GET state: the LM header string we echo back and its
    // parsed ms (so we only ever advance the IMS header forward).
    let mut last_lm_hdr: Option<String> = None;
    let mut lane_lm_ms: i64 = 0;
    loop {
        let b = sh.backoff.load(Ordering::Relaxed);
        let now = now_ms();
        if b > now {
            tokio::time::sleep(Duration::from_millis((b - now) as u64)).await;
        }
        let started = Instant::now();
        // Cap the whole request; the 400ms timeout below only bounds time-to-headers.
        let mut req = client
            .get(BASE)
            .timeout(Duration::from_millis(POLL_TIMEOUT_MS + 30_000));
        if let Some(k) = &api_key {
            req = req.header("API-Key", k);
        }
        if let Some(lm) = &last_lm_hdr {
            req = req.header("if-modified-since", lm.clone());
        }
        // On timeout / transport error the `if let` simply falls through and we re-poll.
        if let Ok(Ok(res)) =
            tokio::time::timeout(Duration::from_millis(POLL_TIMEOUT_MS), req.send()).await
        {
            let status = res.status().as_u16();
            if let Some(pause) = backoff_ms_for_status(status) {
                sh.backoff.store(now_ms() + pause, Ordering::Relaxed);
                if status != 429 {
                    // Silence here is how this went unnoticed: every status
                    // except 429 and 200 fell through to a bare re-poll, so a
                    // total detection outage produced no log line at all for
                    // 90 minutes.
                    eprintln!(
                        "detect lane {lane}: HTTP {status} from the auctions API — backing off {}s. \
                         If this repeats, the poll rate (LANES / POLL_DELAY_MS) is too high.",
                        pause / 1000
                    );
                }
            } else if status == 200 {
                let t_headers = now_ms();
                let lm_hdr = res
                    .headers()
                    .get("last-modified")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                let lm_ms = lm_hdr.as_deref().map(lm_to_ms).unwrap_or(0);
                // Re-anchor the burst window on every publish we witness, so the
                // schedule tracks the real clock instead of drifting off a fixed
                // phase. Anchoring on the PUBLISH time rather than on now() keeps
                // our own detection lag out of the prediction.
                if lm_ms > 0 {
                    sh.next_publish
                        .fetch_max(lm_ms + PUBLISH_INTERVAL_MS, Ordering::AcqRel);
                }
                let known = sh.known_lm.load(Ordering::Relaxed);
                if lm_ms <= known {
                    // Duplicate 200 from a stale edge cluster — drop at the
                    // headers stage without reading the body. Advance our IMS
                    // only forward, so we drop back to cheap 304s.
                    if lm_ms > lane_lm_ms {
                        if let Some(lm) = lm_hdr {
                            last_lm_hdr = Some(lm);
                            lane_lm_ms = lm_ms;
                        }
                    }
                    drop(res);
                } else if sh
                    .streaming
                    .compare_exchange(0, lm_ms, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    // Won the single-streamer claim: drain the body.
                    sh.known_lm.fetch_max(lm_ms, Ordering::AcqRel);
                    if let Some(lm) = &lm_hdr {
                        last_lm_hdr = Some(lm.clone());
                        lane_lm_ms = lm_ms;
                    }
                    stream_dump(lane, res, t_headers, lm_ms, &sh).await;
                    sh.streaming.store(0, Ordering::Release);
                } else {
                    // Lost the claim: advance known-LM + IMS and drop the
                    // redundant duplicate body.
                    sh.known_lm.fetch_max(lm_ms, Ordering::AcqRel);
                    if lm_ms > lane_lm_ms {
                        if let Some(lm) = lm_hdr {
                            last_lm_hdr = Some(lm);
                            lane_lm_ms = lm_ms;
                        }
                    }
                    drop(res);
                }
            }
            // 304 (and everything else): free, fall through to the throttle.
        }
        // Re-read the schedule every iteration: another lane may have just
        // witnessed the publish and moved the window.
        let poll_delay = match schedule {
            Some(s) => poll_delay_for(now_ms(), sh.next_publish.load(Ordering::Relaxed), &s),
            None => flat_delay,
        };
        let el = started.elapsed();
        if el < poll_delay {
            tokio::time::sleep(poll_delay - el).await;
        }
    }
}

/// Drain the winning lane's page-0 body, decoding NEW BINs as chunks arrive and
/// STREAMING them to the loop thread so eval overlaps the download.
async fn stream_dump(
    lane: usize,
    res: reqwest::Response,
    t_headers: i64,
    lm_ms: i64,
    sh: &Arc<Shared>,
) {
    use futures_util::StreamExt;
    let mut ex = Extractor::new();
    let mut stream = res.bytes_stream();
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(1500);
    let mut old_bins: Vec<(String, f64)> = Vec::new();
    let mut n_new = 0usize;
    // A failed decode is currently indistinguishable from "nothing new" — both
    // just don't increment n_new. That means a whole class of listing could be
    // silently invisible to the entire pipeline (never priced, never logged,
    // never even shows up as a rejection) with zero trace anywhere. Counting it
    // separately turns that into a visible, alarmable number instead of silence.
    let mut n_dec_fail = 0usize;
    let mut dec_fail_samples: Vec<String> = Vec::new();
    let mut first_new_dec_ms: i64 = -1;
    let mut dec_cpu_us: u128 = 0;
    let mut bytes = 0usize;
    let mut announced = false;
    // Snapshot the rolled state once per dump: an Arc clone, so the lock is
    // never held across an await and the screen can't shift mid-body.
    let (primed, live) = {
        let st = sh.state.read().unwrap();
        (st.primed, st.live.clone())
    };

    // ---- Parallel decode (DECODE_THREADS > 0; 0 = the old inline path, byte
    // identical, and the instant kill-switch). ----
    //
    // decode_item_bytes is ~50ms of CPU per body and it ran INLINE on the very
    // task that reads the socket, so bytes stopped arriving while we decoded.
    // Measured side by side: a probe decoding ALL 935 items drained the body in
    // 92ms while prod, decoding only ~200, took 224ms p50 and up to 536ms. The
    // box peaks at 60% of ONE core with 8 idle.
    //
    // Workers own the decode; the reader only extracts spans and dispatches, so
    // the socket drains at line rate. Bins may reach the loop thread out of body
    // order, which is fine: it dedupes by uuid and prices each independently.
    // End is only sent after every worker has been joined, so no bin can arrive
    // after the sweep is closed.
    let n_workers: usize = std::env::var("DECODE_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut w_tx: Vec<std::sync::mpsc::Sender<Vec<WireAuction>>> = Vec::new();
    #[allow(clippy::type_complexity)]
    let mut w_handles: Vec<std::thread::JoinHandle<(usize, u128, Vec<String>)>> = Vec::new();
    let first_dec = Arc::new(AtomicI64::new(-1));
    let mut pending: Vec<WireAuction> = Vec::new();
    let mut rr = 0usize;
    for _ in 0..n_workers {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<WireAuction>>();
        let out = sh.tx.clone();
        let fd = first_dec.clone();
        w_handles.push(std::thread::spawn(move || {
            let mut fails = 0usize;
            let mut cpu: u128 = 0;
            let mut samples: Vec<String> = Vec::new();
            while let Ok(items) = rx.recv() {
                let mut bins: Vec<DetectedBin> = Vec::with_capacity(items.len());
                for a in items {
                    let t0 = Instant::now();
                    let decoded = decode_item_bytes(&a.item_bytes);
                    cpu += t0.elapsed().as_micros();
                    match decoded {
                        Some(attrs) => {
                            let _ = fd.compare_exchange(
                                -1,
                                now_ms() - t_headers,
                                Ordering::AcqRel,
                                Ordering::Relaxed,
                            );
                            bins.push(DetectedBin {
                                a: ActiveAuction {
                                    uuid: a.uuid,
                                    starting_bid: a.starting_bid,
                                    auctioneer: a.auctioneer,
                                    item_name: a.item_name,
                                },
                                attrs,
                                start: a.start,
                            });
                        }
                        None => {
                            fails += 1;
                            if samples.len() < 5 {
                                samples.push(a.item_name);
                            }
                        }
                    }
                }
                if !bins.is_empty() {
                    let _ = out.send(DumpMsg::Bins(bins));
                }
            }
            (fails, cpu, samples)
        }));
        w_tx.push(tx);
    }

    // Drain decomposition. drainMs is ~206ms for a 2.4MB body while a single
    // request on the same box moves it in 20ms, and CPU starvation / decode
    // blocking / duplicate lane bodies have all been tested and ruled out. This
    // splits the wall time so the next theory is unnecessary: netWait is time
    // BLOCKED on the socket, the rest is our own work.
    let mut t_net_us: u128 = 0;
    let mut t_feed_us: u128 = 0;
    let mut t_parse_us: u128 = 0;
    let mut t_send_us: u128 = 0;
    let mut n_chunks: u64 = 0;
    let mut slow_chunks: u64 = 0;
    let mut max_chunk_us: u128 = 0;
    loop {
        let t_n = Instant::now();
        let next = stream.next().await;
        let el = t_n.elapsed().as_micros();
        t_net_us += el;
        n_chunks += 1;
        if el > 1000 {
            slow_chunks += 1;
        }
        if el > max_chunk_us {
            max_chunk_us = el;
        }
        let Some(chunk) = next else { break };
        let Ok(chunk) = chunk else { break };
        bytes += chunk.len();
        spans.clear();
        let t_f = Instant::now();
        ex.feed(&chunk, &mut spans);
        t_feed_us += t_f.elapsed().as_micros();
        // Announce as soon as the header fields are final — before any bins, so
        // the loop thread is ready to evaluate the first one that lands.
        if !announced && ex.header_ready() {
            announced = true;
            let _ = sh.tx.send(DumpMsg::Start(DumpStart {
                lane,
                last_updated: ex.last_updated.unwrap_or(0),
                total_pages: ex.total_pages.unwrap_or(0),
                detect_at: t_headers,
            }));
        }
        let mut batch: Vec<DetectedBin> = Vec::new();
        for &(s, e) in spans.iter() {
            let t_p = Instant::now();
            let parsed = serde_json::from_slice::<WireAuction>(&ex.buf[s..e]);
            t_parse_us += t_p.elapsed().as_micros();
            let Ok(a) = parsed else {
                continue;
            };
            if !a.bin {
                continue;
            }
            // Carried-over BIN: prod ships (uuid, bid) only and reuses the
            // cached attrs — never re-decodes (index.ts:608-611).
            if primed && live.contains(&a.uuid) {
                old_bins.push((a.uuid, a.starting_bid));
                continue;
            }
            // NOT primed = the priming sweep: every BIN counts as new and MUST be
            // decoded, so the loop thread's decode cache is seeded for the
            // carried-over BINs of the next dump. Prod pays the same cost and
            // simply withholds alerts (index.ts:501/613). Skipping the decode here
            // instead leaves byKey missing ~47k competitors on every later sweep.
            if a.starting_bid <= 0.0 || a.item_bytes.is_empty() {
                continue;
            }
            if n_workers > 0 {
                // Hand it off; a worker decodes it while we keep reading bytes.
                // n_new counts dispatched here and the workers' decode failures
                // are subtracted after the join, so it still means "decoded".
                n_new += 1;
                pending.push(a);
                continue;
            }
            let t0 = Instant::now();
            let decoded = decode_item_bytes(&a.item_bytes);
            dec_cpu_us += t0.elapsed().as_micros();
            match decoded {
                Some(attrs) => {
                    if first_new_dec_ms < 0 {
                        first_new_dec_ms = now_ms() - t_headers;
                    }
                    n_new += 1;
                    batch.push(DetectedBin {
                        a: ActiveAuction {
                            uuid: a.uuid,
                            starting_bid: a.starting_bid,
                            auctioneer: a.auctioneer,
                            item_name: a.item_name,
                        },
                        attrs,
                        start: a.start,
                    });
                }
                None => {
                    n_dec_fail += 1;
                    if dec_fail_samples.len() < 5 {
                        dec_fail_samples.push(a.item_name.clone());
                    }
                }
            }
        }
        // Ship this chunk's new BINs NOW. The loop thread prices them while the
        // rest of the body is still on the wire.
        if n_workers > 0 {
            if !pending.is_empty() {
                let idx = rr % n_workers;
                rr += 1;
                let _ = w_tx[idx].send(std::mem::take(&mut pending));
            }
        } else if !batch.is_empty() {
            let t_s = Instant::now();
            let _ = sh.tx.send(DumpMsg::Bins(batch));
            t_send_us += t_s.elapsed().as_micros();
        }
    }
    // Flush the tail, then close the queues and WAIT: End must not overtake a
    // bin still being decoded, or the sweep would close without it.
    if n_workers > 0 {
        if !pending.is_empty() {
            let idx = rr % n_workers;
            let _ = w_tx[idx].send(std::mem::take(&mut pending));
        }
        w_tx.clear();
        for h in w_handles.drain(..) {
            if let Ok((fails, cpu, samples)) = h.join() {
                n_dec_fail += fails;
                n_new -= fails;
                dec_cpu_us += cpu;
                for s in samples {
                    if dec_fail_samples.len() < 5 {
                        dec_fail_samples.push(s);
                    }
                }
            }
        }
        first_new_dec_ms = first_dec.load(Ordering::Acquire);
    }
    // Degenerate body (transport error before the array): the loop thread still
    // needs a Start/End pair or it would wait for a sweep that never ends.
    if !announced {
        let _ = sh.tx.send(DumpMsg::Start(DumpStart {
            lane,
            last_updated: ex.last_updated.unwrap_or(0),
            total_pages: ex.total_pages.unwrap_or(0),
            detect_at: t_headers,
        }));
    }
    let t_done = now_ms();
    let last_updated = ex.last_updated.unwrap_or(0);
    let drain_ms = t_done - t_headers;
    let dec_cpu_ms = (dec_cpu_us as f64) / 1000.0;
    let line = serde_json::json!({
        "ev": "dump", "lane": lane, "lm": lm_ms, "lastUpdated": last_updated,
        "detectLagMs": if last_updated > 0 { t_headers - last_updated } else { -1 },
        "drainMs": drain_ms, "bytes": bytes, "newBins": n_new, "decFail": n_dec_fail,
        "oldBins": old_bins.len(), "firstNewDecMs": first_new_dec_ms,
        "decCpuMs": dec_cpu_ms, "detectAt": t_headers,
        "netWaitMs": (t_net_us as f64) / 1000.0, "feedMs": (t_feed_us as f64) / 1000.0,
        "parseMs": (t_parse_us as f64) / 1000.0, "sendMs": (t_send_us as f64) / 1000.0,
        "chunks": n_chunks, "slowChunks": slow_chunks, "maxChunkMs": (max_chunk_us as f64) / 1000.0,
    })
    .to_string();
    {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
    if n_dec_fail > 0 {
        eprintln!("baf DECFAIL: lane {lane} — {n_dec_fail} new BIN(s) failed to decode (invisible to the whole pipeline), e.g. {dec_fail_samples:?}");
    }
    // Loop thread gone (shutdown) → nothing to do but drop the dump.
    let _ = sh.tx.send(DumpMsg::End(DumpEnd {
        old_bins,
        drain_ms,
        dec_cpu_ms,
        first_new_dec_ms,
    }));
}

/// Spawn the detect lanes on `rt`. `LOCAL_IPS` pins lanes to source IPs
/// round-robin (prod runs 5); empty = default route.
pub fn spawn_lanes(rt: &tokio::runtime::Runtime, sh: Arc<Shared>) {
    let ips: Vec<String> = std::env::var("LOCAL_IPS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let lanes: usize = std::env::var("LANES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let api_key: Option<String> = std::env::var("API_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // Per-lane poll throttle. Default = round((8 * LANES) / 3) ms (detectWorker.ts).
    let poll_delay = Duration::from_millis(
        std::env::var("POLL_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| ((8.0 * lanes as f64) / 3.0).round() as u64),
    );
    // Burst polling is opt-in. Off = the flat POLL_DELAY_MS schedule, unchanged.
    let schedule = std::env::var("POLL_BURST")
        .ok()
        .filter(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .map(|_| {
            let d = PollSchedule::default();
            let ms = |k: &str, dflt: u64| {
                std::env::var(k)
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(dflt)
            };
            PollSchedule {
                burst_ms: ms("POLL_BURST_MS", d.burst_ms),
                idle_ms: ms("POLL_IDLE_MS", d.idle_ms),
                lead_ms: ms("POLL_BURST_LEAD_MS", d.lead_ms as u64) as i64,
                tail_ms: ms("POLL_BURST_TAIL_MS", d.tail_ms as u64) as i64,
                giveup_ms: d.giveup_ms,
            }
        });
    match schedule {
        Some(s) => eprintln!(
            "detect: {lanes} lanes over {} ips, BURST schedule (burst={}ms idle={}ms window=-{}..+{}ms \
             around a {}s publish clock => ~{:.0} req/s in-window, ~{:.1} req/s average), api_key={}",
            ips.len().max(1),
            s.burst_ms,
            s.idle_ms,
            s.lead_ms,
            s.tail_ms,
            PUBLISH_INTERVAL_MS / 1000,
            lanes as f64 * 1000.0 / s.burst_ms as f64,
            {
                let win = (s.lead_ms + s.tail_ms) as f64;
                let cycle = PUBLISH_INTERVAL_MS as f64;
                let burst = lanes as f64 * 1000.0 / s.burst_ms as f64;
                let idle = lanes as f64 * 1000.0 / s.idle_ms as f64;
                (burst * win + idle * (cycle - win)) / cycle
            },
            if api_key.is_some() { "set" } else { "none" }
        ),
        None => eprintln!(
            "detect: {lanes} lanes over {} ips, poll_delay={poll_delay:?} (flat; set POLL_BURST=1 for the burst schedule), api_key={}",
            ips.len().max(1),
            if api_key.is_some() { "set" } else { "none" }
        ),
    }
    for lane in 0..lanes {
        // ⛔ DO NOT re-derive the "HTTP/2 flow control is 80% of the drain"
        // theory from this spot. A long comment used to live here claiming
        // netWait was hyper's 64KB default stream window and that big fixed
        // windows plus a DETECT_HTTP1=1 kill-switch would fix it. None of that
        // code ever existed, and it could not have: `reqwest` is declared
        // `default-features = false` with only blocking/rustls-tls/json/gzip/
        // stream (finder-rs/Cargo.toml:23) — **no `http2` feature**, so this
        // client is HTTP/1.1-only and has no stream flow control to tune. The
        // http2_* builder methods do not even compile here (verified
        // 2026-08-14).
        //
        // The gap the comment was reaching for is real but is NOT the protocol.
        // Measured on the prod box, one curl each, same endpoint and second:
        //   curl --http2   ttfb 28.2ms  total 41.4ms
        //   curl --http1.1 ttfb 31.2ms  total 46.4ms
        // versus our netWaitMs p50 191ms. A single cold request is simply not
        // subject to whatever paces a client already issuing ~40 req/s at the
        // same origin, so the comparison does not isolate the protocol. See
        // the "origin pacing" finding: the drain is upstream, and the same
        // delay is paid by everyone reading this dump.
        let mut cb = reqwest::Client::builder()
            .user_agent(crate::USER_AGENT)
            .gzip(true)
            .pool_max_idle_per_host(2)
            .pool_idle_timeout(Duration::from_secs(300))
            .tcp_keepalive(Duration::from_secs(30));
        if !ips.is_empty() {
            let ip: std::net::IpAddr = ips[lane % ips.len()].parse().expect("bad LOCAL_IPS entry");
            cb = cb.local_address(ip);
        }
        let client = cb.build().expect("client build");
        let stagger = lane_stagger(lane, lanes, poll_delay);
        rt.spawn(poll_loop(
            lane,
            client,
            sh.clone(),
            poll_delay,
            schedule,
            api_key.clone(),
            stagger,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Times the Extractor over a real page-0 in prod-shaped ~3.4KB chunks.
    /// Not an assertion (CI machines vary) — run it with --nocapture to compare a
    /// scanner change against the byte-at-a-time baseline it replaced.
    ///   cargo test --release -p finder-rs feed_throughput -- --nocapture --ignored
    #[test]
    #[ignore]
    fn feed_throughput() {
        let data = fixture();
        let mut best = f64::MAX;
        for _ in 0..20 {
            let mut ex = Extractor::default();
            let mut out = Vec::new();
            let t = std::time::Instant::now();
            for c in data.chunks(3400) {
                out.clear();
                ex.feed(c, &mut out);
            }
            best = best.min(t.elapsed().as_secs_f64());
        }
        println!(
            "  feed: {:.2} ms for {:.2} MB = {:.0} MB/s",
            best * 1000.0,
            data.len() as f64 / 1e6,
            data.len() as f64 / 1e6 / best
        );
    }

    fn fixture() -> Vec<u8> {
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/fixture-page0.json"))
            .expect("fixture-page0.json present")
    }

    #[tokio::test]
    async fn every_request_identifies_itself() {
        // Wire-level, not a config assertion: reqwest silently sends NO
        // User-Agent unless one is set, which is what a WAF sees as a bot.
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 2048];
            let n = sock.read(&mut buf).unwrap_or(0);
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
            String::from_utf8_lossy(&buf[..n]).to_string()
        });

        // Built exactly as spawn_lanes builds a poll client.
        let client = reqwest::Client::builder()
            .user_agent(crate::USER_AGENT)
            .gzip(true)
            .build()
            .expect("client");
        let _ = client.get(format!("http://127.0.0.1:{port}/")).send().await;

        let request = server.join().expect("server thread");
        let lower = request.to_lowercase();
        assert!(
            lower.contains("user-agent:"),
            "no User-Agent on the wire:\n{request}"
        );
        assert!(
            request.contains("baf-finder/"),
            "User-Agent does not identify this app:\n{request}"
        );
    }

    #[test]
    fn lanes_tile_the_poll_interval_instead_of_bunching() {
        let d = Duration::from_millis(100);
        let offs: Vec<u128> = (0..4).map(|l| lane_stagger(l, 4, d).as_millis()).collect();
        assert_eq!(offs, vec![0, 25, 50, 75], "4 lanes must tile 100ms evenly");

        // The property that actually matters: the largest gap between consecutive
        // polls (wrapping) IS the worst-case detection delay. Unstaggered, every
        // lane sits at offset 0 and the gap is the whole interval.
        let worst = |offs: &[u128]| {
            let mut v = offs.to_vec();
            v.sort();
            let mut g = v[0] + 100 - v[v.len() - 1];
            for w in v.windows(2) {
                g = g.max(w[1] - w[0]);
            }
            g
        };
        assert_eq!(
            worst(&offs),
            25,
            "staggered: worst-case wait is one quarter"
        );
        assert_eq!(
            worst(&[0, 0, 0, 0]),
            100,
            "in phase: worst case is the FULL interval"
        );

        assert_eq!(
            lane_stagger(0, 1, d),
            Duration::ZERO,
            "a single lane needs no offset"
        );
    }

    #[test]
    fn burst_window_is_fast_and_the_rest_of_the_cycle_is_not() {
        let s = PollSchedule::default();
        let publish = 1_000_000_000i64;
        let fast = Duration::from_millis(s.burst_ms);
        let slow = Duration::from_millis(s.idle_ms);

        // Just before, exactly on, and just after the predicted publish.
        assert_eq!(poll_delay_for(publish - s.lead_ms + 1, publish, &s), fast);
        assert_eq!(poll_delay_for(publish, publish, &s), fast);
        assert_eq!(poll_delay_for(publish + s.tail_ms - 1, publish, &s), fast);

        // Mid-cycle, nowhere near a publish: coast.
        assert_eq!(poll_delay_for(publish - 30_000, publish, &s), slow);
        assert_eq!(poll_delay_for(publish - s.lead_ms - 1, publish, &s), slow);
    }

    #[test]
    fn an_unknown_phase_polls_flat_out() {
        // On startup we have never seen a publish, so there is no window to aim
        // at. Coasting here would delay the first detection by a whole cycle.
        let s = PollSchedule::default();
        assert_eq!(
            poll_delay_for(1_000_000_000, 0, &s),
            Duration::from_millis(s.burst_ms)
        );
    }

    #[test]
    fn a_late_publish_keeps_us_looking_then_stops() {
        let s = PollSchedule::default();
        let publish = 1_000_000_000i64;
        // Overdue: the dump is late, stay fast rather than miss it entirely.
        assert_eq!(
            poll_delay_for(publish + s.tail_ms + 5_000, publish, &s),
            Duration::from_millis(s.burst_ms)
        );
        // Hopelessly overdue: the API is stalled or we are blocked. Backing off
        // here is the whole point — a stuck fast loop IS the request storm.
        assert_eq!(
            poll_delay_for(publish + s.giveup_ms + 1, publish, &s),
            Duration::from_millis(s.idle_ms)
        );
    }

    /// (in-window req/s, average req/s) for `lanes` on this schedule.
    fn rates(s: &PollSchedule, lanes: f64) -> (f64, f64) {
        let window_ms = (s.lead_ms + s.tail_ms) as f64;
        let cycle_ms = PUBLISH_INTERVAL_MS as f64;
        let burst = lanes * 1000.0 / s.burst_ms as f64;
        let idle = lanes * 1000.0 / s.idle_ms as f64;
        (
            burst,
            (burst * window_ms + idle * (cycle_ms - window_ms)) / cycle_ms,
        )
    }

    #[test]
    fn burst_keeps_the_old_resolution_without_the_old_request_rate() {
        // The whole argument for this change: detection is no slower than the
        // always-on schedule, it just stops paying for it all minute.
        const BLOCKED_AT: f64 = 427.0; // measured 2026-07-27, got every IP banned
        let s = PollSchedule::default();
        let (burst, avg) = rates(&s, 4.0);

        assert!(
            burst >= BLOCKED_AT,
            "in-window rate {burst:.0}/s must match the old always-on rate, else this IS slower"
        );
        assert!(
            avg < BLOCKED_AT / 10.0,
            "average {avg:.1}/s must be far under the {BLOCKED_AT}/s that caused the block"
        );
    }

    #[test]
    fn the_burst_window_dwarfs_the_measured_jitter() {
        // Interval spread was 0.0s across 11 consecutive publishes and the phase
        // never moved off :31s. If that ever tightens to nothing this margin is
        // still what absorbs a clock shift, so it must not be shaved to fit.
        let s = PollSchedule::default();
        assert!(
            s.lead_ms >= 1_000 && s.tail_ms >= 1_000,
            "window too tight to absorb a shift"
        );
        assert!(
            s.giveup_ms > PUBLISH_INTERVAL_MS / 4,
            "giving up before a quarter-cycle would drop us to idle during a normal late publish"
        );
    }

    #[test]
    fn a_403_pauses_every_lane_for_a_long_time() {
        // The 2026-07-27 outage: Cloudflare blocked the box, 403s came back in
        // ~20ms, and with no backoff the lanes re-polled at the poll delay —
        // ~400 requests/second aimed at the thing already blocking us, which
        // is why it never cleared on its own.
        let pause = backoff_ms_for_status(403).expect("403 must back off");
        assert!(
            pause >= 30_000,
            "403 backoff is {pause}ms; too short to let a WAF block expire"
        );
        assert!(
            pause > backoff_ms_for_status(429).unwrap(),
            "a block must back off harder than a rate limit"
        );
    }

    #[test]
    fn success_and_not_modified_never_pause_polling() {
        // 304 is the expected answer almost every poll. Backing off on it
        // would throttle detection to nothing.
        assert_eq!(backoff_ms_for_status(200), None);
        assert_eq!(backoff_ms_for_status(304), None);
    }

    #[test]
    fn a_429_still_backs_off() {
        assert_eq!(backoff_ms_for_status(429), Some(RATE_LIMIT_BACKOFF_MS));
    }

    /// Feed `data` through the Extractor in chunks whose sizes come from `next`,
    /// and return (lastUpdated, ordered uuids of every extracted span).
    fn run_extractor(
        data: &[u8],
        mut next: impl FnMut() -> usize,
    ) -> (Option<i64>, Vec<String>, Option<i64>) {
        let mut ex = Extractor::new();
        let mut uuids: Vec<String> = Vec::new();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        let mut i = 0usize;
        while i < data.len() {
            let n = next().max(1).min(data.len() - i);
            spans.clear();
            ex.feed(&data[i..i + n], &mut spans);
            for &(s, e) in spans.iter() {
                let a: WireAuction = serde_json::from_slice(&ex.buf[s..e])
                    .unwrap_or_else(|err| panic!("span [{s}..{e}] is not a valid Auction: {err}"));
                uuids.push(a.uuid);
            }
            i += n;
        }
        (ex.last_updated, uuids, ex.total_pages)
    }

    fn expected(data: &[u8]) -> (i64, Vec<String>) {
        let v: Value = serde_json::from_slice(data).unwrap();
        let arr = v["auctions"].as_array().unwrap();
        let uuids: Vec<String> = arr
            .iter()
            .map(|a| a["uuid"].as_str().unwrap().to_string())
            .collect();
        (v["lastUpdated"].as_i64().unwrap(), uuids)
    }

    #[test]
    fn extractor_matches_full_parse_across_chunkings() {
        let data = fixture();
        let (exp_lu, exp_uuids) = expected(&data);
        assert_eq!(exp_lu, 1783941154121, "fixture lastUpdated sanity");
        assert!(exp_uuids.len() >= 800, "fixture should hold a full page-0");
        // totalPages drives the deep-page fetch: a miss would silently sweep
        // page 0 only, so pin it the same way lastUpdated is pinned.
        let exp_tp: Value = serde_json::from_slice(&data).unwrap();
        let exp_tp = exp_tp["totalPages"].as_i64().unwrap();

        // A xorshift PRNG so chunk boundaries land inside strings, escapes, and
        // the needles — deterministically but without a fixed alignment.
        let mut state: u64 = 0x9e3779b97f4a7c15;
        let mut rng = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        // 1) Byte-at-a-time: the hardest boundary stress.
        let (lu1, u1, tp1) = run_extractor(&data, || 1);
        assert_eq!(lu1, Some(exp_lu), "byte-at-a-time lastUpdated");
        assert_eq!(u1, exp_uuids, "byte-at-a-time uuids in order");
        assert_eq!(tp1, Some(exp_tp), "byte-at-a-time totalPages");

        // 2) A few fixed small primes (odd alignments vs the 12/13/14-byte needles).
        for &sz in &[3usize, 7, 13, 4096] {
            let (lu, u, tp) = run_extractor(&data, || sz);
            assert_eq!(lu, Some(exp_lu), "fixed chunk {sz} lastUpdated");
            assert_eq!(u.len(), exp_uuids.len(), "fixed chunk {sz} count");
            assert_eq!(tp, Some(exp_tp), "fixed chunk {sz} totalPages");
        }

        // 3) Randomized 1KB..64KB chunks.
        let (lu3, u3, tp3) = run_extractor(&data, || 1024 + (rng() as usize % (64 * 1024 - 1024)));
        assert_eq!(lu3, Some(exp_lu), "random-chunk lastUpdated");
        assert_eq!(u3, exp_uuids, "random-chunk uuids in order");
        assert_eq!(tp3, Some(exp_tp), "random-chunk totalPages");
    }

    /// The new-BIN screen must key off the previous FULL sweep's live set, so a
    /// BIN that merely moved pages is NOT re-flipped. Guards the screen logic
    /// (`primed && live.contains`) that stream_dump applies per auction.
    #[test]
    fn screen_uses_published_live_set() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let sh = Shared::new(tx);
        {
            let st = sh.state.read().unwrap();
            assert!(
                !st.primed,
                "starts unprimed — nothing is 'new' before the first full sweep"
            );
        }
        let mut live = HashSet::new();
        live.insert("carried-over-uuid".to_string());
        sh.publish_live(live);
        let st = sh.state.read().unwrap();
        assert!(st.primed, "primed after the first publish");
        assert!(
            st.live.contains("carried-over-uuid"),
            "carried-over BIN is not new"
        );
        assert!(!st.live.contains("fresh-uuid"), "unseen BIN is new");
    }
}
