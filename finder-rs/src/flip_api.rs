//! Flip API on :15100 (port of index.ts startFlipApi, 187-272).
//!
//! Two live consumers, both confirmed in prod:
//!
//! * `GET /filter` — the BinMaster filter editor UI, which is how the filter
//!   actually gets tuned.
//! * `GET /recent-flips` — baf-backend reads this to cross-match flips against
//!   the Discord all-flips channel.
//!
//! `POST /relay-dump` (second-vantage dump ingest) is NOT ported: prod has
//! RELAY_TOKEN set but no relay has ever fed it (every observed dump is
//! src=local), and it would need a hook into detect.rs's first-wins gate. It
//! 404s here rather than silently 403ing, so a relay coming online is loud
//! instead of invisible. See STATUS.md s23.
//!
//! Built on hyper 1.x directly: it is already in the lock via reqwest, so this
//! adds no new framework to the musl build.

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

use crate::recent_flips::RecentFlips;
use crate::store::Store;

/// The editor page, byte-identical to the TS `FILTER_EDITOR_HTML`. Kept as a
/// separate asset rather than a Rust string literal so the two cannot drift:
/// `tools/sync-filter-editor.sh` regenerates it from filterEditor.ts.
const FILTER_EDITOR_HTML: &str = include_str!("../assets/filter_editor.html");

/// Body cap on PUT /filter.json (index.ts:212 destroys the request past this).
const MAX_FILTER_BODY: usize = 5_000_000;

pub struct FlipApiState {
    pub recent: Arc<Mutex<RecentFlips>>,
    pub filter_path: String,
    pub edit_token: String,
    /// HTTP Basic Auth password for the filter-editor routes (`/filter`,
    /// `/filter.json`). Set from `ADMIN_PASSWORD` to the SAME value as the
    /// baf-backend admin dashboard, so one password guards both. Empty = the
    /// editor is unauthenticated (a loud boot warning fires), matching how
    /// baf-backend disables its own dashboard when `ADMIN_PASSWORD` is unset.
    /// The machine endpoints (`/recent-flips`, `/cofl-purchase`) are intentionally
    /// NOT gated here — baf-backend calls them without credentials; lock those at
    /// the firewall instead.
    pub admin_password: String,
    /// For `POST /cofl-purchase`: looks up whether pageflipper's crawler ever
    /// saw the (item, price) COFL just bought, and when.
    pub store: Arc<Mutex<Store>>,
    /// Discord webhook for the COFL-vs-pageflipper comparison line. Empty = off.
    pub cofl_compare_webhook: String,
}

fn json_res(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

/// Constant-time byte comparison so the password check leaks neither length nor
/// content through timing. Length inequality still returns fast, which only
/// reveals length, not the bytes; the loop over equal-length inputs is the part
/// that must not short-circuit.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Password from an `Authorization: Basic base64(user:pass)` header, or None if
/// absent/malformed. The username is ignored (baf-backend checks the password
/// alone), so any user string works; the password is everything after the FIRST
/// colon, since a colon is legal inside a password but not a username (RFC 7617).
fn basic_auth_password(req: &Request<Incoming>) -> Option<String> {
    let raw = req
        .headers()
        .get(hyper::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    basic_auth_password_from_header(raw)
}

/// Pure header parser behind `basic_auth_password`, split out so it is testable
/// without constructing a hyper `Request<Incoming>`.
fn basic_auth_password_from_header(auth: &str) -> Option<String> {
    let b64 = auth
        .strip_prefix("Basic ")
        .or_else(|| auth.strip_prefix("basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    let s = String::from_utf8(decoded).ok()?;
    Some(s.split_once(':')?.1.to_string())
}

/// 401 with a `WWW-Authenticate: Basic` challenge, so a browser hitting `/filter`
/// shows a native login prompt.
fn auth_challenge() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(
            "WWW-Authenticate",
            "Basic realm=\"BAF Finder\", charset=\"UTF-8\"",
        )
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            r#"{"ok":false,"error":"authentication required"}"#,
        )))
        .unwrap()
}

