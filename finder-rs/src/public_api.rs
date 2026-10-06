//! Web UI + JSON API for public-feed consumers to manage their own filter.
//!
//! Exists because the baf mod never sends `{"type":"filter"}` — it speaks only
//! `inventory` and `listed` — so a filter that lives on the WebSocket connection
//! is unreachable for a mod user. Filters are therefore saved server-side against
//! the key, applied at handshake time, and pushed live to that key's open
//! connections when saved here.
//!
//! Routes (loopback only; Caddy fronts them):
//!   * `GET  /app`         the UI, a single self-contained page
//!   * `GET  /api/filter`  the caller's saved filter + what their key allows
//!   * `PUT  /api/filter`  validate and save
//!   * `POST /api/reset`   restore the default filter
//!
//! Auth is the SAME key as the feed, presented as `Authorization: Bearer`, and it
//! is verified through the identical constant-time [`KeyStore`] path. The key is
//! never in a URL here, so it cannot land in a proxy log; the page keeps it in
//! `localStorage` and sends it as a header.

use crate::public_ws::{parse_binmaster, ClientFilter, PublicHub, SavedConfig};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::net::TcpListener;

/// The UI. Kept as a separate asset rather than a Rust string literal so the
/// markup can be edited without touching the server.
const APP_HTML: &str = include_str!("../assets/public_filter_app.html");

/// A filter document is a handful of numbers and short lists. Anything larger is
/// not a filter.
const MAX_BODY: usize = 512 * 1024;

fn json_res(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn unauthorized() -> Response<Full<Bytes>> {
    json_res(
        StatusCode::UNAUTHORIZED,
        r#"{"ok":false,"error":"invalid key"}"#.to_string(),
    )
}

/// The default filter offered to a new consumer.
///
/// Deliberately BELOW every one of the owner's thresholds. Everything on this
/// feed has already failed his filter, so a copy of his numbers would return
/// almost nothing; the useful starting point is looser than his, not equal to it.
/// `manipulated` stays blocked because that guard marks suspect price history,
/// which is a bad flip for anyone.
pub fn default_filter() -> ClientFilter {
    ClientFilter {
        min_profit: 300_000.0,
        min_roi_pct: 5.0,
        min_confidence: 0.5,
        min_volume_per_day: 1.0,
        blocked_guards: vec!["manipulated".to_string()],
        ..Default::default()
    }
}

/// Where the owner's live tier filter sits. Read at request time, not cached, so
/// "pull" always returns what the finder is running right now.
fn owner_filter_path() -> String {
    if let Ok(p) = std::env::var("BINMASTER_FILTER_PATH") {
        return p;
    }
    std::env::var("WS_CONFIG_PATH")
        .ok()
        .and_then(|p| {
            std::path::Path::new(&p).parent().map(|d| {
                d.join("binmaster-filter.json")
                    .to_string_lossy()
                    .into_owned()
            })
        })
        .unwrap_or_else(|| "./data/binmaster-filter.json".to_string())
}

/// Drop the owner's floors into the range this feed actually populates.
///
/// Handing his filter over verbatim is close to useless: every flip here already
/// failed it, so it would match almost nothing but the `flood` bucket. The shape
/// is what is worth copying — his tier ladder and per-item rules — so keep all of
/// that and move only the numbers: profit /10, percentages /2, confidence -0.15,
/// each with a floor so the result stays sane. Volume bands are untouched; they
/// describe the item, not his appetite.
fn loosen(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            let mut out = serde_json::Map::new();
            for (k, val) in o {
                let nv = match (k.as_str(), val.as_f64()) {
                    ("min_profit" | "global_min_profit", Some(n)) => {
                        json!(300_000f64.max((n / 10.0).round()))
                    }
                    ("min_profit_percent" | "global_min_profit_percent", Some(n)) => {
                        json!(5f64.max((n / 2.0).round()))
                    }
                    ("min_confidence" | "global_min_confidence", Some(n)) => {
                        json!(((0.5f64.max(n - 0.15)) * 100.0).round() / 100.0)
                    }
                    _ => loosen(val),
                };
                out.insert(k.clone(), nv);
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(loosen).collect()),
        other => other.clone(),
    }
}

