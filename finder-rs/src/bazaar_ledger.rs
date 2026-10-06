//! Realised profit and loss for the bazaar finder.
//!
//! Until this existed the module had **no P&L at all**. `bazaar-positions.json`
//! holds only what is still open, and the one moment where a trade's result is
//! knowable — `BazaarChat::SellFilled`, which arrives holding both
//! `Position::buy_price` and `Position::sell_price` — threw the sell price away
//! (`p.sell_price = None`) on the very next line and then deleted the position.
//! The "+133.6M / +22.1%" figure in [[finder-bazaar-is-profitable]] had to be
//! reconstructed by hand from 5.6GB of bot chat logs, because the finder itself
//! recorded nothing.
//!
//! # What is banked, and what is NOT
//!
//! One line per realised sale, appended to `BZ_LEDGER_PATH` (default
//! `bazaar-ledger.jsonl` beside the positions file). Append-only JSONL rather
//! than a rewritten blob so a crash mid-write can lose at most the last record,
//! never the history, and so it can be tailed and cut with ordinary tools.
//!
//! ⚠️ **Every record carries a `source`, and they are not equally true.** Mixing
//! them into one number is how a P&L starts lying:
//!
//! - `chat` — Hypixel itself said the units sold. Trustworthy.
//! - `settled` — the sell offer rested past `BZ_ABANDON_AFTER_MIN` and we banked
//!   it without confirmation. This is an ASSUMPTION; the units may still be
//!   sitting on the book. Counted separately.
//! - `orphan` — untracked stock liquidated with no real cost basis. The finder
//!   sets `buy_price` to the ask so the position cannot look infinitely
//!   profitable, which means its "profit" is manufactured. **Excluded from ROI
//!   entirely** and reported only as coins recovered.
//!
//! Anything that is not `chat` is quarantined out of the headline, because
//! [[finder-bazaar-is-profitable]] already found that 62% of measured losses came
//! from a single mispriced order: a P&L that averages a guess with a fact hides
//! exactly that class of event.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::sync::Mutex;

/// How a sale came to be recorded. See the module note: these do not mix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Hypixel's chat confirmed the fill. The only fully trusted source.
    Chat,
    /// We gave up waiting and assumed the resting offer sold.
    Settled,
    /// Untracked inventory liquidated against a synthetic cost basis.
    Orphan,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Chat => "chat",
            Source::Settled => "settled",
            Source::Orphan => "orphan",
        }
    }
}

/// One realised sale.
///
/// `net` is what actually reached the purse: proceeds after Hypixel's bazaar tax
/// (`BZ_TAX`, 1.25%) minus what the units cost us. It is stored rather than
/// derived so a later change to the tax constant cannot silently rewrite
/// history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fill {
    pub ts: i64,
    pub tag: String,
    pub name: String,
    pub bot: String,
    pub units: f64,
    pub buy_price: f64,
    pub sell_price: f64,
    /// `units * sell_price`, before tax.
    pub gross: f64,
    /// Coins Hypixel took.
    pub tax: f64,
    /// `gross - tax - units * buy_price`.
    pub net: f64,
    /// `net / (units * buy_price)`, as a percentage. `None` when the basis is
    /// zero, which is the orphan case and must not become a division by zero.
    pub roi_pct: Option<f64>,
    /// Buy order placed to sale confirmed.
    pub hold_ms: i64,
    pub source: Source,
}

/// Aggregate over a slice of the ledger.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Summary {
    pub trades: usize,
    pub units: f64,
    pub cost: f64,
    pub gross: f64,
    pub tax: f64,
    pub net: f64,
    /// `net / cost * 100`. Zero cost yields zero rather than infinity.
    pub roi_pct: f64,
    pub wins: usize,
    pub losses: usize,
    /// Median hold in minutes, buy placed to sale confirmed.
    pub median_hold_min: f64,
}

impl Summary {
    fn of(fills: &[&Fill]) -> Summary {
        let mut s = Summary {
            trades: fills.len(),
            ..Default::default()
        };
        let mut holds: Vec<i64> = Vec::with_capacity(fills.len());
        for f in fills {
            s.units += f.units;
            s.cost += f.units * f.buy_price;
            s.gross += f.gross;
            s.tax += f.tax;
            s.net += f.net;
            if f.net >= 0.0 {
                s.wins += 1;
            } else {
                s.losses += 1;
            }
            holds.push(f.hold_ms);
        }
        s.roi_pct = if s.cost > 0.0 {
            s.net / s.cost * 100.0
        } else {
            0.0
        };
        holds.sort_unstable();
        s.median_hold_min = holds
            .get(holds.len() / 2)
            .map(|m| *m as f64 / 60_000.0)
            .unwrap_or(0.0);
        s
    }
}

