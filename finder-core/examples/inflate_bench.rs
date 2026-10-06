//! Where does the inflate microsecond budget actually go?
//!
//! `decode_bench` says base64+inflate is ~85% of `decode_item_bytes`. This splits
//! that into base64, the per-call `reset`, and the inflate proper, plus what a
//! fresh `Decompress` costs, so the next change is aimed rather than guessed.
//!
//! usage: inflate_bench <page0.json> [iters]

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use flate2::{Decompress, FlushDecompress, Status};
use std::time::Instant;

fn gzip_body_offset(buf: &[u8]) -> Option<usize> {
    if buf.len() < 18 || buf[2] != 8 {
        return None;
    }
    let flg = buf[3];
    let mut i = 10usize;
    if flg & 0b100 != 0 {
        i += 2 + u16::from_le_bytes(buf.get(i..i + 2)?.try_into().ok()?) as usize;
    }
    if flg & 0b1000 != 0 {
        i += buf.get(i..)?.iter().position(|&b| b == 0)? + 1;
    }
    if flg & 0b1_0000 != 0 {
        i += buf.get(i..)?.iter().position(|&b| b == 0)? + 1;
    }
    if flg & 0b10 != 0 {
        i += 2;
    }
    if i + 8 > buf.len() {
        None
    } else {
        Some(i)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).map(String::as_str).unwrap_or("page0.json");
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);

    let body = std::fs::read(path).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let b64: Vec<String> = v["auctions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["bin"].as_bool().unwrap_or(false))
        .filter_map(|a| a["item_bytes"].as_str().map(str::to_string))
        .collect();
    let n = b64.len();
    println!("{n} BIN payloads, {iters} iters, taking the MIN pass (least noise)\n");

    // Pre-decode base64 once so inflate can be timed on its own.
    let gz: Vec<Vec<u8>> = b64.iter().filter_map(|s| STANDARD.decode(s).ok()).collect();
    let mean_in: usize = gz.iter().map(|g| g.len()).sum::<usize>() / gz.len();

    let mut best = [f64::MAX; 5];
    let label = [
        "base64 decode_slice",
        "Decompress::new(false)",
        "reset(false) only",
        "decompress_vec only",
        "reset + decompress_vec",
    ];

    for _ in 0..iters {
        // 0: base64 into a reused buffer
        let mut buf = vec![0u8; 8192];
        let t = Instant::now();
        for s in &b64 {
            let _ = STANDARD.decode_slice(s.as_bytes(), &mut buf);
        }
        best[0] = best[0].min(t.elapsed().as_secs_f64());

        // 1: constructing a fresh state per payload (what the old code did)
        let t = Instant::now();
        for _ in 0..n {
            let d = Decompress::new(false);
            std::hint::black_box(&d);
        }
        best[1] = best[1].min(t.elapsed().as_secs_f64());

        // 2: reset alone
        let mut d = Decompress::new(false);
        let t = Instant::now();
        for _ in 0..n {
            d.reset(false);
        }
        best[2] = best[2].min(t.elapsed().as_secs_f64());

        // 3: inflate alone, one fresh state per payload so no reset is needed
        let mut out = Vec::with_capacity(16384);
        let t = Instant::now();
        for g in &gz {
            let Some(off) = gzip_body_offset(g) else {
                continue;
            };
            let mut d = Decompress::new(false);
            out.clear();
            let _ = d.decompress_vec(&g[off..], &mut out, FlushDecompress::Finish);
        }
        let with_new = t.elapsed().as_secs_f64();
        best[3] = best[3].min(with_new);

        // 4: the shipped shape — one state, reset per payload
        let mut d = Decompress::new(false);
        let t = Instant::now();
        let mut okc = 0;
        for g in &gz {
            let Some(off) = gzip_body_offset(g) else {
                continue;
            };
            out.clear();
            d.reset(false);
            if let Ok(Status::StreamEnd) =
                d.decompress_vec(&g[off..], &mut out, FlushDecompress::Finish)
            {
                okc += 1;
            }
        }
        best[4] = best[4].min(t.elapsed().as_secs_f64());
        std::hint::black_box(okc);
    }

    println!("  {:<24}{:>12}{:>14}", "step", "us/payload", "ms per 150");
    for (i, l) in label.iter().enumerate() {
        let per = best[i] * 1e6 / n as f64;
        println!("  {l:<24}{per:>12.2}{:>14.2}", per * 150.0 / 1000.0);
    }
    println!("\n  mean gzipped input: {mean_in} bytes");
    println!("  note: step 3 includes a fresh Decompress each time, so");
    println!("        (step 3 - step 1) is the true inflate cost.");
}
