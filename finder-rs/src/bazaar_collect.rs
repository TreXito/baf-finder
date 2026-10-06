//! Bazaar snapshot collector: periodically records the full Hypixel bazaar
//! (`/v2/skyblock/bazaar`) into a DEDICATED sqlite file so a future bazaar
//! *finder* has price/volume history to work with.
//!
//! Deliberately independent of the auction finder: its own DB file, its own
//! thread, no shared state, no shared connection. It only READS the public
//! bazaar endpoint and WRITES its own file, so it can never affect a flip
//! decision or the golden parity of the money core.
//!
//! Storage: products are interned into `bz_product` (int id, not a repeated
//! string) and each snapshot row is keyed `(ts, product)` where `ts` is the
//! API's `lastUpdated`. Re-fetching an unchanged bazaar is a cheap
//! INSERT-OR-IGNORE no-op, so a short poll interval does not bloat the file.
//!
//! Modes / env (all optional):
//!   BAZAAR_COLLECT_ONLY=1   run ONLY the collector loop (dedicated process) and exit
//!   BAZAAR_COLLECT_OFF=1    do NOT auto-spawn the collector inside the serve loop
//!   BAZAAR_DB_PATH=<file>   collector db (default: `bazaar.sqlite` beside the finder db)
//!   BAZAAR_INTERVAL_MS=<n>  poll cadence (default 60000)
//!   BAZAAR_RETENTION_DAYS=<n>  prune snapshots older than n days (default 30; 0 = keep forever)

use crate::hypixel::{self, BazaarProduct};
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// The collector's sqlite persistence. Owns its own connection + an in-RAM
/// product-id cache (tag → interned id) so repeated snapshots skip the lookup.
pub struct BazaarStore {
    conn: Connection,
    ids: HashMap<String, i64>,
}

impl BazaarStore {
    pub fn open(db_path: &str) -> rusqlite::Result<Self> {
        if let Some(parent) = std::path::Path::new(db_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let mut s = BazaarStore {
            conn,
            ids: HashMap::new(),
        };
        s.init_schema()?;
        s.load_ids();
        Ok(s)
    }

    fn init_schema(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS bz_product (
                id  INTEGER PRIMARY KEY,
                tag TEXT NOT NULL UNIQUE
            );
            CREATE TABLE IF NOT EXISTS bz_snapshot (
                ts               INTEGER NOT NULL,  -- bazaar lastUpdated (unix ms)
                product          INTEGER NOT NULL,  -- FK bz_product.id
                buy_price        REAL    NOT NULL,  -- quick_status.buyPrice  (insta-buy avg)
                sell_price       REAL    NOT NULL,  -- quick_status.sellPrice (insta-sell avg)
                buy_volume       INTEGER NOT NULL,  -- units available to insta-buy
                sell_volume      INTEGER NOT NULL,  -- buy-order demand
                buy_moving_week  INTEGER NOT NULL,
                sell_moving_week INTEGER NOT NULL,
                buy_orders       INTEGER NOT NULL,
                sell_orders      INTEGER NOT NULL,
                top_insta_buy    REAL    NOT NULL,  -- best (lowest) sell-offer unit price
                top_insta_sell   REAL    NOT NULL,  -- best (highest) buy-order unit price
                PRIMARY KEY (ts, product)
            ) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS idx_bz_snap_product ON bz_snapshot (product, ts);
            "#,
        )?;
        // Queue depth AT the top of each side, added after the fact: the finder
        // has to estimate how long an order sits before it fills, and the
        // aggregate volumes can't say how much of the book is stacked at the one
        // price that matters. Added as ALTER so an existing 5.7GB collection
        // keeps its history instead of being rebuilt; old rows read back 0.
        for col in [
            "top_insta_buy_amount",
            "top_insta_buy_orders",
            "top_insta_sell_amount",
            "top_insta_sell_orders",
        ] {
            let _ = self.conn.execute(
                &format!("ALTER TABLE bz_snapshot ADD COLUMN {col} INTEGER NOT NULL DEFAULT 0"),
                [],
            );
        }
        Ok(())
    }

    fn load_ids(&mut self) {
        if let Ok(mut st) = self.conn.prepare("SELECT id, tag FROM bz_product") {
            if let Ok(rows) =
                st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            {
                for (id, tag) in rows.flatten() {
                    self.ids.insert(tag, id);
                }
            }
        }
    }

