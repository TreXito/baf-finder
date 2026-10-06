//! Port of `baf-flip-finder/src/store.ts` — the sqlite persistence layer.
//!
//! Behavioral clone: same schema (sold/posted/listing_uuids/listing/censored),
//! same INSERT-OR-IGNORE semantics, same TTS wiring, same flood-brake held-count
//! query, same reconcile logic. A single owned `Connection` mirrors the TS
//! module-level `db`. Operational timestamps use wall-clock (as TS `Date.now()`);
//! `load_references` still takes an explicit `now_ms` so the COMPARE path can pin
//! the clock identically to the money core.

use finder_core::config::{REF_MAX_AGE_DAYS, RETENTION_DAYS, UNLISTED_LOOKBACK_S};
use finder_core::nbt::ItemAttributes;
use finder_core::price_index::Reference;
use rusqlite::{params, Connection, OptionalExtension};
use rustc_hash::FxHashMap;
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

// Operational config (config.ts defaults; env-overridable by the binary).
// RETENTION_DAYS lives in finder_core::config: prod sets 36500 (~never prune)
// and a baked-in 14 would delete the reference history on the first prune.
const TTS_CENSOR_DAYS: i64 = 4;
const TTS_MAX_DAYS: i64 = 14;

fn now_s() -> i64 {
    (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        / 1000) as i64
}

/// One genuinely-new sold auction to persist.
pub struct SoldRow {
    pub auction_id: String,
    pub price: f64,
    pub bin: bool,
    pub sold_at: i64,
    pub seller: String,
    /// WHO bought it, from the ended feed. Lets a purchase be attributed to us,
    /// to a COFL user, or to a rival custom finder -- and combined with a NULL
    /// `tts_ms` (we never saw the listing) it identifies who is buying inside
    /// the 20s BIN grace window, which only a non-dump feed can do.
    pub buyer: String,
    pub attrs: ItemAttributes,
}

/// A NEW BIN listing observed during a sweep (B1 TTS capture).
pub struct ListingRow {
    pub auction_id: String,
    pub start: i64,
    pub item_id: String,
}

/// A flip we pushed, recorded for later resale validation.
#[derive(Default)]
pub struct PostedRow {
    pub uuid: String,
    pub item_name: String,
    pub finder: String,
    pub buy: f64,
    pub reference: f64,
    pub item_uuid: Option<String>,
    pub base_key: Option<String>,
    pub delivered: bool,
    pub est_roi: Option<f64>,
    pub conf: Option<f64>,
    pub samples: Option<i64>,
    pub volatility: Option<f64>,
}

/// A freshly-ended auction to reconcile against posted flips.
pub struct EndedRow {
    pub uuid: String,
    pub price: f64,
    pub sold_at: i64,
    pub item_uuid: Option<String>,
}

/// A confirmed resale of a posted flip (the market's verdict on the estimate).
#[derive(Clone)]
pub struct ResolvedFlip {
    pub uuid: String,
    pub item_name: String,
    pub finder: String,
    pub buy: f64,
    pub reference: f64,
    pub sold_price: f64,
    pub via: &'static str, // "listing" | "item_uuid"
}

pub struct Store {
    conn: Connection,
    tts_capture: bool,
}