/// Reject nonsense before it is persisted. These are the bounds that make a
/// filter meaningful rather than a footgun: a negative floor, a max below the
/// min, or a confidence outside 0..1 can only ever be a mistake.
fn validate(f: &ClientFilter) -> Result<(), String> {
    for (name, v) in [
        ("minProfit", f.min_profit),
        ("minRoiPct", f.min_roi_pct),
        ("minVolumePerDay", f.min_volume_per_day),
        ("minPrice", f.min_price),
        ("maxPrice", f.max_price),
    ] {
        if !v.is_finite() || v < 0.0 {
            return Err(format!("{name} must be a number and not negative"));
        }
    }
    if !f.min_confidence.is_finite() || !(0.0..=1.0).contains(&f.min_confidence) {
        return Err("minConfidence must be between 0 and 1".into());
    }
    if f.max_price > 0.0 && f.max_price < f.min_price {
        return Err("maxPrice is below minPrice, so nothing could ever match".into());
    }
    for (name, list) in [
        ("blacklistIds", &f.blacklist_ids),
        ("allowIds", &f.allow_ids),
        ("blockedGuards", &f.blocked_guards),
        ("finders", &f.finders),
        ("buckets", &f.buckets),
    ] {
        if list.len() > 500 {
            return Err(format!("{name} has too many entries (max 500)"));
        }
    }
    Ok(())
}

/// The bearer key, or None. Only a header is accepted: a key in a query string
/// would be written to every proxy access log it passes through.
fn bearer(req: &Request<Incoming>) -> Option<String> {
    let raw = req
        .headers()
        .get(hyper::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let b = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))?;
    Some(b.trim().to_string())
}