    /// Persist one whole-bazaar snapshot. Returns rows actually inserted (0 when
    /// this `ts` was already stored, e.g. the bazaar hasn't updated since last poll).
    pub fn insert_snapshot(
        &mut self,
        ts: i64,
        products: &HashMap<String, BazaarProduct>,
    ) -> rusqlite::Result<usize> {
        // Disjoint borrows: the tx borrows `conn`, the cache mutates `ids`.
        let Self { conn, ids } = self;
        let tx = conn.transaction()?;
        let mut inserted = 0usize;
        {
            let mut ensure =
                tx.prepare_cached("INSERT OR IGNORE INTO bz_product (tag) VALUES (?1)")?;
            let mut getid = tx.prepare_cached("SELECT id FROM bz_product WHERE tag = ?1")?;
            let mut ins = tx.prepare_cached(
                "INSERT OR IGNORE INTO bz_snapshot
                   (ts, product, buy_price, sell_price, buy_volume, sell_volume,
                    buy_moving_week, sell_moving_week, buy_orders, sell_orders,
                    top_insta_buy, top_insta_sell,
                    top_insta_buy_amount, top_insta_buy_orders,
                    top_insta_sell_amount, top_insta_sell_orders)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            )?;
            for (tag, p) in products {
                let pid = match ids.get(tag) {
                    Some(&id) => id,
                    None => {
                        ensure.execute([tag])?;
                        let id: i64 = getid.query_row([tag], |r| r.get(0))?;
                        ids.insert(tag.clone(), id);
                        id
                    }
                };
                inserted += ins.execute(params![
                    ts,
                    pid,
                    p.buy_price,
                    p.sell_price,
                    p.buy_volume,
                    p.sell_volume,
                    p.buy_moving_week,
                    p.sell_moving_week,
                    p.buy_orders,
                    p.sell_orders,
                    p.top_insta_buy,
                    p.top_insta_sell,
                    p.top_insta_buy_amount,
                    p.top_insta_buy_orders,
                    p.top_insta_sell_amount,
                    p.top_insta_sell_orders,
                ])?;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Drop snapshot rows older than `retention_days`. Returns rows removed.
    pub fn prune(&self, retention_days: i64) -> rusqlite::Result<usize> {
        let cutoff = now_ms() - retention_days * 86_400_000;
        self.conn
            .execute("DELETE FROM bz_snapshot WHERE ts < ?1", [cutoff])
    }

    pub fn snapshot_rows(&self) -> i64 {
        self.conn
            .query_row("SELECT COUNT(*) FROM bz_snapshot", [], |r| r.get(0))
            .unwrap_or(0)
    }
}

/// Where the collector writes, and therefore where the bazaar finder reads.
pub fn db_path(sibling: &str) -> String {
    if let Ok(p) = std::env::var("BAZAAR_DB_PATH") {
        return p;
    }
    std::path::Path::new(sibling)
        .parent()
        .map(|d| d.join("bazaar.sqlite").to_string_lossy().into_owned())
        .unwrap_or_else(|| "./bazaar.sqlite".to_string())
}

fn interval_ms() -> u64 {
    std::env::var("BAZAAR_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60_000)
}

fn retention_days() -> i64 {
    std::env::var("BAZAAR_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
}

/// Fetch once and store it. Never panics; logs and returns on any failure so the
/// caller loop keeps going.
fn collect_once(store: &mut BazaarStore) {
    let Some(full) = hypixel::fetch_bazaar_full() else {
        return; // fetch_bazaar_full already logged the reason
    };
    // Guard a missing/zero lastUpdated so dedup keying can't collapse distinct
    // polls onto ts=0.
    let ts = if full.last_updated > 0 {
        full.last_updated
    } else {
        now_ms()
    };
    match store.insert_snapshot(ts, &full.products) {
        Ok(n) => eprintln!(
            "bazaar-collect: ts={ts} products={} new_rows={n}",
            full.products.len()
        ),
        Err(e) => eprintln!("bazaar-collect: insert failed: {e}"),
    }
}

/// The blocking collector loop: poll → store → (every 6h) prune. Runs until the
/// process exits. Used both as the standalone mode and inside the serve thread.
pub fn run_collector(db_path: &str, interval_ms: u64, retention_days: i64) {
    let mut store = match BazaarStore::open(db_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bazaar-collect: open {db_path} failed: {e} (collector NOT running)");
            return;
        }
    };
    eprintln!(
        "bazaar-collect: db={db_path} interval={interval_ms}ms retention={}d existing_rows={}",
        retention_days,
        store.snapshot_rows()
    );
    let interval = Duration::from_millis(interval_ms.max(1000));
    let prune_every = Duration::from_secs(6 * 3600);
    let mut last_prune = Instant::now();
    loop {
        let t = Instant::now();
        collect_once(&mut store);
        if retention_days > 0 && last_prune.elapsed() >= prune_every {
            match store.prune(retention_days) {
                Ok(n) if n > 0 => {
                    eprintln!("bazaar-collect: pruned {n} rows past {retention_days}d")
                }
                Err(e) => eprintln!("bazaar-collect: prune failed: {e}"),
                _ => {}
            }
            last_prune = Instant::now();
        }
        if let Some(rem) = interval.checked_sub(t.elapsed()) {
            std::thread::sleep(rem);
        }
    }
}

/// Standalone entry (BAZAAR_COLLECT_ONLY=1): read env, run the loop, never return.
pub fn run_from_env(sibling: &str) {
    run_collector(&db_path(sibling), interval_ms(), retention_days());
}

/// Auto-spawn the collector on a background thread from the serve loop, unless
/// BAZAAR_COLLECT_OFF=1. `sibling` seeds the default db path (dir of the finder db).
pub fn spawn(sibling: &str) {
    if std::env::var("BAZAAR_COLLECT_OFF").as_deref() == Ok("1") {
        eprintln!("bazaar-collect: DISABLED (BAZAAR_COLLECT_OFF=1)");
        return;
    }
    let (path, iv, rd) = (db_path(sibling), interval_ms(), retention_days());
    std::thread::spawn(move || run_collector(&path, iv, rd));
}
