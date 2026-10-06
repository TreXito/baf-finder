// Rust side of the TS-vs-Rust finding-speed head-to-head. Times three stages on
// the same 30,942-ref slice + 1000-candidate dump the goldens use:
//   1. PriceIndex build (the boot "index rebuild")
//   2. ModifierModel rebuild
//   3. per-dump FINDING: build the live-BIN map + run the full sniper pipeline
//      (screen + snipe + median + dominance + lbin) over 1000 candidates.
use finder_core::bazaar::Bazaar;
use finder_core::modifier_model::ModifierModel;
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::{PriceIndex, Reference};
use finder_core::sniper::*;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

fn stats(mut xs: Vec<f64>) -> (f64, f64) {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (xs[0], xs[xs.len() / 2]) // (min, median)
}

fn main() {
    let g = concat!(env!("CARGO_MANIFEST_DIR"), "/../goldens");
    let doc: Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{g}/sniper/dump.json")).unwrap())
            .unwrap();
    let now_ms = doc["pinnedNowMs"].as_i64().unwrap();
    let last_updated = doc["source"]["lastUpdated"].as_f64().unwrap();
    let prices: HashMap<String, f64> = doc["bazaar"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_f64().unwrap()))
        .collect();
    let refs: Vec<Reference> = std::fs::read_to_string(format!("{g}/priceIndex/refs.jsonl"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let mk_bazaar = || Bazaar::from_prices(prices.clone(), now_ms);

    // Reconstruct decoded auctions once (finalKey needs an index; build a scratch one).
    let scratch = PriceIndex::build(refs.clone(), mk_bazaar(), now_ms);
    let decoded: Vec<DecodedAuction> = doc["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let cand = &c["candidate"];
            let attrs: ItemAttributes = serde_json::from_value(cand["attrs"].clone()).unwrap();
            let key = scratch.final_key(&attrs);
            DecodedAuction {
                a: ActiveAuction {
                    uuid: cand["uuid"].as_str().unwrap().to_string(),
                    starting_bid: cand["startingBid"].as_f64().unwrap(),
                    auctioneer: cand["auctioneer"].as_str().map(String::from),
                    item_name: cand["itemName"].as_str().unwrap_or("").to_string(),
                },
                attrs,
                key,
            }
        })
        .collect();

    let runs = 12usize;
    let (mut t_build, mut t_model, mut t_find) = (vec![], vec![], vec![]);
    let mut flips_seen = 0usize;
    for _ in 0..runs {
        let t = Instant::now();
        let idx = PriceIndex::build(refs.clone(), mk_bazaar(), now_ms);
        t_build.push(t.elapsed().as_secs_f64() * 1000.0);

        let t = Instant::now();
        let model = ModifierModel::rebuild(&refs, &idx, now_ms);
        t_model.push(t.elapsed().as_secs_f64() * 1000.0);

        let t = Instant::now();
        // per-dump finding: live-BIN map + full pipeline
        let mut bykey: HashMap<String, Vec<Bin>> = HashMap::new();
        for d in &decoded {
            let price =
                (d.a.starting_bid - idx.minor_feature_value(&d.attrs)).max(d.a.starting_bid * 0.5);
            bykey.entry(d.key.clone()).or_default().push(Bin {
                uuid: d.a.uuid.clone(),
                price,
            });
        }
        for l in bykey.values_mut() {
            l.sort_by(|x, y| x.price.partial_cmp(&y.price).unwrap());
        }
        let mut seen = HashSet::new();
        let mut relist = RelistTracker::default();
        let mut fired = 0usize;
        let mut lbin_c: Vec<DecodedAuction> = Vec::new();
        for d in &decoded {
            let _ = screen_auction(d, &idx, &model);
            if eval_clean_snipe(
                d,
                &idx,
                &bykey,
                &mut seen,
                &mut relist,
                last_updated,
                now_ms as f64,
            )
            .is_some()
            {
                fired += 1;
                continue;
            }
            let (flip, priceable) = eval_median_flip(
                d,
                &idx,
                &model,
                &bykey,
                &mut seen,
                &mut relist,
                last_updated,
                now_ms as f64,
            );
            if flip.is_some() {
                fired += 1;
                continue;
            }
            if !priceable {
                lbin_c.push(d.clone());
            }
        }
        fired += eval_dominance_flips(
            &lbin_c,
            &bykey,
            &mut seen,
            &mut relist,
            last_updated,
            &idx,
            now_ms as f64,
        )
        .len();
        fired += eval_lbin_flips(
            &lbin_c,
            &bykey,
            &mut seen,
            &mut relist,
            last_updated,
            &idx,
            now_ms as f64,
        )
        .len();
        t_find.push(t.elapsed().as_secs_f64() * 1000.0);
        flips_seen = fired;
    }

    let (b_min, b_med) = stats(t_build);
    let (m_min, m_med) = stats(t_model);
    let (f_min, f_med) = stats(t_find);
    println!(
        "refs={} candidates={} flips={} runs={runs}\n",
        refs.len(),
        decoded.len(),
        flips_seen
    );
    println!(
        "{:<28} min {:>9.2} ms   median {:>9.2} ms",
        "index build", b_min, b_med
    );
    println!(
        "{:<28} min {:>9.2} ms   median {:>9.2} ms",
        "model rebuild", m_min, m_med
    );
    println!(
        "{:<28} min {:>9.2} ms   median {:>9.2} ms",
        "per-dump finding (1000)", f_min, f_med
    );
    println!(
        "{:<28} min {:>9.3} ms   median {:>9.3} ms",
        "  → per candidate",
        f_min / decoded.len() as f64,
        f_med / decoded.len() as f64
    );
}
