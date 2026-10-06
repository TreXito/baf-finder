//! Kaplan-Meier survival for auction listings — the de-biased half of the
//! volume→TTS migration.
//!
//! `sold.tts_ms` only exists for listings that SOLD, so any statistic built from
//! it alone answers "how fast did the winners win", never "did they win at all".
//! Measured 2026-08-05 over 21 days of tracked listings (3.58M sold, 836k
//! censored, 2,383 items with >=20 listings):
//!
//! | censored share of an item's listings | items |
//! |---|---|
//! | p25 | 17.8% |
//! | median | 29.0% |
//! | p75 | 50.5% |
//! | p90 | 70.9% |
//!
//! and **455 of those 2,383 items show a naive median time-to-sell under 6h
//! while under half their listings ever sell within 24h**. `REDSTONE_ORE` reads
//! 0.22h off 2 sales against 78 listings that never sold at all — a 2%
//! sell-through wearing a 13-minute median.
//!
//! The `censored` table (a listing still unsold after `ttsCensorDays`, with its
//! `lifetime_ms`) is exactly the right-censored half needed to fix this. It was
//! empty when the TTS work shipped in July, which is why `TTS_LIQUIDITY` was left
//! dormant; it now holds 836,396 rows.
//!
//! Kaplan-Meier is the standard estimator for this shape of data: it uses a
//! censored observation for as long as it was observed rather than discarding it
//! (which over-states sell-through) or counting it as a failure at time 0 (which
//! under-states it).

/// Survival S(t) = P(a listing is STILL unsold at `t` hours), Kaplan-Meier.
///
/// `events_h` are hours-to-sale for listings that sold; `censored_h` are
/// hours-observed for listings that had not sold when we stopped watching.
/// Ties are resolved the conventional way: an observation censored at exactly
/// `t` is still at risk for the event at `t`.
///
/// With no censored observations this is just the empirical survival curve, so
/// callers that have no censoring data get the pre-censoring behaviour.
fn survival_at(events_h: &[f64], censored_h: &[f64], t: f64) -> f64 {
    if events_h.is_empty() {
        // Nothing ever sold. Either there is no data at all (unknowable, treat as
        // "still unsold" 0.0 → sell_through 1.0 is decided by the caller) or every
        // listing was censored, which is a true 0% sell-through.
        return 1.0;
    }
    let mut obs: Vec<(f64, bool)> = Vec::with_capacity(events_h.len() + censored_h.len());
    obs.extend(
        events_h
            .iter()
            .filter(|x| x.is_finite() && **x >= 0.0)
            .map(|&x| (x, true)),
    );
    obs.extend(
        censored_h
            .iter()
            .filter(|x| x.is_finite() && **x >= 0.0)
            .map(|&x| (x, false)),
    );
    // Ascending by time; events before censored at the same time so the censored
    // observation counts toward that time's risk set.
    obs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(b.1.cmp(&a.1)));

    let mut at_risk = obs.len() as f64;
    let mut s = 1.0f64;
    let mut i = 0usize;
    while i < obs.len() {
        let time = obs[i].0;
        if time > t {
            break;
        }
        let mut deaths = 0.0f64;
        let mut leaving = 0.0f64;
        while i < obs.len() && obs[i].0 == time {
            if obs[i].1 {
                deaths += 1.0;
            }
            leaving += 1.0;
            i += 1;
        }
        if deaths > 0.0 && at_risk > 0.0 {
            s *= 1.0 - deaths / at_risk;
        }
        at_risk -= leaving;
    }
    s
}

/// P(a listing sells within `horizon_h` hours) = 1 − S(horizon).
///
/// Returns `None` when there is nothing to estimate from (no observations at
/// all), so callers can distinguish "illiquid" from "unknown" — they are very
/// different answers and collapsing them is how a cold-start item gets
/// mistaken for a dead one.
pub fn sell_through(events_h: &[f64], censored_h: &[f64], horizon_h: f64) -> Option<f64> {
    if events_h.is_empty() && censored_h.is_empty() {
        return None;
    }
    if events_h.is_empty() {
        return Some(0.0); // every tracked listing was censored: nothing sold
    }
    Some(1.0 - survival_at(events_h, censored_h, horizon_h))
}

