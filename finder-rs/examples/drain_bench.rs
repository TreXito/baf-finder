//! Why does draining the dump take ~125ms when curl moves the same body in 14ms?
//!
//! 76.7% of all flip profit lives in auctions that are gone within 1 second of
//! us finding them, so ~110ms of self-inflicted drain overhead is real money.
//! Prior work established the shape (~660 chunks, ~0.13ms of wakeup each, no
//! stalls, gzip is not what chunks the stream) but never tested whether the
//! STREAMING consumption pattern is itself the cost.
//!
//! Three ways of taking the same response, back to back on the same process and
//! connection pool, so the comparison is apples to apples:
//!
//!   stream   - what detect.rs does now: `bytes_stream()`, one wakeup per chunk
//!   bytes    - `.bytes()`, hyper fills one buffer, no per-chunk yields
//!   raw      - stream but only counting, no per-chunk work at all
//!
//! If `bytes` is close to curl then the streaming path is the problem, and the
//! first flip could be emitted sooner by buffering than by streaming, even
//! though streaming intuitively looks earlier.
//!
//! usage: drain_bench [iterations]
use futures_util::StreamExt;
use std::time::Instant;

const URL: &str = "https://api.hypixel.net/v2/skyblock/auctions?page=0";

fn pct(v: &mut Vec<f64>, p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p) as usize]
}

#[tokio::main]
async fn main() {
    let iters: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);
    let client = reqwest::Client::builder()
        .gzip(true)
        .tcp_nodelay(true)
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
        .expect("client");

    let mut s_total = Vec::new();
    let mut s_ttfb = Vec::new();
    let mut s_body = Vec::new();
    let mut s_chunks = Vec::new();
    let mut b_total = Vec::new();
    let mut b_ttfb = Vec::new();
    let mut b_body = Vec::new();
    let mut w_ttfb = Vec::new();
    let mut w_body = Vec::new();

    for i in 0..iters {
        // --- streaming, exactly like detect.rs ---
        let t0 = Instant::now();
        let res = client.get(URL).send().await.expect("send");
        let ttfb = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = Instant::now();
        let mut stream = res.bytes_stream();
        let (mut n, mut chunks) = (0usize, 0u64);
        while let Some(c) = stream.next().await {
            let c = c.expect("chunk");
            n += c.len();
            chunks += 1;
        }
        let body = t1.elapsed().as_secs_f64() * 1000.0;
        s_ttfb.push(ttfb);
        s_body.push(body);
        s_total.push(ttfb + body);
        s_chunks.push(chunks as f64);
        println!("  [{i}] stream  ttfb={ttfb:6.1}ms body={body:6.1}ms chunks={chunks:5} bytes={n}");

        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        // --- whole body in one go ---
        let t0 = Instant::now();
        let res = client.get(URL).send().await.expect("send");
        let ttfb = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = Instant::now();
        let b = res.bytes().await.expect("bytes");
        let body = t1.elapsed().as_secs_f64() * 1000.0;
        b_ttfb.push(ttfb);
        b_body.push(body);
        b_total.push(ttfb + body);
        println!(
            "  [{i}] bytes   ttfb={ttfb:6.1}ms body={body:6.1}ms bytes={}",
            b.len()
        );

        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        // --- streaming WITH realistic per-chunk work ---
        // Prod spends decCpu+feed+parse ~= 32ms across ~600 chunks, i.e. ~53us
        // of work per chunk, and it does that work INLINE in the read loop. If
        // coupling the read to that work is what throttles the sender via TCP
        // flow control, this mode reproduces prod's 131ms while the plain
        // stream above stays at ~20ms. That is the whole hypothesis.
        let t0 = Instant::now();
        let res = client.get(URL).send().await.expect("send");
        let ttfb = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = Instant::now();
        let mut stream = res.bytes_stream();
        let (mut n, mut chunks) = (0usize, 0u64);
        while let Some(c) = stream.next().await {
            let c = c.expect("chunk");
            n += c.len();
            chunks += 1;
            // busy-wait ~53us, the measured per-chunk cost in prod
            let spin = Instant::now();
            let mut acc = 0u64;
            while spin.elapsed().as_micros() < 53 {
                acc = acc.wrapping_add(c.len() as u64);
                std::hint::black_box(acc);
            }
        }
        let body = t1.elapsed().as_secs_f64() * 1000.0;
        w_ttfb.push(ttfb);
        w_body.push(body);
        println!("  [{i}] +work   ttfb={ttfb:6.1}ms body={body:6.1}ms chunks={chunks:5} bytes={n}");

        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }

    println!();
    println!(
        "  {:<10}{:>10}{:>10}{:>10}{:>10}",
        "mode", "ttfb p50", "body p50", "body p90", "chunks"
    );
    println!(
        "  {:<10}{:>9.1}{:>10.1}{:>10.1}{:>10.0}",
        "stream",
        pct(&mut s_ttfb, 0.5),
        pct(&mut s_body, 0.5),
        pct(&mut s_body, 0.9),
        pct(&mut s_chunks, 0.5)
    );
    println!(
        "  {:<10}{:>9.1}{:>10.1}{:>10.1}{:>10}",
        "bytes",
        pct(&mut b_ttfb, 0.5),
        pct(&mut b_body, 0.5),
        pct(&mut b_body, 0.9),
        "-"
    );
    println!(
        "  {:<10}{:>9.1}{:>10.1}{:>10.1}{:>10}",
        "+work",
        pct(&mut w_ttfb, 0.5),
        pct(&mut w_body, 0.5),
        pct(&mut w_body, 0.9),
        "-"
    );
    println!();
    println!("  prod drainMs p50 is 131ms. If '+work' lands near that while 'stream'");
    println!("  stays ~20ms, the read loop is throttling the sender and the fix is to");
    println!("  decouple reading from processing.");
    println!();
    let sb = pct(&mut s_body, 0.5);
    let bb = pct(&mut b_body, 0.5);
    println!(
        "  body-transfer delta: {:+.1}ms ({} is faster)",
        bb - sb,
        if bb < sb { "bytes" } else { "stream" }
    );
    println!("  curl on this box does the same body in ~14ms after TTFB.");
}
