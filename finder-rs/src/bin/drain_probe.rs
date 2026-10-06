//! drain_probe: attribute the finder's `drainMs` to a stage.
//!
//! Prod reports drainMs p50 ~155ms for a ~2.4MB page while raw curl moves the
//! same bytes in ~27ms. This replicates `stream_dump`'s pipeline stage by stage
//! against the live endpoint so the missing ~100ms lands on a specific line.
//!
//! Stages, cumulative, each adding one layer of the real loop:
//!   1 NET      reqwest gzip bytes_stream, chunks dropped (no work at all)
//!   2 +COPY    also extend a growing buffer, like Extractor::buf
//!   3 +SCAN    also run the brace/string span scanner
//!   4 +PARSE   also serde_json::from_slice each span (WireAuction shape)
//!   5 +DECODE  also decode_item_bytes on non-carried BINs (the real decode cost)
//!
//! Run on NFO. Touches nothing the finder owns; polls the same public page 0.
use futures_util::StreamExt;
use serde::Deserialize;
use std::time::Instant;

const URL: &str = "https://api.hypixel.net/v2/skyblock/auctions?page=0";

#[derive(Deserialize)]
struct WireAuction {
    uuid: String,
    #[serde(default)]
    bin: bool,
    #[serde(default)]
    starting_bid: f64,
    #[serde(default)]
    item_bytes: String,
}

/// Same brace/string/escape scanner as detect.rs Extractor::feed.
struct Ex {
    buf: Vec<u8>,
    pos: usize,
    in_array: bool,
    depth: i32,
    obj_start: Option<usize>,
    in_str: bool,
    esc: bool,
}

impl Ex {
    fn new() -> Self {
        Ex {
            buf: Vec::with_capacity(4 << 20),
            pos: 0,
            in_array: false,
            depth: 0,
            obj_start: None,
            in_str: false,
            esc: false,
        }
    }
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<(usize, usize)>) {
        self.buf.extend_from_slice(chunk);
        if !self.in_array {
            let n = b"\"auctions\":[";
            match self.buf.windows(n.len()).position(|w| w == n) {
                Some(i) => {
                    self.in_array = true;
                    self.pos = i + n.len();
                }
                None => return,
            }
        }
        while self.pos < self.buf.len() {
            let c = self.buf[self.pos];
            if self.in_str {
                if self.esc {
                    self.esc = false;
                } else if c == b'\\' {
                    self.esc = true;
                } else if c == b'"' {
                    self.in_str = false;
                }
            } else {
                match c {
                    b'"' => self.in_str = true,
                    b'{' => {
                        if self.depth == 0 {
                            self.obj_start = Some(self.pos);
                        }
                        self.depth += 1;
                    }
                    b'}' => {
                        self.depth -= 1;
                        if self.depth == 0 {
                            if let Some(s) = self.obj_start.take() {
                                out.push((s, self.pos + 1));
                            }
                        }
                    }
                    b']' if self.depth == 0 => {
                        self.pos = self.buf.len();
                        break;
                    }
                    _ => {}
                }
            }
            self.pos += 1;
        }
    }
}

async fn run(client: &reqwest::Client, stage: u8) -> (f64, usize, usize) {
    let res = client.get(URL).send().await.expect("send");
    let t0 = Instant::now();
    let mut stream = res.bytes_stream();
    let mut ex = Ex::new();
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(1500);
    let mut bytes = 0usize;
    let mut n = 0usize;
    let mut plain = Vec::<u8>::with_capacity(4 << 20);
    while let Some(Ok(c)) = stream.next().await {
        bytes += c.len();
        match stage {
            1 => {}
            2 => plain.extend_from_slice(&c),
            _ => {
                spans.clear();
                ex.feed(&c, &mut spans);
                if stage >= 4 {
                    for &(s, e) in spans.iter() {
                        if let Ok(a) = serde_json::from_slice::<WireAuction>(&ex.buf[s..e]) {
                            if !a.bin || a.starting_bid <= 0.0 || a.item_bytes.is_empty() {
                                continue;
                            }
                            n += 1;
                            if stage >= 5 {
                                std::hint::black_box(finder_core::nbt::decode_item_bytes(
                                    &a.item_bytes,
                                ));
                            }
                            std::hint::black_box(&a.uuid);
                        }
                    }
                }
            }
        }
    }
    (t0.elapsed().as_secs_f64() * 1e3, bytes, n)
}

/// Spawn `n` background pollers that hammer the endpoint exactly like the
/// finder's losing lanes do while one lane streams the body.
fn spawn_bg(n: usize, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    for _ in 0..n {
        let stop = stop.clone();
        tokio::spawn(async move {
            let c = reqwest::Client::builder()
                .gzip(true)
                .pool_max_idle_per_host(2)
                .build()
                .unwrap();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let t = std::time::Instant::now();
                let _ =
                    tokio::time::timeout(std::time::Duration::from_millis(400), c.get(URL).send())
                        .await;
                let el = t.elapsed();
                if el < std::time::Duration::from_millis(21) {
                    tokio::time::sleep(std::time::Duration::from_millis(21) - el).await;
                }
            }
        });
    }
}

/// Simulate the finder's OTHER in-process work (eval loop thread, ws server,
/// bazaar collector): allocation-heavy churn on dedicated OS threads. This is
/// what separates the bare probe from the real 18-thread finder process.
fn spawn_hogs(n: usize, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    for _ in 0..n {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut sink: Vec<Vec<u8>> = Vec::new();
            let mut i: usize = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                i = i.wrapping_add(1);
                let sz = 64 + (i % 4096);
                let mut v = vec![0u8; sz];
                v[0] = i as u8;
                std::hint::black_box(&v);
                sink.push(v);
                if sink.len() > 512 {
                    sink.clear();
                }
            }
        });
    }
}

#[tokio::main]
async fn main() {
    let client = reqwest::Client::builder()
        .gzip(true)
        .pool_max_idle_per_host(2)
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .build()
        .unwrap();
    // Warm the connection pool + TLS so stage 1 isn't paying handshake.
    let _ = run(&client, 1).await;

    let names = [
        "",
        "1 NET (drop chunks)",
        "2 +COPY buffer",
        "3 +SCAN spans",
        "4 +PARSE wire",
        "5 +DECODE items",
    ];
    let reps: u32 = std::env::var("REPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let bg: usize = std::env::var("BG_LANES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let hogs: usize = std::env::var("HOG_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if bg > 0 {
        println!("(competing background lanes: {bg}, polling every 21ms like the finder)");
        spawn_bg(bg, stop.clone());
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    if hogs > 0 {
        println!("(allocation-heavy background threads: {hogs})");
        spawn_hogs(hogs, stop.clone());
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    for stage in 1u8..=5 {
        let mut best = f64::MAX;
        let mut worst: f64 = 0.0;
        let mut sum = 0.0;
        let (mut b, mut n) = (0, 0);
        for _ in 0..reps {
            let (ms, by, nn) = run(&client, stage).await;
            best = best.min(ms);
            worst = worst.max(ms);
            sum += ms;
            b = by;
            n = nn;
        }
        println!(
            "{:<22} min {:>7.1} ms  avg {:>7.1} ms  max {:>7.1} ms   (bytes {}, decoded {})",
            names[stage as usize],
            best,
            sum / reps as f64,
            worst,
            b,
            n
        );
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
}