/// Kaplan-Meier median hours-to-sell: the first time S(t) drops to 0.5 or below.
///
/// `None` when survival never reaches 0.5 — i.e. most listings never sell, so the
/// median is beyond the observation window and NOT a number to compare against a
/// tier's `max_time_to_sell`. The naive median always returns something here,
/// which is precisely the bug.
pub fn km_median(events_h: &[f64], censored_h: &[f64]) -> Option<f64> {
    if events_h.is_empty() {
        return None;
    }
    let mut times: Vec<f64> = events_h
        .iter()
        .copied()
        .filter(|x| x.is_finite() && *x >= 0.0)
        .collect();
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    times.dedup();
    times
        .into_iter()
        .find(|&t| survival_at(events_h, censored_h, t) <= 0.5)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn no_censoring_reduces_to_the_empirical_curve() {
        let ev = [1.0, 2.0, 3.0, 4.0];
        // 2 of 4 sold by t=2 ⇒ 50%.
        assert!(close(sell_through(&ev, &[], 2.0).unwrap(), 0.5));
        assert!(close(sell_through(&ev, &[], 4.0).unwrap(), 1.0));
        assert!(close(sell_through(&ev, &[], 0.5).unwrap(), 0.0));
        assert_eq!(km_median(&ev, &[]), Some(2.0));
    }

    #[test]
    fn hand_computed_kaplan_meier() {
        // events at 2 and 6; censored at 4. n=3.
        //   t=2: at_risk 3, d=1 ⇒ S = 2/3
        //   t=4: censored, S unchanged, at_risk → 1
        //   t=6: at_risk 1, d=1 ⇒ S = 0
        let ev = [2.0, 6.0];
        let cen = [4.0];
        assert!(close(survival_at(&ev, &cen, 3.0), 2.0 / 3.0));
        assert!(close(survival_at(&ev, &cen, 5.0), 2.0 / 3.0));
        assert!(close(survival_at(&ev, &cen, 6.0), 0.0));
        assert!(close(sell_through(&ev, &cen, 3.0).unwrap(), 1.0 / 3.0));
    }

    #[test]
    fn censoring_is_not_discarded_the_way_the_naive_median_discards_it() {
        // The REDSTONE_ORE shape: 2 fast sales, 78 listings that never sold.
        let ev = [0.2, 0.25];
        let cen: Vec<f64> = vec![96.0; 78];
        // Naive: median 0.225h and "looks instant".
        // Honest: only 2 of 80 listings ever sold ⇒ ~2.5% sell-through.
        let st = sell_through(&ev, &cen, 24.0).unwrap();
        assert!(st > 0.02 && st < 0.03, "sell_through was {st}");
        // and the KM median does not exist: survival never falls to 0.5.
        assert_eq!(km_median(&ev, &cen), None);
    }

    #[test]
    fn a_censored_observation_at_the_horizon_still_counts_as_at_risk() {
        // One sale at 24h, one listing censored at exactly 24h.
        let ev = [24.0];
        let cen = [24.0];
        // At t=24 both are at risk, one dies ⇒ S = 0.5, sell_through = 0.5.
        assert!(close(sell_through(&ev, &cen, 24.0).unwrap(), 0.5));
    }

    #[test]
    fn everything_censored_is_zero_sell_through_not_unknown() {
        assert_eq!(sell_through(&[], &[96.0, 96.0], 24.0), Some(0.0));
    }

    #[test]
    fn no_observations_at_all_is_unknown_not_illiquid() {
        assert_eq!(sell_through(&[], &[], 24.0), None);
        assert_eq!(km_median(&[], &[]), None);
    }

    #[test]
    fn garbage_times_are_ignored_rather_than_poisoning_the_curve() {
        let ev = [1.0, f64::NAN, -5.0, 3.0];
        let st = sell_through(&ev, &[], 3.0).unwrap();
        assert!(close(st, 1.0), "st was {st}");
    }
}