/// Port of filter.ts saveFilterText: validate, persist pretty-printed, apply now.
/// Returns Some(error) exactly where TS returns an error string.
fn save_filter_text(path: &str, text: &str) -> Option<String> {
    let parsed: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return Some(format!("invalid JSON: {e}")),
    };
    if !parsed
        .get("item_specific_filters")
        .is_some_and(|v| v.is_object())
    {
        return Some("missing \"item_specific_filters\" object".to_string());
    }
    if let Some(dir) = std::path::Path::new(path).parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return Some(format!("write failed: {e}"));
        }
    }
    // TS writes JSON.stringify(parsed, null, 2): reserialize the PARSED value,
    // not the raw text, so what lands on disk is normalized the same way.
    let pretty = match serde_json::to_string_pretty(&parsed) {
        Ok(s) => s,
        Err(e) => return Some(format!("write failed: {e}")),
    };
    if let Err(e) = std::fs::write(path, pretty) {
        return Some(format!("write failed: {e}"));
    }
    None
}

async fn handle(
    req: Request<Incoming>,
    st: Arc<FlipApiState>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();
    let token = form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.to_string())
        .unwrap_or_default();

    // ---- Auth gate: the human filter-editor routes require the admin password
    //      (HTTP Basic, same password as the baf-backend admin dashboard). The
    //      machine endpoints below (/recent-flips, /cofl-purchase) are NOT gated:
    //      baf-backend calls them without credentials, so they are firewall-scoped
    //      instead. Empty admin_password = disabled (boot warning), so a
    //      mis-set env fails OPEN to today's behaviour rather than locking out. ----
    let needs_auth = path == "/filter" || path == "/filter/" || path == "/filter.json";
    if needs_auth && !st.admin_password.is_empty() {
        let ok = basic_auth_password(&req)
            .map(|p| ct_eq(p.as_bytes(), st.admin_password.as_bytes()))
            .unwrap_or(false);
        if !ok {
            return Ok(auth_challenge());
        }
    }

    // GET /recent-flips (TS uses startsWith, so query strings pass through).
    if method == Method::GET && path.starts_with("/recent-flips") {
        let body = {
            let r = st.recent.lock().unwrap();
            serde_json::to_string(&r.to_json()).unwrap_or_else(|_| "[]".into())
        };
        return Ok(json_res(StatusCode::OK, body));
    }

    if method == Method::GET && (path == "/filter" || path == "/filter/") {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/html; charset=utf-8")
            .body(Full::new(Bytes::from(FILTER_EDITOR_HTML)))
            .unwrap());
    }

    if method == Method::GET && path == "/filter.json" {
        // TS: getFilterText() || <default>. Unreadable file => the same default.
        let text = std::fs::read_to_string(&st.filter_path).unwrap_or_default();
        let body = if text.is_empty() {
            "{\n  \"item_specific_filters\": { \"GLOBAL\": [] }\n}".to_string()
        } else {
            text
        };
        return Ok(json_res(StatusCode::OK, body));
    }

    if method == Method::PUT && path == "/filter.json" {
        if !st.edit_token.is_empty() && token != st.edit_token {
            return Ok(json_res(
                StatusCode::UNAUTHORIZED,
                serde_json::json!({ "ok": false, "error": "bad or missing token" }).to_string(),
            ));
        }
        let collected = match req.into_body().collect().await {
            Ok(c) => c.to_bytes(),
            Err(e) => {
                return Ok(json_res(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({ "ok": false, "error": e.to_string() }).to_string(),
                ))
            }
        };
        if collected.len() > MAX_FILTER_BODY {
            return Ok(json_res(
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "ok": false, "error": "body too large" }).to_string(),
            ));
        }
        let text = String::from_utf8_lossy(&collected).to_string();
        return Ok(match save_filter_text(&st.filter_path, &text) {
            Some(err) => json_res(
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "ok": false, "error": err }).to_string(),
            ),
            None => {
                // No explicit "apply now" hook is needed: serve_loop's BinFilterStore
                // polls mtime (nanosecond SystemTime) every 50ms, so the write is
                // live well before the user can look. TS calls loadFilter() inline
                // only because its own watcher is on a 5s timer.
                //
                // TS reports filterActive() post-load; here the file has already
                // validated as having an item_specific_filters object, which is the
                // exact condition the editor's own refreshActive() re-checks against
                // /filter.json a moment later, so this agrees with what it displays.
                json_res(
                    StatusCode::OK,
                    serde_json::json!({ "ok": true, "active": true }).to_string(),
                )
            }
        });
    }

    // POST /cofl-purchase — baf-backend calls this whenever the COFL-benchmark
    // account (baf_deer_raph, running Coflnet's own finder, not ours) actually
    // buys something. We can't match on auction uuid (pageflipper's browse-crawl
    // has no uuid for a listing before it hits the public dump), so the join key
    // is (item name, exact BIN price) — good enough since a collision would need
    // the same item at the exact same coin price within the same short window.
    if method == Method::POST && path == "/cofl-purchase" {
        let collected = match req.into_body().collect().await {
            Ok(c) => c.to_bytes(),
            Err(e) => {
                return Ok(json_res(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({"ok": false, "error": e.to_string()}).to_string(),
                ))
            }
        };
        if collected.len() > 4096 {
            return Ok(json_res(
                StatusCode::BAD_REQUEST,
                serde_json::json!({"ok": false, "error": "body too large"}).to_string(),
            ));
        }
        let payload: CoflPurchase = match serde_json::from_slice(&collected) {
            Ok(p) => p,
            Err(e) => {
                return Ok(json_res(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({"ok": false, "error": format!("bad body: {e}")}).to_string(),
                ))
            }
        };
        let sighting = st
            .store
            .lock()
            .unwrap()
            .lookup_pf_sighting(&payload.item_name, payload.price)
            .ok()
            .flatten();
        let webhook = st.cofl_compare_webhook.clone();
        let buyer = payload.buyer.clone();
        tokio::spawn(async move {
            post_cofl_compare(&webhook, &payload, sighting, &buyer).await;
        });
        return Ok(json_res(
            StatusCode::OK,
            serde_json::json!({"ok": true}).to_string(),
        ));
    }

    Ok(Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Full::new(Bytes::new()))
        .unwrap())
}

