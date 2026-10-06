//! Per-stage cost of the hot decode path, in microseconds per auction.
//!
//! `decCpuMs` on prod is ~18ms for ~150 new BINs = ~120us each for a ~1.4KB NBT
//! blob. This splits that into the four things `decode_item_bytes` actually does
//! so an optimisation can be attributed instead of guessed at:
//!
//!   base64  STANDARD.decode            -> allocates a Vec per auction
//!   inflate GzDecoder::read_to_end     -> allocates a Vec per auction
//!   nbt     simdnbt::borrow::read      -> zero-copy over the inflated bytes
//!   attrs   attrs_from_extra           -> IndexMaps + to_lowercase() Strings
//!
//! usage: decode_bench <page0.json> [iters]
//!
//! Run it the same way prod is built or the numbers mean nothing:
//!   RUSTFLAGS="-C target-cpu=znver2" cargo run --release --example decode_bench -- page0.json

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use finder_core::nbt::{attrs_from_extra, decode_item_bytes};
use flate2::read::GzDecoder;
use std::io::{Cursor, Read};
use std::time::Instant;

fn pct(v: &mut Vec<f64>, p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p) as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).map(String::as_str).unwrap_or("page0.json");
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);

    let body = std::fs::read(path).expect("read page0");
    let v: serde_json::Value = serde_json::from_slice(&body).expect("parse page0");
    let aucs = v["auctions"].as_array().expect("auctions array");
    // Only BINs reach decode_item_bytes on the hot path.
    let bytes: Vec<String> = aucs
        .iter()
        .filter(|a| a["bin"].as_bool().unwrap_or(false))
        .filter_map(|a| a["item_bytes"].as_str().map(str::to_string))
        .collect();
    println!(
        "{} BIN item_bytes from {path}, {iters} iters\n",
        bytes.len()
    );

    // ---- whole-path, the number that must come down ----
    let mut whole = Vec::new();
    let mut ok = 0usize;
    for _ in 0..iters {
        let t = Instant::now();
        for b in &bytes {
            if decode_item_bytes(b).is_some() {
                ok += 1;
            }
        }
        whole.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let n = bytes.len() as f64;
    let w = pct(&mut whole, 0.5);
    println!(
        "  decode_item_bytes   {w:>8.2} ms / {} auctions = {:>7.1} us each",
        bytes.len(),
        w * 1000.0 / n
    );
    println!(
        "                      (decoded {} / {})",
        ok / iters,
        bytes.len()
    );
    println!(
        "  extrapolated to prod's ~150 new BINs per dump: {:.1} ms\n",
        w / n * 150.0
    );

    // ---- stage by stage, against the CURRENT path ----
    // base64+inflate share a reusable scratch, so they are timed together via the
    // bench hook; nbt+attrs are timed against the same inflated bytes.
    let mut t_inf = 0.0;
    let mut raw_total = 0usize;
    for _ in 0..iters {
        for b in &bytes {
            let t = Instant::now();
            let len = finder_core::nbt::bench_inflate_len(b);
            t_inf += t.elapsed().as_secs_f64();
            raw_total += len.unwrap_or(0);
        }
    }

    // Pre-inflate once so nbt/attrs are measured without the inflate in the way.
    let inflated: Vec<Vec<u8>> = bytes
        .iter()
        .filter_map(|b| {
            let buf = STANDARD.decode(b).ok()?;
            let mut gz = flate2::read::GzDecoder::new(&buf[..]);
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut gz, &mut out).ok()?;
            Some(out)
        })
        .collect();
    let mut t_nbt = 0.0;
    let mut t_att = 0.0;
    for _ in 0..iters {
        for raw in &inflated {
            let t = Instant::now();
            let mut cursor = Cursor::new(&raw[..]);
            let base = match simdnbt::borrow::read(&mut cursor) {
                Ok(simdnbt::borrow::Nbt::Some(b)) => b,
                _ => continue,
            };
            t_nbt += t.elapsed().as_secs_f64();

            let t = Instant::now();
            if let Some(items) = base.list("i") {
                if let Some(first) = items.compounds().and_then(|c| c.first()) {
                    if let Some(tag) = first.compound("tag") {
                        if let Some(extra) = tag.compound("ExtraAttributes") {
                            let _ = attrs_from_extra(&extra);
                        }
                    }
                }
            }
            t_att += t.elapsed().as_secs_f64();
        }
    }
    let per = |s: f64| s * 1e6 / (n * iters as f64);
    let tot = t_inf + t_nbt + t_att;
    println!("  stage                us/auction     share    ms per 150 BINs");
    for (name, s) in [
        ("base64+inflate", t_inf),
        ("nbt read", t_nbt),
        ("attrs_from_extra", t_att),
    ] {
        println!(
            "  {name:<18} {:>9.1} {:>9.1}% {:>15.2}",
            per(s),
            s / tot * 100.0,
            per(s) * 150.0 / 1000.0
        );
    }
    println!(
        "  {:<18} {:>9.1} {:>9}  {:>14.2}",
        "TOTAL",
        per(tot),
        "",
        per(tot) * 150.0 / 1000.0
    );
    println!(
        "\n  mean inflated NBT size: {} bytes",
        raw_total / (bytes.len() * iters)
    );
    println!(
        "  simd: {}",
        if cfg!(target_feature = "avx2") {
            "avx2 ENABLED"
        } else {
            "avx2 OFF (baseline x86-64)"
        }
    );
}
