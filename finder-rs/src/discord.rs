//! Discord webhook posting — verbatim port of discord.ts (the "found" channel).
//!
//! This is what the user actually watches to see the finder work. It was
//! previously filed as a "post-cutover fast-follow"; that was wrong, it is part
//! of the TS behavior the port is supposed to clone.
//!
//! Rate limits: Discord tolerates only a few posts per few seconds, so a burst
//! of flips gets 429'd. Queue posts, space them out, honour `retry_after`.

use finder_core::nbt::fmt_js_f64;
use finder_core::price_index::KeyStats;
use finder_core::sniper::Flip;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::ws_server::PushResult;

const MAX_QUEUE: usize = 200;
const MIN_GAP_MS: u64 = 1300;

/// JS `Number.prototype.toFixed`: round half away from zero, fixed decimals.
/// Rust's `{:.n}` rounds half-to-even, which would differ on exact .5 ties.
fn to_fixed(n: f64, d: u32) -> String {
    let m = 10f64.powi(d as i32);
    let r = (n * m).round() / m;
    format!("{:.*}", d as usize, r)
}

/// Port of `coins`: 1.2M / 340k compact formatting.
fn coins(n: f64) -> String {
    let a = n.abs();
    if a >= 1e9 {
        format!("{}B", to_fixed(n / 1e9, 2))
    } else if a >= 1e6 {
        format!("{}M", to_fixed(n / 1e6, 2))
    } else if a >= 1e3 {
        format!("{}k", to_fixed(n / 1e3, 1))
    } else {
        // TS: String(Math.round(n)) — JS rounds .5 toward +Infinity.
        fmt_js_f64((n + 0.5).floor())
    }
}

/// Port of `attrLine`.
fn attr_line(f: &Flip) -> String {
    let a = &f.attrs;
    let mut parts: Vec<String> = Vec::new();
    for (k, v) in a.attributes.iter() {
        parts.push(format!("{k} {}", fmt_js_f64(*v)));
    }
    // TS `if (a.upgradeLevel)` is a truthiness check, so 0 is skipped.
    if let Some(u) = a.upgrade_level {
        if u != 0.0 {
            parts.push(format!("⭐{}", fmt_js_f64(u)));
        }
    }
    if a.recombobulated {
        parts.push("recomb".to_string());
    }
    if parts.is_empty() {
        "base".to_string()
    } else {
        parts.join(" • ")
    }
}

/// Port of `confidenceLabel`.
fn confidence_label(c: f64) -> String {
    let pct = (c * 100.0 + 0.5).floor(); // JS Math.round
    let dots = if c >= 0.7 {
        "🟢"
    } else if c >= 0.4 {
        "🟡"
    } else {
        "🔴"
    };
    format!("{dots} {}%", fmt_js_f64(pct))
}

fn field(name: &str, value: String, inline: bool) -> Value {
    json!({ "name": name, "value": value, "inline": inline })
}