#[derive(serde::Deserialize)]
struct CoflPurchase {
    item_name: String,
    price: i64,
    /// When Coflnet's OWN finder told the bot about this flip (its "found" time).
    received_at_ms: i64,
    purchased_at_ms: i64,
    buyer: String,
}

async fn post_cofl_compare(
    webhook: &str,
    p: &CoflPurchase,
    sighting: Option<(i64, String)>,
    buyer: &str,
) {
    if webhook.is_empty() {
        return;
    }
    let content = match sighting {
        Some((crawl_ts, seller)) => {
            let delta = p.received_at_ms - crawl_ts;
            let (verb, ms) = if delta >= 0 {
                ("before", delta)
            } else {
                ("after", -delta)
            };
            let seller_note = if seller.is_empty() {
                String::new()
            } else {
                format!(" (seller `{seller}`)")
            };
            let to_purchase = p.purchased_at_ms - crawl_ts;
            format!(
                "🏁 **{}** — COFL ({buyer}) bought for {} coins.\npageflipper saw this listing **{ms} ms {verb}** COFL's own find time{seller_note} — {to_purchase} ms before the actual purchase.",
                p.item_name, p.price
            )
        }
        None => format!(
            "❌ **{}** — COFL ({buyer}) bought for {} coins.\npageflipper never saw this listing.",
            p.item_name, p.price
        ),
    };
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .build()
            .expect("reqwest client")
    });
    let body = serde_json::json!({ "username": "BAF vs COFL", "content": content });
    for _ in 0..3 {
        match client.post(webhook).json(&body).send().await {
            Ok(res) if res.status().as_u16() == 429 => {
                let wait = res
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|j| j.get("retry_after").and_then(|v| v.as_f64()))
                    .map(|s| ((s * 1000.0) as u64).clamp(500, 10_000))
                    .unwrap_or(2000);
                tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                continue;
            }
            Ok(res) => {
                if !res.status().is_success() {
                    eprintln!("cofl-compare webhook rejected: {}", res.status());
                }
                return;
            }
            Err(e) => {
                eprintln!("cofl-compare webhook failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            }
        }
    }
}

/// Spawns the flip API. Mirrors index.ts:271 (FLIP_API_HOST default 0.0.0.0,
/// FLIP_API_PORT default 15100).
pub async fn serve(st: Arc<FlipApiState>) -> std::io::Result<()> {
    let port: u16 = std::env::var("FLIP_API_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15100);
    let host = std::env::var("FLIP_API_HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let listener = TcpListener::bind((host.as_str(), port)).await?;
    tracing_port(port);
    loop {
        let (stream, _) = listener.accept().await?;
        let st = st.clone();
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let _ = http1::Builder::new()
                .serve_connection(io, service_fn(move |req| handle(req, st.clone())))
                .await;
        });
    }
}

