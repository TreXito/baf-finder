//! detect_probe: a standalone CloudFlare-POP detection-latency probe.
//!
//! Drop the compiled static binary on a cheap VPS in any region and it reports,
//! per Hypixel auction-house dump, WHICH CloudFlare POP served you and HOW MANY
//! ms after Hypixel actually published (`detectLagMs`) that POP handed you the
//! new copy. Run the same binary in several regions at once (Ashburn, Newark,
//! Dallas, ...) and the POP with the consistently-lowest `detectLagMs` is the one
//! closest to Hypixel's origin: that is where the finder's DETECTION should live
//! to stop losing the ~50ms API race to COFL. (Pricing + serving stay on NFO with
//! the bots; only detection is POP-bound.)
//!
//! It shares no finder state and only polls the public page 0, so it is a tiny
//! portable binary with no secrets. `detectLagMs = t_headers - body.lastUpdated`;
//! `lastUpdated` is Hypixel's own publish timestamp (identical across every POP),
//! so the number is directly comparable region-to-region. Keep the probe hosts on
//! NTP so their wall clocks agree to a few ms (skew shows up as a constant offset,
//! which a quick side-by-side idle comparison reveals).
//!
//! Env: LANES (default 4), POLL_DELAY_MS (default 15), API_KEY (optional; the
//! public endpoint is keyless, matching the finder, so leave it unset for a fair
//! comparison). Output: one JSON line per detected dump on stdout.
//!
//! Aggregate a run with, e.g.:
//!   grep detectLagMs probe.log | jq -s 'group_by(.pop)[]|{pop:.[0].pop,n:length,
//!     p50:(sort_by(.detectLagMs)[length/2|floor].detectLagMs)}'

use futures_util::StreamExt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const URL: &str = "https://api.hypixel.net/v2/skyblock/auctions?page=0";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Pull the integer value of `"lastUpdated":<n>` out of the head of the (already
/// decompressed) JSON body. It sits in the first ~90 bytes, so the caller only
/// needs to buffer a few hundred bytes of the stream.
fn parse_last_updated(buf: &[u8]) -> Option<i64> {
    let needle = b"\"lastUpdated\":";
    let p = buf.windows(needle.len()).position(|w| w == needle)? + needle.len();
    let mut n: i64 = 0;
    let mut seen = false;
    for &b in &buf[p..] {
        if b.is_ascii_digit() {
            n = n * 10 + (b - b'0') as i64;
            seen = true;
        } else if seen {
            break;
        } else if b == b' ' {
            continue; // tolerate `"lastUpdated": 123`
        } else {
            break;
        }
    }
    seen.then_some(n)
}

struct Shared {
    /// Highest `lastUpdated` any lane has logged, so a dump is reported ONCE even
    /// though every lane sees it (they all hit the same POP at ~the same instant).
    known: AtomicI64,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let lanes: usize = std::env::var("LANES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let poll_delay = Duration::from_millis(
        std::env::var("POLL_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15),
    );
    let api_key = std::env::var("API_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    eprintln!(
        "detect_probe: {lanes} lanes, poll_delay={poll_delay:?}, api_key={}; one JSON line per dump on stdout",
        if api_key.is_some() { "set" } else { "none" }
    );
    let sh = Arc::new(Shared {
        known: AtomicI64::new(0),
    });
    let mut handles = Vec::new();
    for lane in 0..lanes {
        let client = reqwest::Client::builder()
            .gzip(true)
            .pool_max_idle_per_host(2)
            .pool_idle_timeout(Duration::from_secs(300))
            .tcp_keepalive(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(3))
            .build()
            .expect("client build");
        handles.push(tokio::spawn(lane_loop(
            lane,
            client,
            sh.clone(),
            poll_delay,
            api_key.clone(),
        )));
    }
    for h in handles {
        let _ = h.await;
    }
}

async fn lane_loop(
    lane: usize,
    client: reqwest::Client,
    sh: Arc<Shared>,
    poll_delay: Duration,
    api_key: Option<String>,
) {
    // Conditional-GET state: echo the last Last-Modified back so unchanged polls
    // come back as cheap 304s instead of the full ~2.5MB body.
    let mut last_lm: Option<String> = None;
    loop {
        let started = Instant::now();
        let mut req = client.get(URL);
        if let Some(k) = &api_key {
            req = req.header("API-Key", k);
        }
        if let Some(lm) = &last_lm {
            req = req.header("if-modified-since", lm.clone());
        }
        if let Ok(Ok(res)) = tokio::time::timeout(Duration::from_millis(2000), req.send()).await {
            // Stamp the detection at HEADER arrival, before reading the body, so
            // the body read never inflates detectLag.
            let t_headers = now_ms();
            if res.status().as_u16() == 200 {
                // Pull the identifying headers out before the body consumes `res`.
                let pop = res
                    .headers()
                    .get("cf-ray")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.rsplit('-').next())
                    .unwrap_or("?")
                    .to_string();
                let age = res
                    .headers()
                    .get("age")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("?")
                    .to_string();
                if let Some(lm) = res
                    .headers()
                    .get("last-modified")
                    .and_then(|v| v.to_str().ok())
                {
                    last_lm = Some(lm.to_string());
                }
                // Read just the head of the (decompressed) body for lastUpdated.
                let mut buf: Vec<u8> = Vec::with_capacity(512);
                let mut stream = res.bytes_stream();
                while let Some(Ok(chunk)) = stream.next().await {
                    buf.extend_from_slice(&chunk);
                    if buf.len() >= 384 {
                        break;
                    }
                }
                if let Some(lu) = parse_last_updated(&buf) {
                    // First lane to advance `known` past this lastUpdated owns the
                    // single log line for this dump.
                    let prev = sh.known.load(Ordering::Acquire);
                    if lu > prev
                        && sh
                            .known
                            .compare_exchange(prev, lu, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                    {
                        println!(
                            "{{\"t\":{t_headers},\"pop\":\"{pop}\",\"lastUpdated\":{lu},\"detectLagMs\":{},\"age\":\"{age}\",\"lane\":{lane}}}",
                            t_headers - lu
                        );
                    }
                }
            }
            // 304 and everything else: cheap, fall through to the throttle.
        }
        let el = started.elapsed();
        if el < poll_delay {
            tokio::time::sleep(poll_delay - el).await;
        }
    }
}