/// Port of `buildEmbed`. Field ORDER matters: it is what the channel looks like.
fn build_embed(f: &Flip, ws: Option<&PushResult>, ctx: FoundCtx) -> Value {
    let now_ms = ctx.exact_ms;
    let c = f.confidence;
    let color: i64 = if c >= 0.7 {
        0x2ecc71
    } else if c >= 0.4 {
        0xf1c40f
    } else {
        0xe67e22
    };
    let cofl = format!("https://sky.coflnet.com/auction/{}", f.uuid);

    let mut fields = vec![
        field("Finder", format!("`{}`", f.finder), true),
        field("Auction", "`BIN`".into(), true),
        field("Confidence", confidence_label(c), true),
        field("Buy", coins(f.price), true),
        field("Sells for", coins(f.reference), true),
        field("Profit", coins(f.profit), true),
        field("ROI", format!("{}%", to_fixed(f.roi_pct, 0)), true),
        field("Samples", fmt_js_f64(f.samples as f64), true),
        field("Guard", f.guard.clone(), true),
    ];
    // Median-only liquidity detail (lbin flips have no sold history).
    if let Some(ms) = &f.median_stats {
        let ms: &KeyStats = ms;
        fields.push(field(
            "Volume",
            format!("{}/day", to_fixed(ms.volume_per_day, 1)),
            true,
        ));
        fields.push(field(
            "Spread",
            format!("±{}%", fmt_js_f64((ms.spread_pct * 100.0 + 0.5).floor())),
            true,
        ));
        fields.push(field(
            "Last sold",
            if ms.last_sold_ago_h < 1.0 {
                "<1h ago".to_string()
            } else {
                format!("{}h ago", to_fixed(ms.last_sold_ago_h, 0))
            },
            true,
        ));
    }
    // TS: new Date(ms).toISOString().replace('T',' ').replace('Z',' UTC')
    // EXACT emit instant, not the sweep start.
    let found_iso = iso_ms(ctx.exact_ms).replace('T', " ").replace('Z', " UTC");
    // How long until the PUBLIC dump could show this — our actual edge, and the
    // same number whether or not the auction is currently buyable.
    let lead = ctx
        .api_visible_at_ms
        .map(|t| t - ctx.exact_ms as f64)
        .filter(|l| *l > 0.0);
    let found_detail = match (ctx.purchase_at_ms, lead) {
        // BED: pre-API and not yet buyable. We hold it and release near the lift.
        (Some(p), l) => {
            let wait = (p - ctx.exact_ms as f64).max(0.0);
            format!(
                "`{found_iso}` — 🛏 **BED**, buyable in {}s{}",
                to_fixed(wait / 1000.0, 1),
                match l {
                    Some(l) => format!(" · **−{} ms PRE-API**", fmt_js_f64(l.round())),
                    None => String::new(),
                }
            )
        }
        // PRE-API but already buyable: the best case, act on it right now.
        (None, Some(l)) => format!(
            "`{found_iso}` — ⚡ **−{} ms PRE-API** (buyable NOW, dump is {}s behind)",
            fmt_js_f64(l.round()),
            to_fixed(l / 1000.0, 1)
        ),
        // Ordinary dump flip.
        (None, None) => format!(
            "`{found_iso}` — {} ms after dump release",
            fmt_js_f64(f.found_after_refresh_ms)
        ),
    };
    fields.push(field("Found", found_detail, false));
    fields.push(field("Attributes", attr_line(f), false));
    if let Some(w) = ws {
        let v = match &w.mismatch {
            None => format!(
                "✅ matched settings — pushed to {} bot{}",
                w.delivered,
                if w.delivered == 1 { "" } else { "s" }
            ),
            Some(m) => format!("❌ not pushed — {m}"),
        };
        fields.push(field("Client", v, false));
    }
    fields.push(field("Key", format!("`{}`", f.key), false));
    fields.push(field(
        "Trace ID",
        format!("`{}-{}`", f.uuid, f.finder),
        false,
    ));
    fields.push(field(
        "Open",
        format!("[sky.coflnet.com]({cofl}) • `/viewauction {}`", f.uuid),
        false,
    ));

    json!({
        "title": if ctx.purchase_at_ms.is_some() {
            format!("🛏 {} (BED — pre-API)", f.item_name)
        } else if ctx.is_pre_api() {
            format!("⚡ {} (PRE-API)", f.item_name)
        } else {
            format!("💰 {}", f.item_name)
        },
        "url": cofl,
        "color": color,
        "fields": fields,
        "timestamp": iso_ms(now_ms),
    })
}

/// JS `new Date(ms).toISOString()` → 2026-07-15T22:37:43.114Z
fn iso_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Context the `Flip` struct cannot carry, captured at EMIT time.
#[derive(Clone, Copy)]
pub struct FoundCtx {
    /// Wall-clock instant we actually emitted this flip.
    ///
    /// ⚠️ NOT `Flip::found_at_ms` — that is the SWEEP START, shared by every flip
    /// in the sweep, and runs hundreds of ms early. And NOT the post time either:
    /// the Discord queue is drained asynchronously with a gap between posts, so
    /// stamping in `post_one` would report the webhook's schedule, not ours.
    pub exact_ms: i64,
    /// `start + BED_GRACE_MS` when the auction is still inside its grace window.
    /// `Some` means this is a BED: nobody can buy it YET.
    pub purchase_at_ms: Option<f64>,
    /// When the public dump would first show this auction — i.e. when our edge
    /// expires. ⚠️ NOT the same thing as a bed: the dump publishes only every
    /// 60s, so an auction can be past its 20s grace (buyable NOW) and still
    /// invisible to every dump-reader for another ~40s. That is pre-API without
    /// being a bed, and it is the BETTER case — we can act on it immediately.
    pub api_visible_at_ms: Option<f64>,
}