fn tracing_port(port: u16) {
    // pino-shaped, matching index.ts's 'flip API listening'.
    println!(
        r#"{{"level":30,"time":"{}","port":{},"msg":"flip API listening"}}"#,
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        port
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> String {
        let p = std::env::temp_dir().join(format!("flipapi-{name}-{}.json", std::process::id()));
        p.to_string_lossy().to_string()
    }

    #[test]
    fn rejects_invalid_json_like_ts() {
        let p = tmp("bad");
        let err = save_filter_text(&p, "{not json").unwrap();
        assert!(err.starts_with("invalid JSON:"), "got {err}");
        assert!(
            !std::path::Path::new(&p).exists(),
            "must not write on parse failure"
        );
    }

    #[test]
    fn rejects_missing_item_specific_filters() {
        let p = tmp("missing");
        let err = save_filter_text(&p, r#"{"other": 1}"#).unwrap();
        assert_eq!(err, "missing \"item_specific_filters\" object");
        assert!(!std::path::Path::new(&p).exists());
    }

    #[test]
    fn non_object_item_specific_filters_is_rejected() {
        // TS checks typeof === 'object'; an array/number must not pass.
        let p = tmp("nonobj");
        assert!(save_filter_text(&p, r#"{"item_specific_filters": 5}"#).is_some());
    }

    #[test]
    fn writes_pretty_and_roundtrips() {
        let p = tmp("ok");
        let _ = std::fs::remove_file(&p);
        assert!(save_filter_text(&p, r#"{"item_specific_filters":{"GLOBAL":[]}}"#).is_none());
        let on_disk = std::fs::read_to_string(&p).unwrap();
        assert!(
            on_disk.contains("\n  "),
            "must be 2-space pretty like TS, got: {on_disk}"
        );
        let back: serde_json::Value = serde_json::from_str(&on_disk).unwrap();
        assert!(back["item_specific_filters"]["GLOBAL"].is_array());
        let _ = std::fs::remove_file(&p);
    }

    /// Encode `user:pass` the way a browser builds an `Authorization: Basic` header.
    fn basic(user: &str, pass: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
        )
    }

    #[test]
    fn basic_auth_extracts_password_any_username() {
        // The username is ignored; the password is what matters, and a colon
        // inside the password must survive (split on the FIRST colon only).
        assert_eq!(
            basic_auth_password_from_header(&basic("admin", "s3cret")).as_deref(),
            Some("s3cret")
        );
        assert_eq!(
            basic_auth_password_from_header(&basic("", "s3cret")).as_deref(),
            Some("s3cret")
        );
        assert_eq!(
            basic_auth_password_from_header(&basic("x", "a:b:c")).as_deref(),
            Some("a:b:c")
        );
    }

    #[test]
    fn basic_auth_rejects_malformed() {
        assert_eq!(basic_auth_password_from_header("Bearer abc"), None); // wrong scheme
        assert_eq!(basic_auth_password_from_header("Basic !!!notbase64"), None);
        // Valid base64 but no colon => not a user:pass pair.
        let nocolon = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("nopassword")
        );
        assert_eq!(basic_auth_password_from_header(&nocolon), None);
    }

    #[test]
    fn ct_eq_matches_only_exact() {
        assert!(ct_eq(b"hunter2", b"hunter2"));
        assert!(!ct_eq(b"hunter2", b"hunter3"));
        assert!(!ct_eq(b"hunter2", b"hunter2x")); // length mismatch
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b"")); // an empty configured password would match empty, hence the caller guards on non-empty
    }

    /// The exact predicate the /filter guard applies: Basic header's password vs
    /// the configured admin password, constant-time.
    #[test]
    fn guard_predicate_accepts_right_password_only() {
        let admin = "baf-admin-pw";
        let check = |hdr: &str| {
            basic_auth_password_from_header(hdr)
                .map(|p| ct_eq(p.as_bytes(), admin.as_bytes()))
                .unwrap_or(false)
        };
        assert!(check(&basic("whoever", admin)), "correct password passes");
        assert!(!check(&basic("whoever", "wrong")), "wrong password fails");
        assert!(!check("Basic "), "empty/garbage fails");
        assert!(!check("Bearer baf-admin-pw"), "bearer scheme fails");
    }
}