async fn handle(
    req: Request<Incoming>,
    hub: Arc<PublicHub>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    // HEAD as well as GET: link checkers and uptime monitors use HEAD, and a 404
    // there reads as "the UI is down" when it is fine. hyper drops the body for a
    // HEAD response itself, so the same branch serves both.
    if (method == Method::GET || method == Method::HEAD) && (path == "/app" || path == "/app/") {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/html; charset=utf-8")
            // The page holds a key in localStorage; keep it out of shared caches.
            .header("cache-control", "no-store")
            .header("referrer-policy", "no-referrer")
            .header("x-content-type-options", "nosniff")
            .body(Full::new(Bytes::from(APP_HTML)))
            .unwrap());
    }

    if !path.starts_with("/api/") {
        return Ok(json_res(
            StatusCode::NOT_FOUND,
            r#"{"ok":false,"error":"not found"}"#.to_string(),
        ));
    }

    // Every /api route is authenticated by the same key as the feed.
    let Some(key) = bearer(&req).and_then(|k| hub.keys.lookup(&k)) else {
        return Ok(unauthorized());
    };

    match (method, path.as_str()) {
        (Method::GET, "/api/filter") => {
            let saved = hub.filters.get(&key.label);
            let binmaster = saved.as_ref().and_then(|c| c.binmaster.clone());
            Ok(json_res(
                StatusCode::OK,
                json!({
                    "ok": true,
                    "label": key.label,
                    "saved": saved.is_some(),
                    "binmaster": binmaster,
                    "filter": saved.map(|c| c.filter).unwrap_or_else(default_filter),
                    "default": default_filter(),
                    "allowPricing": key.allow_pricing,
                    "buckets": key.buckets_or(&hub.buckets),
                    "minProfitFloor": key.min_profit_floor(hub.min_profit),
                })
                .to_string(),
            ))
        }
        // The owner's live filter, as a starting point. Both forms are returned so
        // the page can offer "exactly his" and "his, usable here".
        (Method::GET, "/api/owner-filter") => {
            let path = owner_filter_path();
            let doc: Value = match std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
            {
                Some(d) => d,
                None => {
                    return Ok(json_res(
                        StatusCode::NOT_FOUND,
                        r#"{"ok":false,"error":"there is no tier filter to pull right now"}"#
                            .into(),
                    ))
                }
            };
            let rules = doc
                .get("item_specific_filters")
                .and_then(|v| v.as_object())
                .map(|o| {
                    o.values()
                        .filter_map(|v| v.as_array())
                        .map(|a| a.len())
                        .sum::<usize>()
                })
                .unwrap_or(0);
            Ok(json_res(
                StatusCode::OK,
                json!({"ok":true,"rules":rules,"exact":doc,"loosened":loosen(&doc)}).to_string(),
            ))
        }
        (Method::PUT, "/api/filter") | (Method::POST, "/api/filter") => {
            let body = match req.into_body().collect().await {
                Ok(b) => b.to_bytes(),
                Err(_) => {
                    return Ok(json_res(
                        StatusCode::BAD_REQUEST,
                        r#"{"ok":false,"error":"could not read body"}"#.into(),
                    ))
                }
            };
            if body.len() > MAX_BODY {
                return Ok(json_res(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    r#"{"ok":false,"error":"filter too large"}"#.into(),
                ));
            }
            let parsed: ClientFilter = match serde_json::from_slice(&body) {
                Ok(f) => f,
                Err(e) => {
                    return Ok(json_res(
                        StatusCode::BAD_REQUEST,
                        json!({"ok":false,"error":format!("could not read that filter: {e}")})
                            .to_string(),
                    ))
                }
            };
            if let Err(e) = validate(&parsed) {
                return Ok(json_res(
                    StatusCode::BAD_REQUEST,
                    json!({"ok":false,"error":e}).to_string(),
                ));
            }
            if let Err(e) = hub.filters.set_filter(&key.label, parsed.clone()) {
                eprintln!("public api: could not save filter for '{}': {e}", key.label);
                return Ok(json_res(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    r#"{"ok":false,"error":"could not save"}"#.into(),
                ));
            }
            let cfg = hub.filters.get(&key.label).unwrap_or_default();
            let live = hub.apply_saved_config(&key.label, &cfg);
            eprintln!(
                "public api: '{}' saved thresholds ({live} live connection(s) updated)",
                key.label
            );
            Ok(json_res(
                StatusCode::OK,
                json!({"ok":true,"appliedTo":live}).to_string(),
            ))
        }
        // Upload / replace / clear the tier filter. The body is the BinMaster
        // document itself, or `null` to go back to thresholds only.
        (Method::PUT, "/api/binmaster") | (Method::POST, "/api/binmaster") => {
            let body = match req.into_body().collect().await {
                Ok(b) => b.to_bytes(),
                Err(_) => {
                    return Ok(json_res(
                        StatusCode::BAD_REQUEST,
                        r#"{"ok":false,"error":"could not read body"}"#.into(),
                    ))
                }
            };
            if body.len() > MAX_BODY {
                return Ok(json_res(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    json!({"ok":false,"error":format!("filter is larger than {}KB", MAX_BODY / 1024)}).to_string(),
                ));
            }
            let doc: Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(json_res(
                        StatusCode::BAD_REQUEST,
                        json!({"ok":false,"error":format!("that is not valid JSON: {e}")})
                            .to_string(),
                    ))
                }
            };
            let doc = if doc.is_null() { None } else { Some(doc) };
            if let Some(d) = &doc {
                // Mirror the finder's own filter-editor check so the message names
                // the actual problem instead of a serde path.
                if !d
                    .get("item_specific_filters")
                    .is_some_and(|v| v.is_object())
                {
                    return Ok(json_res(
                        StatusCode::BAD_REQUEST,
                        r#"{"ok":false,"error":"a tier filter needs an \"item_specific_filters\" object"}"#.into(),
                    ));
                }
                if parse_binmaster(d).is_none() {
                    return Ok(json_res(
                        StatusCode::BAD_REQUEST,
                        r#"{"ok":false,"error":"could not read that as a tier filter — check filter_type and matcher on each rule"}"#.into(),
                    ));
                }
            }
            let tiers = doc
                .as_ref()
                .and_then(|d| d.get("item_specific_filters"))
                .and_then(|v| v.as_object())
                .map(|o| {
                    o.values()
                        .filter_map(|v| v.as_array())
                        .map(|a| a.len())
                        .sum::<usize>()
                })
                .unwrap_or(0);
            if let Err(e) = hub.filters.set_binmaster(&key.label, doc.clone()) {
                return Ok(json_res(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"ok":false,"error":e}).to_string(),
                ));
            }
            let cfg = hub.filters.get(&key.label).unwrap_or_default();
            let live = hub.apply_saved_config(&key.label, &cfg);
            eprintln!(
                "public api: '{}' {} a tier filter ({tiers} rule(s), {live} live connection(s))",
                key.label,
                if doc.is_some() { "uploaded" } else { "cleared" }
            );
            Ok(json_res(
                StatusCode::OK,
                json!({"ok":true,"active":doc.is_some(),"rules":tiers,"appliedTo":live})
                    .to_string(),
            ))
        }
        (Method::POST, "/api/reset") => {
            let cfg = SavedConfig {
                filter: default_filter(),
                binmaster: None,
            };
            if let Err(e) = hub.filters.set(&key.label, cfg.clone()) {
                return Ok(json_res(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"ok":false,"error":e}).to_string(),
                ));
            }
            let live = hub.apply_saved_config(&key.label, &cfg);
            Ok(json_res(
                StatusCode::OK,
                json!({"ok":true,"appliedTo":live,"filter":cfg.filter}).to_string(),
            ))
        }
        _ => Ok(json_res(
            StatusCode::NOT_FOUND,
            r#"{"ok":false,"error":"not found"}"#.to_string(),
        )),
    }
}

