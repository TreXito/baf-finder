//! What is `decode_item_bytes` throwing away, and is it worth money?
//!
//! The parser walks `root.i[0].tag.ExtraAttributes` and reads a fixed list of
//! keys out of it. Everything else on the item -- the stack `Count` that lives on
//! `i[0]` itself, and any ExtraAttributes key not on that list -- never reaches
//! `ItemAttributes`, so it cannot key, cannot become a feature, and cannot split
//! a price pool. ABICASE's `model` is the case that cost us 8.66M.
//!
//! This walks a live dump with the SAME simdnbt decode the finder uses (an
//! earlier Python probe reported every item as Count=10, which is nonsense --
//! do not resurrect it) and ranks what we discard by the sale volume of the
//! items carrying it, so "460 suspects" becomes a list of actual missing fields.
//!
//!   cargo run --release --example nbt_probe -- [--pages N] [--db PATH]

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use flate2::read::GzDecoder;
use simdnbt::borrow::{NbtCompound, NbtTag};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Cursor, Read};

/// The read-set now lives in `finder_core::nbt`, which is the only place that
/// can be authoritative about it. This probe reports what the parser does NOT
/// read, so a hand-synced copy here made it report unread fields as read --
/// which is how `additional_coins` hid on Midas items until 2026-08-14.
use finder_core::nbt::READ_KEYS;

#[derive(Default)]
struct ItemStat {
    listings: usize,
    /// Count -> how many listings, and the prices seen at that Count.
    counts: BTreeMap<i64, Vec<f64>>,
    /// Distinct values of every unread key, so a key that is constant across an
    /// item (worthless for keying) is distinguishable from one that varies.
    unread_vals: HashMap<String, HashSet<String>>,
}

fn tag_scalar(t: &NbtTag) -> Option<String> {
    if let Some(s) = t.string() {
        return Some(s.to_str().into_owned());
    }
    if let Some(b) = t.byte() {
        return Some(b.to_string());
    }
    if let Some(s) = t.short() {
        return Some(s.to_string());
    }
    if let Some(i) = t.int() {
        return Some(i.to_string());
    }
    if let Some(l) = t.long() {
        return Some(l.to_string());
    }
    if let Some(f) = t.float() {
        return Some(f.to_string());
    }
    if let Some(d) = t.double() {
        return Some(d.to_string());
    }
    None
}