impl Store {
    /// Open (creating dirs), set WAL, create the schema + run migrations.
    pub fn open(db_path: &str, tts_capture: bool) -> rusqlite::Result<Self> {
        if let Some(parent) = std::path::Path::new(db_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // WAL defaults to synchronous=FULL, which fsyncs on EVERY commit. Since
        // these writes are autocommit, that is one fsync per row -- and
        // `record_posted` runs inside the eval loop, once per emitted flip. That
        // showed up as the single biggest slice of our own compute: `emit` was
        // 12-32ms of a 19-42ms `eval`, against 5-14ms for the actual pricing.
        // Because emit is synchronous, a flip found early in a sweep delayed the
        // evaluation of every auction after it.
        //
        // WAL + NORMAL is SQLite's own recommended pairing: it is still safe
        // against OS crashes and corruption, and only risks losing the most
        // recent commits on a hard power loss. Nothing money-critical depends on
        // that durability -- `posted` is ledger bookkeeping, `listing`/`sold` are
        // reference data that refills, and the cost basis lives in its own JSON
        // file flushed separately.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let s = Store { conn, tts_capture };
        s.init_schema()?;
        Ok(s)
    }

    fn init_schema(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sold (
                auction_id  TEXT PRIMARY KEY,
                item_id     TEXT NOT NULL,
                price       INTEGER NOT NULL,
                bin         INTEGER NOT NULL,
                sold_at     INTEGER NOT NULL,
                attrs       TEXT NOT NULL,
                seller      TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS idx_sold_time ON sold (sold_at);
            CREATE INDEX IF NOT EXISTS idx_sold_item ON sold (item_id);
            CREATE TABLE IF NOT EXISTS posted (
                uuid         TEXT PRIMARY KEY,
                item_name    TEXT,
                finder       TEXT,
                buy          INTEGER,
                reference    INTEGER,
                posted_at    INTEGER NOT NULL,
                sold_price   INTEGER,
                sold_at      INTEGER,
                item_uuid    TEXT,
                bought_at    INTEGER
            );
            CREATE TABLE IF NOT EXISTS listing_uuids (
                listing_uuid TEXT PRIMARY KEY,
                flip_uuid   TEXT NOT NULL,
                item_name   TEXT,
                listed_at   INTEGER NOT NULL,
                FOREIGN KEY (flip_uuid) REFERENCES posted(uuid)
            );
            CREATE INDEX IF NOT EXISTS idx_listing_uuids_flip ON listing_uuids (flip_uuid);
            CREATE TABLE IF NOT EXISTS listing (
                auction_id TEXT PRIMARY KEY,
                start      INTEGER NOT NULL,
                first_seen INTEGER NOT NULL,
                item_id    TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_listing_seen ON listing (first_seen);
            CREATE TABLE IF NOT EXISTS censored (
                auction_id  TEXT PRIMARY KEY,
                item_id     TEXT NOT NULL,
                start       INTEGER NOT NULL,
                censored_at INTEGER NOT NULL,
                lifetime_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_censored_item ON censored (item_id);
            CREATE TABLE IF NOT EXISTS pf_sightings (
                item_name TEXT NOT NULL,
                price     INTEGER NOT NULL,
                seller    TEXT NOT NULL DEFAULT '',
                crawl_ts  INTEGER NOT NULL,
                page      INTEGER,
                PRIMARY KEY (item_name, price)
            );
            CREATE INDEX IF NOT EXISTS idx_pf_sightings_ts ON pf_sightings (crawl_ts);
            "#,
        )?;
        // Migrations (idempotent — mirror ensureColumn).
        self.ensure_column("sold", "tts_ms", "tts_ms INTEGER");
        self.ensure_column("sold", "seller", "seller TEXT NOT NULL DEFAULT ''");
        self.ensure_column("sold", "buyer", "buyer TEXT NOT NULL DEFAULT ''");
        self.ensure_column("posted", "item_uuid", "item_uuid TEXT");
        self.ensure_column("posted", "bought_at", "bought_at INTEGER");
        self.ensure_column("posted", "base_key", "base_key TEXT");
        self.ensure_column("posted", "delivered", "delivered INTEGER");
        self.ensure_column("posted", "gone_at", "gone_at INTEGER");
        self.ensure_column("posted", "est_roi", "est_roi REAL");
        self.ensure_column("posted", "conf", "conf REAL");
        self.ensure_column("posted", "samples", "samples INTEGER");
        self.ensure_column("posted", "volatility", "volatility REAL");
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_posted_item ON posted (item_uuid);
             CREATE INDEX IF NOT EXISTS idx_posted_base ON posted (base_key);
             -- unlisted_purchases_by_item_uuid runs at the TOP of every sweep, on
             -- the critical path to the first flip. Without this it SCANs all of
             -- `posted` and sorts in a temp B-tree: 190ms per sweep, growing with
             -- the table. Partial index on the exact predicate: 190ms -> 14ms.
             CREATE INDEX IF NOT EXISTS idx_posted_unlisted ON posted (bought_at)
                 WHERE sold_at IS NULL AND item_uuid IS NOT NULL;",
        )?;
        Ok(())
    }

    fn ensure_column(&self, table: &str, col: &str, ddl: &str) {
        let existing: Vec<String> = self
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .and_then(|mut st| {
                st.query_map([], |r| r.get::<_, String>(1))
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();
        if !existing.is_empty() && !existing.iter().any(|c| c == col) {
            let _ = self
                .conn
                .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {ddl}"));
        }
    }

    /// `loadReferences()` — recent pricing window into RAM. Corrupt rows skipped.
    pub fn load_references(&self, now_ms: i64) -> rusqlite::Result<Vec<Reference>> {
        let cutoff = (now_ms / 1000) - (*REF_MAX_AGE_DAYS as i64) * 86400;
        let mut stmt = self.conn.prepare(
            "SELECT price, sold_at, seller, attrs, tts_ms FROM sold WHERE sold_at >= ?1",
        )?;
        // Size the Vec up front. 3.7M references at ~200 bytes of `Reference` is
        // ~740MB, and growing that by doubling copies well over a gigabyte for
        // nothing. The count is a covering-index scan on idx_sold_time, measured
        // at 72ms on prod against an 11.1s load, so it pays for itself many times.
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sold WHERE sold_at >= ?1",
                [cutoff],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let mut refs = Vec::with_capacity(n.max(0) as usize);
        let rows = stmt.query_map([cutoff], |row| {
            let price: i64 = row.get(0)?;
            let sold_at: i64 = row.get(1)?;
            let seller: Option<String> = row.get(2)?;
            // ⚠️ get_ref, NOT get::<String>. The attrs blob is ~1.5KB and there are
            // 3.7M of them; `get::<String>` allocated and memcpy'd every one purely
            // to hand it to serde and drop it. `as_str` borrows straight out of
            // SQLite's own row buffer, so the parse reads the bytes in place.
            let attrs_s = row.get_ref(3)?.as_str().unwrap_or("");
            let tts_ms: Option<i64> = row.get(4)?;
            // Parsed HERE so the borrow stays inside the closure. A row whose attrs
            // do not parse is skipped, exactly as before.
            Ok(serde_json::from_str(attrs_s).ok().map(|attrs| Reference {
                price: price as f64,
                sold_at: sold_at as f64,
                seller: seller.unwrap_or_default(),
                tts_ms: tts_ms.map(|t| t as f64),
                attrs,
            }))
        })?;
        for r in rows {
            if let Some(rf) = r? {
                refs.push(rf);
            }
        }
        Ok(refs)
    }

    /// The right-censored half of the time-to-sell data: per item id, the hours
    /// each listing was observed for before the censor sweep took it still unsold.
    ///
    /// Same window as [`Store::load_references`], so the survival curve and the
    /// price pools are measured over the same span. `censored` carries no attrs and
    /// no price, which is why this is item-level and not key-level.
    ///
    /// Cheap next to `load_references`: 836k rows of two small columns against
    /// 1.2M rows of ~1.5KB attrs blobs.
    pub fn load_censored(&self, now_ms: i64) -> rusqlite::Result<FxHashMap<String, Vec<f64>>> {
        let cutoff = (now_ms / 1000) - (*REF_MAX_AGE_DAYS as i64) * 86400;
        let mut stmt = self
            .conn
            .prepare("SELECT item_id, lifetime_ms FROM censored WHERE censored_at >= ?1")?;
        let rows = stmt.query_map([cutoff], |row| {
            let item_id: String = row.get(0)?;
            let lifetime_ms: i64 = row.get(1)?;
            Ok((item_id, lifetime_ms))
        })?;
        let mut out: FxHashMap<String, Vec<f64>> = FxHashMap::default();
        for r in rows {
            let (item_id, lifetime_ms) = r?;
            if lifetime_ms < 0 || item_id.is_empty() {
                continue;
            }
            out.entry(item_id)
                .or_default()
                .push(lifetime_ms as f64 / 3_600_000.0);
        }
        Ok(out)
    }

    pub fn sold_count(&self) -> rusqlite::Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM sold", [], |r| r.get(0))
    }

    /// Record that pageflipper's crawler laid eyes on this exact (item name, BIN
    /// price) — regardless of whether it cleared any profit gate. `INSERT OR
    /// IGNORE` on the `(item_name, price)` key means a later re-scan of the same
    /// still-listed auction can't overwrite the true first-seen time. Batched in
    /// one transaction since a single page can carry hundreds of candidates.
    pub fn record_pf_sightings(
        &mut self,
        rows: &[(String, i64, String, i64, i64)],
    ) -> rusqlite::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut ins = tx.prepare_cached(
                "INSERT OR IGNORE INTO pf_sightings (item_name, price, seller, crawl_ts, page) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (item_name, price, seller, crawl_ts, page) in rows {
                ins.execute(params![item_name, price, seller, crawl_ts, page])?;
            }
        }
        tx.commit()
    }

    /// Was this (item name, price) seen by the pageflipper crawler, and when?
    /// Used to compare against a COFL-reported purchase of the same listing.
    pub fn lookup_pf_sighting(
        &self,
        item_name: &str,
        price: i64,
    ) -> rusqlite::Result<Option<(i64, String)>> {
        self.conn
            .query_row(
                "SELECT crawl_ts, seller FROM pf_sightings WHERE item_name = ?1 AND price = ?2",
                params![item_name, price],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
    }

    /// Drop sightings older than `cutoff_ms` — this table only needs to cover the
    /// short window between a crawl and a COFL purchase report, not history.
    pub fn prune_pf_sightings(&self, cutoff_ms: i64) -> rusqlite::Result<usize> {
        self.conn.execute(
            "DELETE FROM pf_sightings WHERE crawl_ts < ?1",
            params![cutoff_ms],
        )
    }

    /// `insertSold` — persist genuinely-new sales, returning the set actually
    /// inserted (INSERT OR IGNORE hits). Stamps tts_ms from the recorded listing
    /// `start` when known+sane and consumes that listing row.
    pub fn insert_sold(&mut self, rows: &[SoldRow]) -> rusqlite::Result<HashSet<String>> {
        let tts_max_ms = TTS_MAX_DAYS * 86400 * 1000;
        let tx = self.conn.transaction()?;
        let mut inserted = HashSet::new();
        {
            let mut ins = tx.prepare_cached(
                "INSERT OR IGNORE INTO sold (auction_id, item_id, price, bin, sold_at, attrs, seller, tts_ms, buyer)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            let mut find_start =
                tx.prepare_cached("SELECT start FROM listing WHERE auction_id = ?1")?;
            let mut del_listing = tx.prepare_cached("DELETE FROM listing WHERE auction_id = ?1")?;
            for r in rows {
                let lstart: Option<i64> = if self.tts_capture {
                    find_start.query_row([&r.auction_id], |row| row.get(0)).ok()
                } else {
                    None
                };
                let tts_ms: Option<i64> = match lstart {
                    Some(start) if start > 0 => {
                        let t = r.sold_at * 1000 - start;
                        if (0..=tts_max_ms).contains(&t) {
                            Some(t)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                let attrs_json = serde_json::to_string(&r.attrs).unwrap_or_else(|_| "{}".into());
                let changes = ins.execute(params![
                    r.auction_id,
                    r.attrs.id,
                    r.price.round() as i64,
                    if r.bin { 1 } else { 0 },
                    r.sold_at,
                    attrs_json,
                    r.seller,
                    tts_ms,
                    r.buyer,
                ])?;
                if changes > 0 {
                    inserted.insert(r.auction_id.clone());
                    if lstart.is_some() {
                        del_listing.execute([&r.auction_id])?;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// `recordListings` — record NEW BIN listings' `start` for later TTS. No-op
    /// when TTS capture is disabled or the batch is empty.
    pub fn record_listings(&mut self, rows: &[ListingRow]) -> rusqlite::Result<()> {
        if !self.tts_capture || rows.is_empty() {
            return Ok(());
        }
        let now = now_s();
        let tx = self.conn.transaction()?;
        {
            let mut ins = tx.prepare_cached(
                "INSERT OR IGNORE INTO listing (auction_id, start, first_seen, item_id)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for r in rows {
                if r.start > 0 {
                    ins.execute(params![r.auction_id, r.start, now, r.item_id])?;
                }
            }
        }
        tx.commit()
    }

    /// `pruneListings` — sweep listings that never sold within ttsCensorDays into
    /// `censored`; returns the number moved.
    pub fn prune_listings(&mut self) -> rusqlite::Result<usize> {
        if !self.tts_capture {
            return Ok(0);
        }
        let now = now_s();
        let cutoff = now - TTS_CENSOR_DAYS * 86400;
        let tx = self.conn.transaction()?;
        let moved = tx.execute(
            "INSERT OR IGNORE INTO censored (auction_id, item_id, start, censored_at, lifetime_ms)
             SELECT auction_id, item_id, start, ?1, (?1 * 1000 - start) FROM listing WHERE first_seen < ?2",
            params![now, cutoff],
        )?;
        tx.execute("DELETE FROM listing WHERE first_seen < ?1", params![cutoff])?;
        tx.commit()?;
        Ok(moved)
    }

    /// `pruneOld` — drop references past the retention window. Returns removed count.
    pub fn prune_old(&self) -> rusqlite::Result<usize> {
        let cutoff = now_s() - *RETENTION_DAYS * 86400;
        self.conn
            .execute("DELETE FROM sold WHERE sold_at < ?1", params![cutoff])
    }

    /// `recordPosted` — remember a pushed flip for later resale validation.
    /// Batched sibling of [`Store::record_posted`], written in ONE transaction.
    ///
    /// One autocommit INSERT per emitted flip is one commit per flip, and that
    /// ran on the sweep loop inside `emit`. Batching a sweep's rows into a single
    /// transaction turns N commits into one.
    pub fn record_posted_batch(&mut self, rows: &[PostedRow]) -> rusqlite::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut ins = tx.prepare_cached(
                "INSERT OR IGNORE INTO posted
                 (uuid, item_name, finder, buy, reference, posted_at, item_uuid, base_key, delivered, est_roi, conf, samples, volatility)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?;
            let now = now_s();
            for p in rows {
                ins.execute(params![
                    p.uuid,
                    p.item_name,
                    p.finder,
                    p.buy.round() as i64,
                    p.reference.round() as i64,
                    now,
                    p.item_uuid,
                    p.base_key,
                    if p.delivered { 1 } else { 0 },
                    p.est_roi,
                    p.conf,
                    p.samples,
                    p.volatility,
                ])?;
            }
        }
        tx.commit()
    }

    pub fn record_posted(&self, p: &PostedRow) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO posted
             (uuid, item_name, finder, buy, reference, posted_at, item_uuid, base_key, delivered, est_roi, conf, samples, volatility)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                p.uuid,
                p.item_name,
                p.finder,
                p.buy.round() as i64,
                p.reference.round() as i64,
                now_s(),
                p.item_uuid,
                p.base_key,
                if p.delivered { 1 } else { 0 },
                p.est_roi,
                p.conf,
                p.samples,
                p.volatility,
            ],
        )?;
        Ok(())
    }

    /// `unsoldHeldCount` — flood-brake count for a base item (bought-unsold ≤48h
    /// plus delivered pushes ≤10min not yet reconciled).
    pub fn unsold_held_count(&self, base_key: &str) -> i64 {
        let now = now_s();
        self.conn
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM posted WHERE base_key = ?1 AND bought_at IS NOT NULL AND sold_at IS NULL AND gone_at IS NULL AND posted_at >= ?2)
                 + (SELECT COUNT(*) FROM posted WHERE base_key = ?1 AND bought_at IS NULL AND sold_at IS NULL AND gone_at IS NULL AND delivered = 1 AND posted_at >= ?3)",
                params![base_key, now - 48 * 3600, now - 600],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    /// The `~variant` we keyed an item under when we BOUGHT it, by item_uuid.
    ///
    /// The sell side cannot re-derive a lore-only component: the mod's inventory
    /// slots carry `ExtraAttributes` and nothing else (0 of 929 golden fixtures
    /// contain `Lore`), so a heavy Loudmouth Bass decodes as a plain one and
    /// would be listed against the 1.20M pooled median. We saw the real value at
    /// buy time, so carry it forward rather than trying to recover it.
    pub fn bought_variant(&self, item_uuid: &str) -> Option<String> {
        let base_key: String = self
            .conn
            .query_row(
                "SELECT base_key FROM posted
                 WHERE item_uuid = ?1 AND base_key IS NOT NULL
                 ORDER BY posted_at DESC LIMIT 1",
                params![item_uuid],
                |r| r.get(0),
            )
            .ok()?;
        // base_key is `ID[*stars][xN][~variant]`; we want the variant only.
        base_key.split_once('~').map(|(_, v)| v.to_string())
    }

    /// `reconcileHeldAgainstInventory` — mark bought-unsold flips `gone_at` when
    /// their item_uuid is no longer in the bot's inventory. Returns rows marked.
    pub fn reconcile_held_against_inventory(
        &mut self,
        present: &HashSet<String>,
        grace_sec: i64,
    ) -> rusqlite::Result<usize> {
        let now = now_s();
        let cutoff = now - grace_sec;
        let candidates: Vec<(String, String)> = {
            let mut stmt = self.conn.prepare(
                "SELECT uuid, item_uuid FROM posted
                 WHERE bought_at IS NOT NULL AND sold_at IS NULL AND gone_at IS NULL
                   AND item_uuid IS NOT NULL AND bought_at <= ?1",
            )?;
            let rows = stmt.query_map([cutoff], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            rows.filter_map(|r| r.ok()).collect()
        };
        let missing: Vec<String> = candidates
            .into_iter()
            .filter(|(_, item_uuid)| !present.contains(item_uuid))
            .map(|(uuid, _)| uuid)
            .collect();
        if missing.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        let mut gone = 0;
        {
            let mut mark = tx.prepare_cached(
                "UPDATE posted SET gone_at = ?1 WHERE uuid = ?2 AND gone_at IS NULL",
            )?;
            for uuid in &missing {
                gone += mark.execute(params![now, uuid])?;
            }
        }
        tx.commit()?;
        Ok(gone)
    }

    /// Find a bought-but-not-yet-listed flip by the physical item's NBT uuid, so
    /// the self-listing poll can link a bot's live auction back to what it paid,
    /// without any report from the mod. `NOT IN listing_uuids` skips a flip that
    /// already has a real listing recorded (relist after a first listing expired).
    pub fn find_unlisted_purchase_by_item_uuid(
        &self,
        item_uuid: &str,
    ) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT uuid FROM posted
                 WHERE item_uuid = ?1 AND bought_at IS NOT NULL AND sold_at IS NULL
                   AND uuid NOT IN (SELECT flip_uuid FROM listing_uuids)
                 ORDER BY bought_at DESC LIMIT 1",
                params![item_uuid],
                |r| r.get(0),
            )
            .optional()
    }

    /// Every bought-but-unlisted flip, keyed by the physical item's NBT uuid.
    ///
    /// The sweep-side replacement for the per-bot NetherAPI self-listing poll.
    /// Our own relist is a brand-new BIN, so it lands on page 0 of the dump we
    /// already fetch every sweep: matching against this map costs one indexed
    /// query per sweep and zero API requests, versus one request per lister bot
    /// every 45s (which alone consumed most of a 90-per-5-minute key budget).
    /// It is also faster, since page 0 arrives in ~7s rather than up to 45s.
    ///
    /// Newest purchase wins per item, matching the `ORDER BY bought_at DESC
    /// LIMIT 1` of `find_unlisted_purchase_by_item_uuid` (ASC + overwrite here).
    pub fn unlisted_purchases_by_item_uuid(&self) -> rusqlite::Result<HashMap<String, String>> {
        // Bounded on purpose. `bought_at` is set when the flagged auction ends --
        // bought by ANYONE, not necessarily us -- so the unbounded predicate
        // matched 64,310 of 139,235 rows and grew forever, costing ~190ms at the
        // top of EVERY sweep before a single flip could be evaluated. The map only
        // exists to link a bot's relist back to its purchase, and a relist follows
        // its buy by minutes, so a multi-day window is already generous.
        let cutoff = now_s() - *UNLISTED_LOOKBACK_S;
        let mut st = self.conn.prepare_cached(
            "SELECT item_uuid, uuid FROM posted
             WHERE item_uuid IS NOT NULL AND item_uuid <> '' AND bought_at IS NOT NULL AND sold_at IS NULL
               AND bought_at > ?1
               AND uuid NOT IN (SELECT flip_uuid FROM listing_uuids)
             ORDER BY bought_at ASC",
        )?;
        let rows = st.query_map([cutoff], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out: HashMap<String, String> = HashMap::new();
        for row in rows {
            let (item_uuid, flip_uuid) = row?;
            out.insert(item_uuid, flip_uuid);
        }
        Ok(out)
    }

    /// `recordListingUuid` — associate a bot listing uuid with its flip.
    pub fn record_listing_uuid(
        &self,
        listing_uuid: &str,
        flip_uuid: &str,
        item_name: Option<&str>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO listing_uuids (listing_uuid, flip_uuid, item_name, listed_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![listing_uuid, flip_uuid, item_name, now_s()],
        )?;
        Ok(())
    }

    /// `reconcilePosted` — reconcile ended auctions against posted flips.
    /// Listing-uuid or item_uuid match ⇒ resale (sold_at set); direct uuid match
    /// ⇒ bought only.
    pub fn reconcile_posted(
        &mut self,
        ended: &[EndedRow],
    ) -> rusqlite::Result<(Vec<ResolvedFlip>, usize)> {
        let mut resolved = Vec::new();
        let mut bought = 0usize;
        let tx = self.conn.transaction()?;
        {
            let mut listing_match =
                tx.prepare_cached("SELECT flip_uuid FROM listing_uuids WHERE listing_uuid = ?1")?;
            let mut by_uuid = tx.prepare_cached(
                "SELECT uuid, item_name, finder, buy, reference FROM posted WHERE uuid = ?1 AND sold_at IS NULL",
            )?;
            let mut by_item = tx.prepare_cached(
                "SELECT uuid, item_name, finder, buy, reference FROM posted
                 WHERE item_uuid = ?1 AND uuid != ?2 AND sold_at IS NULL AND posted_at <= ?3",
            )?;
            let mut mark_sold = tx.prepare_cached("UPDATE posted SET sold_price = ?1, sold_at = ?2 WHERE uuid = ?3 AND sold_at IS NULL")?;
            let mut mark_bought = tx.prepare_cached(
                "UPDATE posted SET bought_at = ?1 WHERE uuid = ?2 AND bought_at IS NULL",
            )?;

            let row_map =
                |row: &rusqlite::Row| -> rusqlite::Result<(String, String, String, i64, i64)> {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                };
            for r in ended {
                // 1) explicit listing-uuid match → resale
                let flip_uuid: Option<String> =
                    listing_match.query_row([&r.uuid], |row| row.get(0)).ok();
                if let Some(fu) = flip_uuid {
                    if let Ok((uuid, item_name, finder, buy, reference)) =
                        by_uuid.query_row([&fu], row_map)
                    {
                        mark_sold.execute(params![r.price.round() as i64, r.sold_at, uuid])?;
                        resolved.push(ResolvedFlip {
                            uuid,
                            item_name,
                            finder,
                            buy: buy as f64,
                            reference: reference as f64,
                            sold_price: r.price.round(),
                            via: "listing",
                        });
                    }
                    continue;
                }
                // 2) the flip auction itself ended → bought
                let direct: Option<String> = by_uuid.query_row([&r.uuid], |row| row.get(0)).ok();
                if direct.is_some() {
                    bought += mark_bought.execute(params![r.sold_at, r.uuid])?;
                    continue;
                }
                // 3) same physical item resold under a new auction uuid → resale
                if let Some(item_uuid) = &r.item_uuid {
                    if let Ok((uuid, item_name, finder, buy, reference)) =
                        by_item.query_row(params![item_uuid, r.uuid, r.sold_at], row_map)
                    {
                        mark_sold.execute(params![r.price.round() as i64, r.sold_at, uuid])?;
                        resolved.push(ResolvedFlip {
                            uuid,
                            item_name,
                            finder,
                            buy: buy as f64,
                            reference: reference as f64,
                            sold_price: r.price.round(),
                            via: "item_uuid",
                        });
                    }
                }
            }
        }
        tx.commit()?;
        Ok((resolved, bought))
    }

    /// `medianPostedBuy` — median buy of flips posted in the last `days` (min
    /// samples guard). None when too few rows.
    pub fn median_posted_buy(&self, days: i64, min_samples: usize) -> Option<i64> {
        let since = now_s() - days * 86400;
        let buys: Vec<i64> = self
            .conn
            .prepare("SELECT buy FROM posted WHERE posted_at >= ?1 AND buy IS NOT NULL AND buy > 0 ORDER BY buy")
            .ok()?
            .query_map([since], |r| r.get(0))
            .ok()?
            .filter_map(|r| r.ok())
            .collect();
        if buys.len() < min_samples {
            return None;
        }
        let mid = buys.len() >> 1;
        Some(if buys.len() % 2 == 1 {
            buys[mid]
        } else {
            ((buys[mid - 1] + buys[mid]) as f64 / 2.0).round() as i64
        })
    }

    /// `postedStats` — hit-rate summary over the last `days`.
    pub fn posted_stats(&self, days: i64) -> (i64, i64, i64, f64) {
        let since = now_s() - days * 86400;
        self.conn
            .query_row(
                "SELECT COUNT(*), COUNT(bought_at), COUNT(sold_at) FROM posted WHERE posted_at >= ?1",
                params![since],
                |r| {
                    let posted: i64 = r.get(0)?;
                    let bought: i64 = r.get(1)?;
                    let sold: i64 = r.get(2)?;
                    let hit = if posted > 0 { sold as f64 / posted as f64 } else { 0.0 };
                    Ok((posted, bought, sold, hit))
                },
            )
            .unwrap_or((0, 0, 0, 0.0))
    }
}