/// Serve the UI + API. Loopback by default: Caddy is the only thing that should
/// ever reach it, exactly like the feed itself.
pub async fn serve(hub: Arc<PublicHub>) -> std::io::Result<()> {
    let port: u16 = std::env::var("PUBLIC_API_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15103);
    let host = std::env::var("PUBLIC_API_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let listener = TcpListener::bind((host.as_str(), port)).await?;
    eprintln!("public filter UI: listening {host}:{port} (/app)");
    loop {
        let (stream, _) = listener.accept().await?;
        let hub = hub.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let _ = http1::Builder::new()
                .serve_connection(io, service_fn(move |req| handle(req, hub.clone())))
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_sits_below_every_owner_threshold() {
        // The whole point: this feed carries what the owner's filter rejected, so
        // a default at or above his numbers would return an empty feed. These are
        // his live values as of 2026-08-10.
        let d = default_filter();
        assert!(d.min_profit < 3_000_000.0, "must be under hardMinProfit");
        assert!(d.min_roi_pct < 10.0);
        assert!(d.min_confidence < 0.70);
        assert!(d.min_volume_per_day < 6.0);
        // ...but still refuse suspect price history, which is bad for anyone.
        assert!(d.blocked_guards.iter().any(|g| g == "manipulated"));
    }

    #[test]
    fn validation_rejects_the_footguns() {
        let bad = |f: ClientFilter| assert!(validate(&f).is_err());
        bad(ClientFilter {
            min_profit: -1.0,
            ..Default::default()
        });
        bad(ClientFilter {
            min_confidence: 1.5,
            ..Default::default()
        });
        bad(ClientFilter {
            min_confidence: f64::NAN,
            ..Default::default()
        });
        // A ceiling under the floor can never match anything.
        bad(ClientFilter {
            min_price: 10_000_000.0,
            max_price: 1_000_000.0,
            ..Default::default()
        });
        bad(ClientFilter {
            blacklist_ids: vec!["X".into(); 501],
            ..Default::default()
        });
        // maxPrice 0 means "no ceiling", so it must NOT trip the min/max check.
        assert!(validate(&ClientFilter {
            min_price: 10_000_000.0,
            max_price: 0.0,
            ..Default::default()
        })
        .is_ok());
        assert!(validate(&default_filter()).is_ok());
    }

    #[test]
    fn a_key_is_never_accepted_from_the_query_string() {
        // A key in a URL lands in proxy logs. Only the header is honoured, and
        // `bearer` is the only way a key enters this module. Check the SERVER
        // half only: this assertion's own text would otherwise match itself.
        let src = include_str!("public_api.rs");
        let server = src.split("#[cfg(test)]").next().unwrap();
        // The call, not the word: "query" appears in the prose above `bearer`.
        assert!(
            !server.contains(".query("),
            "no route may read a key from the query string"
        );
        assert!(
            server.contains("hyper::header::AUTHORIZATION"),
            "the key must come from the auth header"
        );
    }
}
