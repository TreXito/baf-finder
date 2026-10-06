//! Port of the numeric helpers in priceIndex.ts. NaN/±Infinity on empty inputs
//! is DELIBERATE (Deviation D1) — the Rust must produce the same, not coerce.

/// `median(sorted)` — caller passes an ascending-sorted slice.
pub fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return f64::NAN;
    }
    let mid = n >> 1;
    if n % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    }
}

/// `percentile(sorted, p)` — nearest-rank via `floor(p*(len-1))`.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((p * (sorted.len() as f64 - 1.0)).floor())
        .max(0.0)
        .min(sorted.len() as f64 - 1.0) as usize;
    sorted[idx]
}

/// Hoare quickselect (in place): after return, `xs[k]` is the k-th smallest.
pub fn quickselect(xs: &mut [f64], k: usize) -> f64 {
    let mut lo = 0usize;
    let mut hi = xs.len() - 1;
    while lo < hi {
        let mid = (lo + hi) >> 1;
        let mut pivot = xs[mid];
        if xs[lo] > xs[hi] {
            xs.swap(lo, hi);
        }
        if xs[lo] > pivot {
            pivot = xs[lo];
        } else if xs[hi] < pivot {
            pivot = xs[hi];
        }
        let mut i = lo;
        let mut j = hi;
        while i <= j {
            while xs[i] < pivot {
                i += 1;
            }
            while xs[j] > pivot {
                j -= 1;
            }
            if i <= j {
                xs.swap(i, j);
                i += 1;
                // j can be 0 here; guard the usize underflow (TS uses signed).
                if j == 0 {
                    break;
                }
                j -= 1;
            }
        }
        if k <= j {
            hi = j;
        } else if k >= i {
            lo = i;
        } else {
            break;
        }
    }
    xs[k]
}

/// `medianOf(xs)` — median without a full sort (mutates xs). Same value as
/// `median` of the sorted input.
pub fn median_of(xs: &mut [f64]) -> f64 {
    let n = xs.len();
    if n == 0 {
        return f64::NAN;
    }
    let mid = n >> 1;
    let upper = quickselect(xs, mid);
    if n % 2 == 1 {
        return upper;
    }
    let mut lower = f64::NEG_INFINITY;
    for &x in xs.iter().take(mid) {
        if x > lower {
            lower = x;
        }
    }
    (lower + upper) / 2.0
}

/// `minMax(xs)` — empty → { min: +Inf, max: -Inf } (Deviation D1).
pub fn min_max(xs: &[f64]) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for &x in xs {
        if x < min {
            min = x;
        }
        if x > max {
            max = x;
        }
    }
    (min, max)
}

pub fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        f64::NAN
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

pub fn stddev(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    let var: Vec<f64> = xs.iter().map(|x| (x - m).powi(2)).collect();
    mean(&var).sqrt()
}

/// Multiset subset: every element of `need` (with multiplicity) appears in `have`.
pub fn subset_multiset(need: &[String], have: &[String]) -> bool {
    if need.is_empty() {
        return true;
    }
    use std::collections::HashMap;
    let mut counts: HashMap<&str, i64> = HashMap::new();
    for h in have {
        *counts.entry(h.as_str()).or_insert(0) += 1;
    }
    for n in need {
        let c = counts.entry(n.as_str()).or_insert(0);
        if *c <= 0 {
            return false;
        }
        *c -= 1;
    }
    true
}