/// Depth-limited hunt for a key by name anywhere under a compound, so a field we
/// do not know the location of (like `model`) is still found.
fn find_key(
    c: &NbtCompound,
    want: &str,
    path: &str,
    depth: usize,
    out: &mut Vec<(String, String)>,
) {
    if depth == 0 {
        return;
    }
    for (k, v) in c.iter() {
        let ks = k.to_str();
        let p = format!("{path}.{ks}");
        if ks == want {
            out.push((
                p.clone(),
                tag_scalar(&v).unwrap_or_else(|| "<compound>".into()),
            ));
        }
        if let Some(sub) = v.compound() {
            find_key(&sub, want, &p, depth - 1, out);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str, default: &str| -> String {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_else(|| default.to_string())
    };
    let max_pages: usize = arg("--pages", "0").parse().unwrap_or(0);
    let db_path = arg("--db", "/root/finder/data/auctions.sqlite");
    // --item ID: dump every NBT field of the first few listings of one item, for
    // when the question is "what COULD have keyed this apart" rather than "what
    // do we discard overall".
    let focus = arg("--item", "");
    let mut focus_shown = 0usize;
    // --group KEY: for the focused item, split live listings by an ExtraAttributes
    // key and show what each group asks. This is the test that decides whether a
    // field is worth keying on: if the groups price alike, keying on it only
    // fragments pools for nothing.
    let group_key = arg("--group", "");
    let mut groups: HashMap<String, Vec<f64>> = HashMap::new();

    // ---- sale volume, so the ranking is by money and not by listing count ----
    let mut sold_n: HashMap<String, i64> = HashMap::new();
    let mut sold_med: HashMap<String, f64> = HashMap::new();
    // --soldtsv PATH: pre-aggregated `item_id \t n \t avg_price`, for running the
    // probe away from the box that holds the 3.9GB auctions.sqlite. Same numbers
    // as the SQL below, just carried over instead of recomputed.
    let sold_tsv = arg("--soldtsv", "");
    if !sold_tsv.is_empty() {
        match std::fs::read_to_string(&sold_tsv) {
            Ok(text) => {
                for line in text.lines() {
                    let mut f = line.split('\t');
                    let (Some(id), Some(n), Some(avg)) = (f.next(), f.next(), f.next()) else {
                        continue;
                    };
                    let (Ok(n), Ok(avg)) = (n.parse::<i64>(), avg.parse::<f64>()) else {
                        continue;
                    };
                    sold_n.insert(id.to_string(), n);
                    sold_med.insert(id.to_string(), avg);
                }
                eprintln!("  sold history: {} distinct items (tsv)", sold_n.len());
            }
            Err(e) => eprintln!("  !! --soldtsv {sold_tsv} unreadable ({e})"),
        }
    }
    match rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) {
        Ok(con) => {
            let mut st = con
                .prepare("SELECT item_id, COUNT(*), AVG(price) FROM sold WHERE price > 0 GROUP BY item_id")
                .expect("sold query");
            let rows = st
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, f64>(2)?,
                    ))
                })
                .expect("sold rows");
            for r in rows.flatten() {
                sold_n.insert(r.0.clone(), r.1);
                sold_med.insert(r.0, r.2);
            }
            eprintln!("  sold history: {} distinct items", sold_n.len());
        }
        Err(e) => eprintln!("  !! no sold history ({e}); ranking by listings only"),
    }

    // ---- walk the live dump ----
    let client = reqwest::blocking::Client::builder()
        .gzip(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("client");

    let mut items: HashMap<String, ItemStat> = HashMap::new();
    let mut i0_keys: HashMap<String, usize> = HashMap::new();
    let mut tag_keys: HashMap<String, usize> = HashMap::new();
    let mut unread_ea: HashMap<String, (usize, HashSet<String>)> = HashMap::new();
    let mut model_hits: HashMap<String, HashMap<String, usize>> = HashMap::new();
    let mut total = 0usize;
    let mut decoded = 0usize;
    let mut stacked = 0usize;

    let mut page = 0usize;
    let mut pages = 1usize;
    while page < pages {
        let url = format!("https://api.hypixel.net/v2/skyblock/auctions?page={page}");
        let body: serde_json::Value = match client.get(&url).send().and_then(|r| r.json()) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  page {page} failed: {e}");
                break;
            }
        };
        if page == 0 {
            pages = body.get("totalPages").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
            if max_pages > 0 {
                pages = pages.min(max_pages);
            }
            eprintln!("  dump: {pages} pages");
        }
        let auctions = match body.get("auctions").and_then(|v| v.as_array()) {
            Some(a) => a,
            None => break,
        };
        for a in auctions {
            total += 1;
            let Some(ib) = a.get("item_bytes").and_then(|v| v.as_str()) else {
                continue;
            };
            let price = a
                .get("starting_bid")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);

            let Ok(buf) = STANDARD.decode(ib) else {
                continue;
            };
            let raw = if buf.len() >= 2 && buf[0] == 0x1f && buf[1] == 0x8b {
                let mut gz = GzDecoder::new(&buf[..]);
                let mut out = Vec::with_capacity(buf.len() * 4);
                if gz.read_to_end(&mut out).is_err() {
                    continue;
                }
                out
            } else {
                buf
            };
            let mut cur = Cursor::new(&raw[..]);
            let Ok(simdnbt::borrow::Nbt::Some(base)) = simdnbt::borrow::read(&mut cur) else {
                continue;
            };
            let Some(list) = base.list("i") else { continue };
            let Some(first) = list.compounds().and_then(|c| c.first()) else {
                continue;
            };
            let Some(tag) = first.compound("tag") else {
                continue;
            };
            let Some(extra) = tag.compound("ExtraAttributes") else {
                continue;
            };
            let Some(id) = extra.string("id").map(|s| s.to_str().into_owned()) else {
                continue;
            };
            decoded += 1;

            // --- the stack count, on i[0] and therefore invisible today ---
            let count = first
                .get("Count")
                .and_then(|t| tag_scalar(&t))
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(1);
            if count > 1 {
                stacked += 1;
            }

            let st = items.entry(id.clone()).or_default();
            st.listings += 1;
            st.counts.entry(count).or_default().push(price);

            for (k, _) in first.iter() {
                *i0_keys.entry(k.to_str().into_owned()).or_default() += 1;
            }
            for (k, _) in tag.iter() {
                *tag_keys.entry(k.to_str().into_owned()).or_default() += 1;
            }
            for (k, v) in extra.iter() {
                let ks = k.to_str().into_owned();
                if READ_KEYS.contains(&ks.as_str()) {
                    continue;
                }
                let e = unread_ea.entry(ks.clone()).or_insert((0, HashSet::new()));
                e.0 += 1;
                e.1.insert(id.clone());
                let val = tag_scalar(&v).unwrap_or_else(|| "<compound>".into());
                st.unread_vals.entry(ks).or_default().insert(val);
            }

            if !group_key.is_empty() && (focus.is_empty() || id == focus) {
                let g = extra
                    .get(&group_key)
                    .and_then(|t| tag_scalar(&t))
                    .unwrap_or_else(|| "<absent>".into());
                // --raw: emit the (value, ask) pair per listing so a numeric key
                // can be banded outside this tool. Grouping on a raw integer like
                // `collected_coins` makes every listing its own singleton group,
                // which answers nothing.
                if args.iter().any(|a| a == "--raw") {
                    let modifier = extra
                        .string("modifier")
                        .map(|s| s.to_str().into_owned())
                        .unwrap_or_else(|| "-".into());
                    println!("RAW\t{id}\t{g}\t{price:.0}\t{modifier}");
                }
                groups.entry(format!("{id}/{g}")).or_default().push(price);
            }

            // --weights: for the focused item, pull the lore-only "Current weight"
            // and print it against the ask, which is the test for whether a
            // lore-derived number is worth keying on.
            if !focus.is_empty() && id == focus && args.iter().any(|a| a == "--weights") {
                if let Some(lore) = tag.compound("display").and_then(|d| d.list("Lore")) {
                    if let Some(strings) = lore.strings() {
                        for l in strings.iter() {
                            let t = l.to_str();
                            if let Some(i) = t.find("Current weight:") {
                                let digits: String = t[i..]
                                    .chars()
                                    .filter(|c| c.is_ascii_digit() || *c == ' ')
                                    .collect();
                                if let Some(w) = digits.split_whitespace().next_back() {
                                    // Decode through the REAL production path so
                                    // this verifies what the finder actually keys
                                    // on, not a re-implementation of it.
                                    let variant = finder_core::nbt::decode_item_bytes(ib)
                                        .map(|a| a.variant)
                                        .unwrap_or_default();
                                    println!(
                                        "WEIGHT\t{w}\t{price:.0}\t{}",
                                        if variant.is_empty() { "-" } else { &variant }
                                    );
                                }
                            }
                        }
                    }
                }
            }
            // --attrs: run the REAL production decode (finder_core's
            // decode_item_bytes, the same call the finder makes on every sweep)
            // over live listings and print the pricing key plus the extras it
            // produced. This is how you prove a newly-read NBT field actually
            // reaches pricing on real data, without waiting for the item to sell.
            // Flag-sensitive: run it with the feature's env var set.
            if !focus.is_empty() && id == focus && args.iter().any(|a| a == "--attrs") {
                if let Some(a) = finder_core::nbt::decode_item_bytes(ib) {
                    let extras: Vec<String> =
                        a.extras.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    println!(
                        "ATTRS\task={price:.0}\tbase_key={}\textras=[{}]",
                        finder_core::price_index::base_key(&a),
                        extras.join(", ")
                    );
                }
                continue;
            }
            if !focus.is_empty()
                && id == focus
                && focus_shown < 6
                && !args.iter().any(|a| a == "--weights")
            {
                focus_shown += 1;
                println!("--- {id} #{focus_shown}  list price {price:.0}  Count={count}");
                let dname = tag
                    .compound("display")
                    .and_then(|d| d.string("Name"))
                    .map(|s| s.to_str().into_owned())
                    .unwrap_or_default();
                println!("    display.Name: {dname}");
                for (k, v) in extra.iter() {
                    let ks = k.to_str();
                    let val = tag_scalar(&v).unwrap_or_else(|| {
                        v.compound()
                            .map(|c| {
                                let ks: Vec<String> =
                                    c.iter().map(|(k, _)| k.to_str().into_owned()).collect();
                                format!("{{{}}}", ks.join(","))
                            })
                            .unwrap_or_else(|| "<list>".into())
                    });
                    let mark = if READ_KEYS.contains(&ks.as_ref()) {
                        " "
                    } else {
                        "*"
                    };
                    println!("   {mark}EA.{ks} = {val}");
                }
                // The lore is where Hypixel renders values that exist nowhere in
                // ExtraAttributes (Loudmouth Bass "Bass Weight" is the case that
                // found this), and `i[0].components` is the modern data-component
                // block we never touch at all. Both are invisible to pricing.
                if let Some(lore) = tag.compound("display").and_then(|d| d.list("Lore")) {
                    if let Some(strings) = lore.strings() {
                        for l in strings.iter().take(40) {
                            println!("    LORE: {}", l.to_str());
                        }
                    }
                }
                if let Some(comp) = first.compound("components") {
                    for (k, v) in comp.iter() {
                        let val = tag_scalar(&v).unwrap_or_else(|| {
                            v.compound()
                                .map(|c| {
                                    let ks: Vec<String> =
                                        c.iter().map(|(k, _)| k.to_str().into_owned()).collect();
                                    format!("{{{}}}", ks.join(","))
                                })
                                .unwrap_or_else(|| "<list>".into())
                        });
                        println!("   *COMP.{} = {}", k.to_str(), val);
                    }
                }
            }

            // `model` is the ABICASE field; find it wherever it lives.
            let mut hits = Vec::new();
            find_key(&tag, "model", "tag", 4, &mut hits);
            for (_, val) in hits {
                *model_hits
                    .entry(id.clone())
                    .or_default()
                    .entry(val)
                    .or_default() += 1;
            }
        }
        page += 1;
    }

    println!();
    println!(
        "=== dump: {total} auctions, {decoded} decoded, {stacked} with Count>1 ({:.1}%)",
        stacked as f64 / decoded.max(1) as f64 * 100.0
    );

    // ---- structure we ignore ----
    let show = |title: &str, m: &HashMap<String, usize>| {
        let mut v: Vec<_> = m.iter().collect();
        v.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        println!();
        println!("=== {title}");
        for (k, n) in v.iter().take(15) {
            println!(
                "  {:<28}{:>9}  {:>5.1}%",
                k,
                n,
                **n as f64 / decoded.max(1) as f64 * 100.0
            );
        }
    };
    show("keys on i[0] (only `tag` is read)", &i0_keys);
    show("keys on tag (only ExtraAttributes is read)", &tag_keys);

    // ---- unread ExtraAttributes keys, ranked by the sale volume behind them ----
    let mut ranked: Vec<(f64, String, usize, usize, i64)> = unread_ea
        .iter()
        .map(|(k, (n, ids))| {
            let vol: i64 = ids.iter().filter_map(|i| sold_n.get(i)).sum();
            let value: f64 = ids
                .iter()
                .filter_map(|i| Some(*sold_n.get(i)? as f64 * *sold_med.get(i)?))
                .sum();
            (value, k.clone(), *n, ids.len(), vol)
        })
        .collect();
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    println!();
    println!("=== ExtraAttributes keys we never read, by sale value of the items carrying them");
    println!(
        "  {:<30}{:>9}{:>8}{:>12}{:>16}",
        "key", "listings", "items", "sales", "sold value"
    );
    for (value, k, n, nids, vol) in ranked.iter().take(30) {
        println!(
            "  {:<30}{:>9}{:>8}{:>12}{:>15.0}B",
            k,
            n,
            nids,
            vol,
            value / 1e9
        );
    }

    // ---- stacks: which items pool a 1x and a 64x under one median ----
    let mut stackers: Vec<(f64, &String, &ItemStat)> = items
        .iter()
        .filter(|(_, s)| s.counts.keys().any(|c| *c > 1) && s.counts.len() > 1)
        .map(|(id, s)| {
            let vol = *sold_n.get(id).unwrap_or(&0) as f64;
            let med = *sold_med.get(id).unwrap_or(&0.0);
            (vol * med, id, s)
        })
        .collect();
    stackers.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    println!();
    println!("=== items listed at MORE THAN ONE stack size (one pool, many quantities)");
    println!(
        "  {:<30}{:>8}{:>10}{:>34}",
        "item", "sales", "listings", "count -> median list price"
    );
    for (_, id, s) in stackers.iter().take(25) {
        let mut parts = Vec::new();
        for (c, ps) in s.counts.iter().take(6) {
            let mut v = ps.clone();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            parts.push(format!("{}x:{:.0}", c, v[v.len() / 2]));
        }
        println!(
            "  {:<30}{:>8}{:>10}   {}",
            &id[..id.len().min(29)],
            sold_n.get(*id).copied().unwrap_or(0),
            s.listings,
            parts.join("  ")
        );
    }

    if !groups.is_empty() {
        let mut gv: Vec<_> = groups.iter().collect();
        gv.sort_by_key(|(_, ps)| std::cmp::Reverse(ps.len()));
        println!();
        println!("=== live listings grouped by ExtraAttributes.{group_key}");
        println!(
            "  {:<44}{:>7}{:>15}{:>15}{:>15}",
            "item / value", "n", "min ask", "median ask", "max ask"
        );
        for (g, ps) in gv.iter().take(30) {
            let mut v = (*ps).clone();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            println!(
                "  {:<44}{:>7}{:>15.0}{:>15.0}{:>15.0}",
                &g[..g.len().min(43)],
                v.len(),
                v[0],
                v[v.len() / 2],
                v[v.len() - 1]
            );
        }
    }

    // ---- model ----
    println!();
    println!("=== `model` sightings");
    if model_hits.is_empty() {
        println!("  none in this dump");
    } else {
        let mut mv: Vec<_> = model_hits.iter().collect();
        mv.sort_by_key(|(_, m)| std::cmp::Reverse(m.values().sum::<usize>()));
        for (id, vals) in mv.iter().take(10) {
            let mut vs: Vec<_> = vals.iter().collect();
            vs.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
            let shown: Vec<String> = vs.iter().take(8).map(|(v, n)| format!("{v}:{n}")).collect();
            println!(
                "  {:<22} {} listings, {} distinct: {}",
                id,
                vals.values().sum::<usize>(),
                vals.len(),
                shown.join("  ")
            );
        }
    }
}