/// Append-only realised-P&L log.
///
/// The whole file is held in memory. At the observed rate — 13 confirmed sell
/// fills in 10.8h — this reaches a few thousand records a year, so the simplicity
/// is worth more than the bytes. `BZ_LEDGER_MAX` caps the in-memory tail if that
/// ever stops being true; the file itself is never truncated.
pub struct BazaarLedger {
    path: String,
    fills: Mutex<Vec<Fill>>,
}

impl BazaarLedger {
    /// Load the existing ledger, or start an empty one.
    ///
    /// A corrupt line is skipped rather than fatal: a torn last write must not
    /// stop the finder from booting, and losing one record is strictly better
    /// than losing the file.
    pub fn load(state_path: &str) -> BazaarLedger {
        let path = std::env::var("BZ_LEDGER_PATH").unwrap_or_else(|_| {
            std::path::Path::new(state_path)
                .parent()
                .map(|d| d.join("bazaar-ledger.jsonl").to_string_lossy().into_owned())
                .unwrap_or_else(|| "./bazaar-ledger.jsonl".to_string())
        });
        BazaarLedger::at(path)
    }

    /// Load from an explicit path.
    ///
    /// Separate from [`BazaarLedger::load`] so tests never touch the process
    /// environment: `BZ_LEDGER_PATH` is global, and setting it from tests that
    /// cargo runs in parallel makes each one read whichever file another test
    /// installed last.
    pub fn at(path: String) -> BazaarLedger {
        let mut fills: Vec<Fill> = Vec::new();
        let mut skipped = 0usize;
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                match serde_json::from_str::<Fill>(line) {
                    Ok(f) => fills.push(f),
                    Err(_) => skipped += 1,
                }
            }
        }
        let cap = std::env::var("BZ_LEDGER_MAX")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(50_000);
        if fills.len() > cap {
            let drop = fills.len() - cap;
            fills.drain(..drop);
        }
        if !fills.is_empty() || skipped > 0 {
            eprintln!(
                "bazaar-ledger: {} realised sale(s) loaded from {}{}",
                fills.len(),
                path,
                if skipped > 0 {
                    format!(" ({skipped} unreadable line(s) skipped)")
                } else {
                    String::new()
                }
            );
        }
        BazaarLedger {
            path,
            fills: Mutex::new(fills),
        }
    }

    /// Bank a realised sale.
    ///
    /// ⚠️ Call this BEFORE clearing `Position::sell_price`. That field is the
    /// only record of what we asked, and the pre-existing `SellFilled` handler
    /// nulls it on the line after the fill is counted.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        tag: &str,
        name: &str,
        bot: &str,
        units: f64,
        buy_price: f64,
        sell_price: f64,
        tax_rate: f64,
        placed_at_ms: i64,
        now: i64,
        source: Source,
    ) {
        if units <= 0.0 || !units.is_finite() || !sell_price.is_finite() || !buy_price.is_finite() {
            return;
        }
        let gross = units * sell_price;
        let tax = gross * tax_rate;
        let cost = units * buy_price;
        let net = gross - tax - cost;
        let fill = Fill {
            ts: now,
            tag: tag.to_string(),
            name: name.to_string(),
            bot: bot.to_string(),
            units,
            buy_price,
            sell_price,
            gross,
            tax,
            net,
            roi_pct: if cost > 0.0 {
                Some(net / cost * 100.0)
            } else {
                None
            },
            hold_ms: (now - placed_at_ms).max(0),
            source,
        };
        eprintln!(
            "bazaar-ledger: {} {} x{:.0} @ {:.1} <- {:.1} | net {:+.0} ({}) | {} | {:.0}min",
            source.as_str(),
            name,
            units,
            sell_price,
            buy_price,
            net,
            fill.roi_pct
                .map(|r| format!("{r:+.1}%"))
                .unwrap_or_else(|| "no basis".to_string()),
            bot,
            fill.hold_ms as f64 / 60_000.0,
        );
        // Append first, remember second: if the write fails the record is not
        // real, and an in-memory total that the file cannot back is worse than
        // no total.
        if let Ok(line) = serde_json::to_string(&fill) {
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                Ok(mut f) => {
                    if let Err(e) = writeln!(f, "{line}") {
                        eprintln!("bazaar-ledger: append to {} failed: {e}", self.path);
                        return;
                    }
                }
                Err(e) => {
                    eprintln!("bazaar-ledger: open {} failed: {e}", self.path);
                    return;
                }
            }
        }
        self.fills.lock().unwrap().push(fill);
    }

    /// Realised P&L over the last `hours`, split by source. `None` = all time.
    ///
    /// Returns `(chat, settled, orphan)`. The caller reports them apart on
    /// purpose; see the module note on why they must not be summed.
    pub fn summarise(&self, hours: Option<f64>, now: i64) -> (Summary, Summary, Summary) {
        let fills = self.fills.lock().unwrap();
        // ⚠️ `i64::MIN`, not 0. "All time" must mean all time: a 0 floor silently
        // drops any record whose timestamp is not a real unix ms, which is
        // exactly what a fixture or a clock-skewed bot writes.
        let cutoff = hours
            .map(|h| now - (h * 3_600_000.0) as i64)
            .unwrap_or(i64::MIN);
        let within: Vec<&Fill> = fills.iter().filter(|f| f.ts >= cutoff).collect();
        let pick = |s: Source| -> Summary {
            let sel: Vec<&Fill> = within.iter().copied().filter(|f| f.source == s).collect();
            Summary::of(&sel)
        };
        (
            pick(Source::Chat),
            pick(Source::Settled),
            pick(Source::Orphan),
        )
    }

    /// Best and worst products by realised net, confirmed sales only.
    ///
    /// Ranked by TOTAL net rather than per-unit margin, because a product that
    /// nets a little on huge volume is worth more attention than one that nets a
    /// lot once. `n` from each end.
    pub fn by_product(&self, hours: Option<f64>, now: i64, n: usize) -> Vec<(String, f64, usize)> {
        let fills = self.fills.lock().unwrap();
        let cutoff = hours
            .map(|h| now - (h * 3_600_000.0) as i64)
            .unwrap_or(i64::MIN);
        let mut agg: std::collections::HashMap<String, (f64, usize)> =
            std::collections::HashMap::new();
        for f in fills
            .iter()
            .filter(|f| f.ts >= cutoff && f.source == Source::Chat)
        {
            let e = agg.entry(f.name.clone()).or_insert((0.0, 0));
            e.0 += f.net;
            e.1 += 1;
        }
        let mut v: Vec<(String, f64, usize)> =
            agg.into_iter().map(|(k, (net, c))| (k, net, c)).collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        if v.len() <= n * 2 {
            return v;
        }
        let mut out: Vec<(String, f64, usize)> = v[..n].to_vec();
        out.extend_from_slice(&v[v.len() - n..]);
        out
    }

    /// One log line: the headline number, plus what is being excluded from it.
    ///
    /// `open_cost` and `open_units` come from the live position book, so the line
    /// answers "did we make money" and "how much is still at risk" together.
    /// Unrealised stock is deliberately NOT marked to market: the whole reason
    /// this module ranks on confirmed fills is that the book lies about what we
    /// can actually get out at ([[finder-bazaar-spread-is-not-opportunity]]).
    pub fn report(&self, now: i64, open_cost: f64, open_units: f64, open_positions: usize) {
        let (d_chat, _, _) = self.summarise(Some(24.0), now);
        let (a_chat, a_settled, a_orphan) = self.summarise(None, now);
        if a_chat.trades == 0 && a_settled.trades == 0 && a_orphan.trades == 0 {
            eprintln!(
                "bazaar-ledger: no realised sales yet | open {open_positions} position(s), \
                 {open_units:.0} units, {open_cost:.0} coins at risk"
            );
            return;
        }
        eprintln!(
            "bazaar-ledger: 24h confirmed {} trades, cost {:.0}, net {:+.0} ({:+.1}%), \
             {}W/{}L, med hold {:.0}min | all-time confirmed {} trades, net {:+.0} ({:+.1}%)",
            d_chat.trades,
            d_chat.cost,
            d_chat.net,
            d_chat.roi_pct,
            d_chat.wins,
            d_chat.losses,
            d_chat.median_hold_min,
            a_chat.trades,
            a_chat.net,
            a_chat.roi_pct,
        );
        eprintln!(
            "bazaar-ledger: EXCLUDED from the above -- assumed-sold {} trades net {:+.0} \
             (unconfirmed), orphan {} trades recovered {:.0} (no real basis) | \
             open {} position(s), {:.0} units, {:.0} coins at risk",
            a_settled.trades,
            a_settled.net,
            a_orphan.trades,
            a_orphan.gross - a_orphan.tax,
            open_positions,
            open_units,
            open_cost,
        );
        let ranked = self.by_product(Some(24.0), now, 3);
        if !ranked.is_empty() {
            let s: Vec<String> = ranked
                .iter()
                .map(|(n, net, c)| format!("{n} {net:+.0}({c})"))
                .collect();
            eprintln!("bazaar-ledger: 24h by product: {}", s.join(" | "));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger(dir: &std::path::Path) -> BazaarLedger {
        BazaarLedger::at(dir.join("l.jsonl").to_string_lossy().into_owned())
    }

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("bzledger-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The arithmetic, on the shape of a real trade: buy 239 Oil Barrel at
    /// 3,480.9, sell at 3,900, 1.25% bazaar tax.
    #[test]
    fn net_is_proceeds_after_tax_minus_cost() {
        let d = tmpdir("net");
        let l = ledger(&d);
        l.record(
            "OIL_BARREL",
            "Oil Barrel",
            "darcyiscoool",
            239.0,
            3_480.9,
            3_900.0,
            0.0125,
            1_000,
            61_000,
            Source::Chat,
        );
        let (chat, _, _) = l.summarise(None, 61_000);
        let gross = 239.0 * 3_900.0;
        let expect = gross - gross * 0.0125 - 239.0 * 3_480.9;
        assert_eq!(chat.trades, 1);
        assert!((chat.net - expect).abs() < 0.01, "got {}", chat.net);
        assert_eq!(chat.wins, 1);
        assert!((chat.median_hold_min - 1.0).abs() < 0.01);
    }

    /// ⛔ The three sources must never be summed into one headline.
    #[test]
    fn sources_are_reported_apart() {
        let d = tmpdir("sources");
        let l = ledger(&d);
        for (src, sell) in [
            (Source::Chat, 200.0),
            (Source::Settled, 200.0),
            (Source::Orphan, 200.0),
        ] {
            l.record("T", "Thing", "bot", 10.0, 100.0, sell, 0.0, 0, 1_000, src);
        }
        let (c, s, o) = l.summarise(None, 1_000);
        assert_eq!((c.trades, s.trades, o.trades), (1, 1, 1));
        assert_eq!(c.net, 1_000.0);
    }

    /// An orphan has no real cost basis, so ROI must be absent, not infinite.
    #[test]
    fn zero_basis_yields_no_roi_not_infinity() {
        let d = tmpdir("orphan");
        let l = ledger(&d);
        l.record(
            "T",
            "Thing",
            "bot",
            5.0,
            0.0,
            50.0,
            0.0125,
            0,
            1_000,
            Source::Orphan,
        );
        let (_, _, orphan) = l.summarise(None, 1_000);
        assert_eq!(orphan.trades, 1);
        assert_eq!(orphan.roi_pct, 0.0, "zero cost must not divide");
        let f = &l.fills.lock().unwrap()[0];
        assert!(f.roi_pct.is_none());
    }

    /// The file is the record; a reload must reproduce the totals exactly.
    #[test]
    fn survives_a_restart() {
        let d = tmpdir("reload");
        {
            let l = ledger(&d);
            l.record("A", "A", "b", 2.0, 10.0, 20.0, 0.0, 0, 100, Source::Chat);
            l.record("B", "B", "b", 3.0, 10.0, 5.0, 0.0, 0, 200, Source::Chat);
        }
        let l2 = ledger(&d);
        let (chat, _, _) = l2.summarise(None, 200);
        assert_eq!(chat.trades, 2);
        assert_eq!(chat.net, 20.0 - 15.0);
        assert_eq!((chat.wins, chat.losses), (1, 1));
    }

    /// A torn final write must cost one record, not the history.
    #[test]
    fn a_corrupt_line_is_skipped_not_fatal() {
        let d = tmpdir("corrupt");
        {
            let l = ledger(&d);
            l.record("A", "A", "b", 1.0, 10.0, 20.0, 0.0, 0, 100, Source::Chat);
        }
        let p = d.join("l.jsonl");
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, "{{\"ts\":123,\"tag\":\"tr").unwrap();
        drop(f);
        let l2 = ledger(&d);
        let (chat, _, _) = l2.summarise(None, 100);
        assert_eq!(chat.trades, 1, "the good record must survive");
    }

    /// Losses have to be visible; a P&L that only counts wins is decoration.
    #[test]
    fn a_loss_is_recorded_as_a_loss() {
        let d = tmpdir("loss");
        let l = ledger(&d);
        l.record(
            "A",
            "A",
            "b",
            100.0,
            50.0,
            40.0,
            0.0125,
            0,
            100,
            Source::Chat,
        );
        let (chat, _, _) = l.summarise(None, 100);
        assert!(chat.net < 0.0, "got {}", chat.net);
        assert_eq!((chat.wins, chat.losses), (0, 1));
        assert!(chat.roi_pct < 0.0);
    }

    /// The time window must actually bind, or "24h" is a lie.
    #[test]
    fn the_window_excludes_older_trades() {
        let d = tmpdir("window");
        let l = ledger(&d);
        let now = 100_000_000i64;
        let day = 86_400_000i64;
        l.record(
            "A",
            "A",
            "b",
            1.0,
            1.0,
            2.0,
            0.0,
            0,
            now - 2 * day,
            Source::Chat,
        );
        l.record(
            "B",
            "B",
            "b",
            1.0,
            1.0,
            2.0,
            0.0,
            0,
            now - 3_600_000,
            Source::Chat,
        );
        assert_eq!(l.summarise(Some(24.0), now).0.trades, 1);
        assert_eq!(l.summarise(None, now).0.trades, 2);
    }
}