impl FoundCtx {
    /// Did we get this before the public dump could show it, bed or not?
    pub fn is_pre_api(&self) -> bool {
        self.purchase_at_ms.is_some()
            || self
                .api_visible_at_ms
                .is_some_and(|t| t > self.exact_ms as f64)
    }
}

type Item = (Flip, Option<PushResult>, FoundCtx);

#[derive(Clone)]
pub struct Discord {
    url: String,
    queue: Arc<Mutex<VecDeque<Item>>>,
}

impl Discord {
    /// `url` empty = disabled (TS logs a warn per flip and drops it).
    pub fn new(url: String) -> Self {
        Discord {
            url,
            queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub fn enabled(&self) -> bool {
        !self.url.is_empty()
    }

    /// Port of `enqueueFlip`. Sync + non-blocking: safe to call from the loop
    /// thread on the latency path. The drain task does the posting.
    pub fn enqueue(&self, flip: &Flip, ws: Option<&PushResult>, ctx: FoundCtx) {
        if !self.enabled() {
            eprintln!("DISCORD_WEBHOOK_URL not set — flip not sent");
            return;
        }
        let mut q = self.queue.lock().unwrap();
        if q.len() >= MAX_QUEUE {
            q.pop_front(); // stale flips are worthless; drop oldest
        }
        q.push_back((flip.clone(), ws.cloned(), ctx));
    }

    /// Spawn the drain task on the tokio runtime. Mirrors TS's single-flight
    /// processQueue: one post at a time, MIN_GAP_MS between.
    pub fn spawn(&self, rt: &tokio::runtime::Runtime) {
        let url = self.url.clone();
        let queue = self.queue.clone();
        if url.is_empty() {
            return;
        }
        rt.spawn(async move {
            let client = reqwest::Client::new();
            loop {
                let next = { queue.lock().unwrap().pop_front() };
                match next {
                    Some((flip, ws, ctx)) => {
                        post_one(&client, &url, &flip, ws.as_ref(), ctx).await;
                        tokio::time::sleep(Duration::from_millis(MIN_GAP_MS)).await;
                    }
                    // TS's processQueue exits when drained and is restarted by the
                    // next enqueue; polling idle is the same behavior, simpler.
                    None => tokio::time::sleep(Duration::from_millis(100)).await,
                }
            }
        });
    }
}

/// Port of `postOne`: 3 attempts, honour 429 retry_after, warn on non-ok.
async fn post_one(
    client: &reqwest::Client,
    url: &str,
    flip: &Flip,
    ws: Option<&PushResult>,
    ctx: FoundCtx,
) {
    let body = json!({
        "username": format!("TreXito-{}", flip.finder),
        "embeds": [build_embed(flip, ws, ctx)],
    });
    for _ in 0..3 {
        match client.post(url).json(&body).send().await {
            Ok(res) if res.status().as_u16() == 429 => {
                let retry_after = res
                    .json::<Value>()
                    .await
                    .ok()
                    .and_then(|j| j.get("retry_after").and_then(|v| v.as_f64()))
                    .unwrap_or(1.0);
                let wait = ((retry_after * 1000.0) as u64).clamp(1000, 10_000);
                eprintln!("discord 429 — backing off {wait}ms");
                tokio::time::sleep(Duration::from_millis(wait)).await;
                continue;
            }
            Ok(res) => {
                if !res.status().is_success() {
                    eprintln!("discord webhook rejected: {}", res.status());
                }
                return;
            }
            Err(e) => {
                eprintln!("discord webhook failed: {e}");
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coins_matches_ts() {
        // TS: >=1e9 -> B(2dp), >=1e6 -> M(2dp), >=1e3 -> k(1dp), else round
        assert_eq!(coins(1_400_000_000.0), "1.40B");
        assert_eq!(coins(11_863_422.6), "11.86M");
        assert_eq!(coins(75_000_000.0), "75.00M");
        assert_eq!(coins(340_000.0), "340.0k");
        assert_eq!(coins(999.0), "999");
        assert_eq!(coins(0.0), "0");
    }

    #[test]
    fn confidence_label_thresholds() {
        assert_eq!(confidence_label(0.95), "🟢 95%");
        assert_eq!(confidence_label(0.7), "🟢 70%");
        assert_eq!(confidence_label(0.69), "🟡 69%");
        assert_eq!(confidence_label(0.4), "🟡 40%");
        assert_eq!(confidence_label(0.39), "🔴 39%");
    }

    #[test]
    fn iso_matches_js_toisostring() {
        // 1784155063100 = 2026-07-15T22:37:43.100Z (the Divan Boots flip)
        assert_eq!(iso_ms(1784155063100), "2026-07-15T22:37:43.100Z");
        let found = iso_ms(1784155063100).replace('T', " ").replace('Z', " UTC");
        assert_eq!(found, "2026-07-15 22:37:43.100 UTC");
    }

    #[test]
    fn queue_drops_oldest_at_cap() {
        let d = Discord::new("http://x".into());
        for _ in 0..MAX_QUEUE + 5 {
            d.queue
                .lock()
                .unwrap()
                .push_back((dummy_flip(), None, dummy_ctx()));
        }
        assert_eq!(d.queue.lock().unwrap().len(), MAX_QUEUE + 5);
        // enqueue past the cap must drop from the front, not grow
        d.enqueue(&dummy_flip(), None, dummy_ctx());
        assert_eq!(d.queue.lock().unwrap().len(), MAX_QUEUE + 5);
    }

    fn dummy_ctx() -> FoundCtx {
        FoundCtx {
            exact_ms: 1_784_155_063_100,
            purchase_at_ms: None,
            api_visible_at_ms: None,
        }
    }

    /// The Found field is what the user reads to judge whether the finder is
    /// working, so pin BOTH shapes: an ordinary dump flip, and a bed (which must
    /// read PRE-API and be titled as a bed).
    #[test]
    fn found_field_reports_exact_time_and_pre_api_for_beds() {
        let f = dummy_flip();
        let t = 1_784_155_063_100i64;

        let dump = build_embed(
            &f,
            None,
            FoundCtx {
                exact_ms: t,
                purchase_at_ms: None,
                api_visible_at_ms: None,
            },
        );
        let found = dump["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == "Found")
            .unwrap()["value"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            found.contains("2026-07-15 22:37:43.100 UTC"),
            "exact emit time: {found}"
        );
        assert!(found.contains("after dump release"), "{found}");
        assert!(dump["title"].as_str().unwrap().starts_with("💰"));

        // Same flip caught mid-bed, buyable 15.0s from now.
        let bed = build_embed(
            &f,
            None,
            FoundCtx {
                exact_ms: t,
                purchase_at_ms: Some((t + 15_000) as f64),
                api_visible_at_ms: Some((t + 42_000) as f64),
            },
        );
        let found = bed["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == "Found")
            .unwrap()["value"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(found.contains("2026-07-15 22:37:43.100 UTC"), "{found}");
        assert!(
            found.contains("42000 ms PRE-API"),
            "lead is to API VISIBILITY, not the lift: {found}"
        );
        assert!(found.contains("buyable in 15.0s"), "{found}");
        assert!(
            bed["title"].as_str().unwrap().contains("BED"),
            "{}",
            bed["title"]
        );

        // Past its grace (buyable NOW) but the dump has not published it yet.
        // This is pre-API WITHOUT being a bed — the case that was invisible before.
        let pre = build_embed(
            &f,
            None,
            FoundCtx {
                exact_ms: t,
                purchase_at_ms: None,
                api_visible_at_ms: Some((t + 31_000) as f64),
            },
        );
        let found = pre["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == "Found")
            .unwrap()["value"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(found.contains("31000 ms PRE-API"), "{found}");
        assert!(found.contains("buyable NOW"), "{found}");
        assert!(
            pre["title"].as_str().unwrap().contains("PRE-API"),
            "{}",
            pre["title"]
        );
        assert!(
            !pre["title"].as_str().unwrap().contains("BED"),
            "{}",
            pre["title"]
        );
    }

    fn dummy_flip() -> Flip {
        Flip {
            uuid: "u".into(),
            item_name: "i".into(),
            finder: "median".into(),
            price: 1.0,
            reference: 2.0,
            profit: 1.0,
            roi_pct: 1.0,
            confidence: 0.5,
            samples: 1,
            key: "k".into(),
            guard: "none".into(),
            found_after_refresh_ms: 1.0,
            found_at_ms: 1.0,
            attrs: serde_json::from_value(serde_json::json!({"id":"X"})).unwrap(),
            median_stats: None,
        }
    }
}
