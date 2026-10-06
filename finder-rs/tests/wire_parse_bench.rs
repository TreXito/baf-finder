//! Timing probe for the per-auction wire parse in the drain loop.
//! `cargo test -p finder-rs --release --test wire_parse_bench -- --nocapture`
//!
//! detect.rs:398 runs `serde_json::from_slice::<WireAuction>` on EVERY auction
//! span, including the multi-KB base64 `item_bytes`, just to read bin/uuid/price.
//! This measures what that costs and what borrowing instead would save.
use serde::Deserialize;
use std::borrow::Cow;
use std::time::Instant;

/// Mirror of detect.rs WireAuction (owned Strings), as prod runs it today.
#[derive(Deserialize)]
struct Owned {
    #[allow(dead_code)]
    uuid: String,
    #[serde(default)]
    #[allow(dead_code)]
    auctioneer: Option<String>,
    #[serde(default)]
    bin: bool,
    #[serde(default)]
    starting_bid: f64,
    #[serde(default)]
    #[allow(dead_code)]
    item_name: String,
    #[serde(default)]
    #[allow(dead_code)]
    item_bytes: String,
    #[serde(default)]
    #[allow(dead_code)]
    start: f64,
}

/// Same fields, borrowed from the input buffer instead of allocated.
#[derive(Deserialize)]
struct Borrowed<'a> {
    #[allow(dead_code)]
    #[serde(borrow)]
    uuid: Cow<'a, str>,
    #[serde(default, borrow)]
    #[allow(dead_code)]
    auctioneer: Option<Cow<'a, str>>,
    #[serde(default)]
    bin: bool,
    #[serde(default)]
    starting_bid: f64,
    #[serde(default, borrow)]
    #[allow(dead_code)]
    item_name: Cow<'a, str>,
    #[serde(default, borrow)]
    #[allow(dead_code)]
    item_bytes: Cow<'a, str>,
    #[serde(default)]
    #[allow(dead_code)]
    start: f64,
}

/// Split the `auctions` array into raw per-object byte spans, the way the
/// streaming Extractor hands them to the decode loop.
fn spans(buf: &[u8]) -> Vec<(usize, usize)> {
    let needle = b"\"auctions\":[";
    let mut i = buf.windows(needle.len()).position(|w| w == needle).unwrap() + needle.len();
    let (mut depth, mut in_str, mut esc, mut start) = (0i32, false, false, 0usize);
    let mut out = Vec::new();
    while i < buf.len() {
        let c = buf[i];
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' => {
                    if depth == 0 {
                        start = i;
                    }
                    depth += 1;
                }
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        out.push((start, i + 1));
                    }
                }
                b']' if depth == 0 => break,
                _ => {}
            }
        }
        i += 1;
    }
    out
}

#[test]
fn wire_parse_timing() {
    let buf =
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/fixture-page0.json")).expect("fixture");
    let sp = spans(&buf);
    println!("page bytes: {}  auction spans: {}", buf.len(), sp.len());

    const REPS: u32 = 20;

    let t = Instant::now();
    let mut nb = 0usize;
    for _ in 0..REPS {
        for &(s, e) in &sp {
            if let Ok(a) = serde_json::from_slice::<Owned>(&buf[s..e]) {
                if a.bin && a.starting_bid > 0.0 {
                    nb += 1;
                }
            }
        }
    }
    let owned_ms = t.elapsed().as_secs_f64() * 1e3 / REPS as f64;

    let t = Instant::now();
    let mut nb2 = 0usize;
    for _ in 0..REPS {
        for &(s, e) in &sp {
            if let Ok(a) = serde_json::from_slice::<Borrowed>(&buf[s..e]) {
                if a.bin && a.starting_bid > 0.0 {
                    nb2 += 1;
                }
            }
        }
    }
    let borrowed_ms = t.elapsed().as_secs_f64() * 1e3 / REPS as f64;

    assert_eq!(nb, nb2, "both parsers must agree on BIN count");
    println!("OWNED    (prod today) {:>7.2} ms per page", owned_ms);
    println!("COW-BORROWED         {:>7.2} ms per page", borrowed_ms);
    println!(
        "saving               {:>7.2} ms per page  ({:.0}% faster)",
        owned_ms - borrowed_ms,
        100.0 * (owned_ms - borrowed_ms) / owned_ms
    );
}
