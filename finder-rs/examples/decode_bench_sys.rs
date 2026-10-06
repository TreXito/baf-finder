//! `finder-core/examples/decode_bench`, on the SYSTEM allocator (musl malloc in
//! the deploy artifact). The A/B partner of `decode_bench_mi`; identical code,
//! only the allocator differs.
//!
//! An example is its own crate root, so it does not inherit main.rs's
//! `#[global_allocator]` — it has to declare its own. That is the whole reason
//! this file exists as a near-duplicate.
//!
//! Build BOTH for musl and run BOTH on the box; a glibc host number says nothing
//! about the deploy artifact:
//!   cargo build --release --target x86_64-unknown-linux-musl --example decode_bench    -p finder-core
//!   cargo build --release --target x86_64-unknown-linux-musl --example decode_bench_mi -p finder-rs
//!
//! usage: decode_bench_sys <page0.json> [iters] [threads]
//!
//! `threads` > 1 decodes the same payloads from N threads at once, which is the
//! case that actually matters: prod runs 4 detect lanes plus a bazaar poller, and
//! a global allocator lock only shows up under contention.

use finder_core::nbt::decode_item_bytes;
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).map(String::as_str).unwrap_or("page0.json");
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(12);
    let threads: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1);

    let body = std::fs::read(path).expect("read page0");
    let v: serde_json::Value = serde_json::from_slice(&body).expect("parse page0");
    let bytes: Arc<Vec<String>> = Arc::new(
        v["auctions"]
            .as_array()
            .expect("auctions")
            .iter()
            .filter(|a| a["bin"].as_bool().unwrap_or(false))
            .filter_map(|a| a["item_bytes"].as_str().map(str::to_string))
            .collect(),
    );
    let n = bytes.len();
    println!("system malloc | {n} BIN payloads, {iters} iters, {threads} thread(s)");

    let mut best = f64::MAX;
    for _ in 0..iters {
        let t = Instant::now();
        if threads == 1 {
            let mut ok = 0usize;
            for b in bytes.iter() {
                if decode_item_bytes(b).is_some() {
                    ok += 1;
                }
            }
            std::hint::black_box(ok);
        } else {
            let mut hs = Vec::new();
            for _ in 0..threads {
                let bs = bytes.clone();
                hs.push(std::thread::spawn(move || {
                    let mut ok = 0usize;
                    for b in bs.iter() {
                        if decode_item_bytes(b).is_some() {
                            ok += 1;
                        }
                    }
                    ok
                }));
            }
            for h in hs {
                std::hint::black_box(h.join().unwrap());
            }
        }
        // Per-thread wall time: every thread decodes the whole set, so the
        // elapsed time IS one thread's latency for n auctions under contention.
        best = best.min(t.elapsed().as_secs_f64());
    }
    let per = best * 1e6 / n as f64;
    println!(
        "  {:>7.2} ms for {n} auctions = {per:>6.1} us each",
        best * 1000.0
    );
    println!(
        "  extrapolated to prod's ~150 new BINs per dump: {:.2} ms",
        per * 150.0 / 1000.0
    );
}
