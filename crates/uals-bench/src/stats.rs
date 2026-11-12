//! Paired bootstrap for comparing two runs on the same queries.

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use serde::{Deserialize, Serialize};

use crate::metrics::mean;

pub const RESAMPLES: usize = 10_000;
/// Fixed seed: the same inputs always give the same interval.
pub const SEED: u64 = 0x5EED_0FA1;

/// Result of a paired bootstrap of `mean(b) - mean(a)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bootstrap {
    pub n: usize,
    pub mean_a: f64,
    pub mean_b: f64,
    /// `mean_b - mean_a`.
    pub diff: f64,
    /// 2.5th percentile of the resampled differences.
    pub ci_low: f64,
    /// 97.5th percentile of the resampled differences.
    pub ci_high: f64,
    /// True when the 95% interval does not contain 0.
    pub significant: bool,
}

/// Resamples the per-query differences `b[i] - a[i]` with replacement `resamples` times and
/// takes the 95% percentile interval of the resampled means. `None` when there are no pairs.
/// Panics if the slices differ in length (callers pair values by query id first).
pub fn paired_bootstrap(a: &[f64], b: &[f64], resamples: usize, seed: u64) -> Option<Bootstrap> {
    assert_eq!(a.len(), b.len(), "paired samples must have equal length");
    let n = a.len();
    if n == 0 || resamples == 0 {
        return None;
    }
    let diffs: Vec<f64> = a.iter().zip(b).map(|(x, y)| y - x).collect();
    let mut rng = StdRng::seed_from_u64(seed);
    let mut means: Vec<f64> = (0..resamples)
        .map(|_| {
            let sum: f64 = (0..n).map(|_| diffs[rng.random_range(0..n)]).sum();
            sum / n as f64
        })
        .collect();
    means.sort_by(f64::total_cmp);
    let low = means[(0.025 * resamples as f64).floor() as usize];
    let high = means[((0.975 * resamples as f64).ceil() as usize).min(resamples) - 1];
    let (mean_a, mean_b) = (mean(a), mean(b));
    Some(Bootstrap {
        n,
        mean_a,
        mean_b,
        diff: mean_b - mean_a,
        ci_low: low,
        ci_high: high,
        significant: low > 0.0 || high < 0.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_inputs_same_interval() {
        let a: Vec<f64> = (0..40).map(|i| f64::from(i % 5) / 4.0).collect();
        let b: Vec<f64> = (0..40).map(|i| f64::from((i * 3) % 7) / 6.0).collect();
        let first = paired_bootstrap(&a, &b, RESAMPLES, SEED).unwrap();
        let second = paired_bootstrap(&a, &b, RESAMPLES, SEED).unwrap();
        assert_eq!(first, second);
        let other_seed = paired_bootstrap(&a, &b, RESAMPLES, SEED + 1).unwrap();
        assert_ne!((first.ci_low, first.ci_high), (other_seed.ci_low, other_seed.ci_high));
        assert_eq!(first.diff, other_seed.diff, "the point estimate does not depend on the seed");
    }

    #[test]
    fn known_constant_difference_has_a_zero_width_interval() {
        // b is always a + 0.25: every resample has mean exactly 0.25
        let a = [0.0, 0.5, 1.0, 0.2];
        let b: Vec<f64> = a.iter().map(|x| x + 0.25).collect();
        let r = paired_bootstrap(&a, &b, RESAMPLES, SEED).unwrap();
        assert!((r.diff - 0.25).abs() < 1e-12);
        assert!((r.ci_low - 0.25).abs() < 1e-9 && (r.ci_high - 0.25).abs() < 1e-9);
        assert!(r.significant);
    }

    #[test]
    fn clear_improvement_is_significant_and_noise_is_not() {
        // 50 queries; b is better on 30 of them by 1.0 and equal elsewhere
        let a = vec![0.0; 50];
        let b: Vec<f64> = (0..50).map(|i| if i < 30 { 1.0 } else { 0.0 }).collect();
        let r = paired_bootstrap(&a, &b, RESAMPLES, SEED).unwrap();
        assert!((r.diff - 0.6).abs() < 1e-12);
        assert!(r.ci_low > 0.4 && r.ci_high < 0.8, "{r:?}");
        assert!(r.significant);

        // symmetric differences: +1 on 5 queries, -1 on 5 queries, mean 0
        let a: Vec<f64> = (0..10).map(|i| if i < 5 { 0.0 } else { 1.0 }).collect();
        let b: Vec<f64> = (0..10).map(|i| if i < 5 { 1.0 } else { 0.0 }).collect();
        let r = paired_bootstrap(&a, &b, RESAMPLES, SEED).unwrap();
        assert_eq!(r.diff, 0.0);
        assert!(r.ci_low < 0.0 && r.ci_high > 0.0, "{r:?}");
        assert!(!r.significant);
    }

    #[test]
    fn no_pairs_gives_none() {
        assert_eq!(paired_bootstrap(&[], &[], RESAMPLES, SEED), None);
    }
}
