//! Searches an integer CQ/CRF domain for the value that reaches a target
//! quality score.
//!
//! The evaluator is assumed to be non-increasing: raising `cq` never raises
//! the score. The search keeps a bracket between the highest acceptable value
//! and the lowest unacceptable value, and uses Brent-style inverse quadratic
//! interpolation with secant and bisection fallbacks to shrink it. Callers
//! using a lower-is-better distance must negate both their scores and target;
//! tolerance remains in the original distance units.

use std::collections::HashMap;
use std::future::Future;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub cq: u32,
    pub score: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct SearchConfig {
    pub min: u32,
    pub max: u32,
    /// Target score in the same units as the evaluator's output.
    pub target: f64,
    /// Stop as soon as an acceptable sample is this close to the target.
    /// Set to 0.0 to always resolve the bracket fully.
    pub tolerance: f64,
    /// Number of evaluations the search itself may perform, including the
    /// midpoint probe used when `initial` is empty. Samples supplied in
    /// `initial` do not count against this budget.
    pub max_new_evaluations: usize,
}

#[derive(Debug)]
pub struct SearchOutcome {
    /// Highest CQ that met the target (lowest bitrate that still qualifies).
    pub cq: u32,
    /// Every sample seen by the search, in evaluation order.
    pub samples: Vec<Sample>,
    /// True when the bracket was resolved, an acceptable sample was within
    /// `tolerance`, the target was unreachable, or the target was met at `max`.
    pub converged: bool,
}

/// Finds the highest integer `cq` in `[min, max]` whose score is at least
/// `target`.
///
/// `initial` may contain already-evaluated samples (typically two probes run
/// in parallel by the caller); `evaluate` is called only for new probes.
pub async fn search<F, Fut, R>(
    config: &SearchConfig,
    initial: &[Sample],
    evaluate: F,
    mut report: R,
) -> SearchOutcome
where
    F: Fn(u32) -> Fut,
    Fut: Future<Output = f64>,
    R: FnMut(&Sample),
{
    assert!(config.min <= config.max, "search range is empty");
    assert!(config.target.is_finite(), "target must be finite");
    assert!(
        config.tolerance.is_finite() && config.tolerance >= 0.0,
        "tolerance must be finite and nonnegative"
    );

    let target = config.target;
    let mut cache: HashMap<u32, f64> = HashMap::new();
    let mut samples: Vec<Sample> = Vec::new();

    for sample in initial {
        if sample.cq < config.min || sample.cq > config.max {
            continue;
        }
        if cache.contains_key(&sample.cq) {
            continue;
        }
        assert!(sample.score.is_finite(), "sample score must be finite");
        cache.insert(sample.cq, sample.score);
        samples.push(*sample);
        report(sample);
    }

    let mut budget = config.max_new_evaluations;
    if samples.is_empty() {
        if budget == 0 {
            return fallback(config, samples);
        }
        budget -= 1;
        let midpoint = config.min + (config.max - config.min) / 2;
        eval_once(midpoint, &evaluate, &mut cache, &mut samples, &mut report).await;
    }

    // An initial probe may already be close enough to the target.
    if config.tolerance > 0.0 {
        if let Some(sample) = best_acceptable(&samples, target)
            .filter(|sample| sample.score - target <= config.tolerance)
        {
            return SearchOutcome {
                cq: sample.cq,
                samples,
                converged: true,
            };
        }
    }

    let (mut acceptable, mut unacceptable) = bracket(&samples, target);

    // If every probe is on one side of the target, check the corresponding
    // edge of the domain. A failed check at that edge proves the target is
    // either unreachable or reachable everywhere.
    if acceptable.is_none() {
        let boundary = config.min;
        if !cache.contains_key(&boundary) {
            if budget == 0 {
                return fallback(config, samples);
            }
            budget -= 1;
            eval_once(boundary, &evaluate, &mut cache, &mut samples, &mut report).await;
            (acceptable, unacceptable) = bracket(&samples, target);
        }
        if acceptable.is_none() {
            // Even the best quality in range misses the target.
            return SearchOutcome {
                cq: config.min,
                samples,
                converged: true,
            };
        }
    } else if unacceptable.is_none() {
        let boundary = config.max;
        if !cache.contains_key(&boundary) {
            if budget == 0 {
                return fallback(config, samples);
            }
            budget -= 1;
            eval_once(boundary, &evaluate, &mut cache, &mut samples, &mut report).await;
            (acceptable, unacceptable) = bracket(&samples, target);
        }
        if unacceptable.is_none() {
            // Every quality in range meets the target.
            return SearchOutcome {
                cq: config.max,
                samples,
                converged: true,
            };
        }
    }

    let (mut lo, mut hi) = match (acceptable, unacceptable) {
        (Some(lo), Some(hi)) if lo.cq < hi.cq => (lo, hi),
        // The curve is not monotone, so a bracket cannot be trusted.
        _ => return fallback(config, samples),
    };

    if hi.cq - lo.cq <= 1 {
        return SearchOutcome {
            cq: lo.cq,
            samples,
            converged: true,
        };
    }

    if config.tolerance > 0.0 && lo.score - target <= config.tolerance {
        return SearchOutcome {
            cq: lo.cq,
            samples,
            converged: true,
        };
    }

    // Numerical Recipes' Brent state. The values are CQs (integers stored as
    // f64) and the probe is rounded to an unevaluated integer before use.
    let mut a = lo.cq as f64;
    let mut b = hi.cq as f64;
    let mut c = hi.cq as f64;
    let mut fa = lo.score - target;
    let mut fb = hi.score - target;
    let mut fc = fb;
    let mut d = b - a;
    let mut e = d;

    loop {
        if (fb > 0.0 && fc > 0.0) || (fb < 0.0 && fc < 0.0) {
            c = a;
            fc = fa;
            e = b - a;
            d = e;
        }
        if fc.abs() < fb.abs() {
            a = b;
            b = c;
            c = a;
            fa = fb;
            fb = fc;
            fc = fa;
        }

        let tol1 = 0.5_f64;
        let xm = 0.5 * (c - b);
        if xm.abs() <= tol1 {
            break;
        }

        if e.abs() >= tol1 && fa.abs() > fb.abs() {
            let s = fb / fa;
            let (mut p, mut q);
            if a == c {
                p = 2.0 * xm * s;
                q = 1.0 - s;
            } else {
                let q0 = fa / fc;
                let r = fb / fc;
                p = s * (2.0 * xm * q0 * (q0 - r) - (b - a) * (r - 1.0));
                q = (q0 - 1.0) * (r - 1.0) * (s - 1.0);
            }
            if p > 0.0 {
                q = -q;
            } else {
                p = -p;
            }
            let min1 = 3.0 * xm * q - (tol1 * q).abs();
            let min2 = (e * q).abs();
            if 2.0 * p < min1.min(min2) {
                e = d;
                d = p / q;
            } else {
                d = xm;
                e = d;
            }
        } else {
            d = xm;
            e = d;
        }

        a = b;
        fa = fb;

        let low = lo.cq;
        let high = hi.cq;
        if high - low <= 1 {
            break;
        }
        if budget == 0 {
            return fallback(config, samples);
        }

        let old_b = b;
        let mut candidate = b + d;
        if !candidate.is_finite() {
            candidate = 0.5 * (low as f64 + high as f64);
        }
        let min_probe = low as f64 + 1.0;
        let max_probe = high as f64 - 1.0;
        let next = candidate.round().clamp(min_probe, max_probe);
        let mut next_cq = next as u32;

        if cache.contains_key(&next_cq) {
            next_cq = low + (high - low) / 2;
        }
        if cache.contains_key(&next_cq) || next_cq <= low || next_cq >= high {
            // No unevaluated probe remains inside the bracket.
            break;
        }

        budget -= 1;
        let score = eval_once(next_cq, &evaluate, &mut cache, &mut samples, &mut report).await;

        b = next_cq as f64;
        fb = score - target;
        // Keep Brent's step history consistent with the integer probe we
        // actually made (rounding/clamping may have changed the step).
        d = b - old_b;

        match bracket(&samples, target) {
            (Some(new_lo), Some(new_hi)) if new_lo.cq < new_hi.cq => {
                lo = new_lo;
                hi = new_hi;
            }
            _ => break,
        }

        if score >= target && config.tolerance > 0.0 && score - target <= config.tolerance {
            return SearchOutcome {
                cq: lo.cq,
                samples,
                converged: true,
            };
        }
    }

    if hi.cq - lo.cq <= 1 {
        SearchOutcome {
            cq: lo.cq,
            samples,
            converged: true,
        }
    } else {
        fallback(config, samples)
    }
}

async fn eval_once<F, Fut, R>(
    cq: u32,
    evaluate: &F,
    cache: &mut HashMap<u32, f64>,
    samples: &mut Vec<Sample>,
    report: &mut R,
) -> f64
where
    F: Fn(u32) -> Fut,
    Fut: Future<Output = f64>,
    R: FnMut(&Sample),
{
    if let Some(score) = cache.get(&cq) {
        return *score;
    }

    let score = evaluate(cq).await;
    assert!(score.is_finite(), "evaluator score must be finite");
    cache.insert(cq, score);
    let sample = Sample { cq, score };
    samples.push(sample);
    report(&sample);
    score
}

/// Returns the highest acceptable sample and the lowest unacceptable sample.
fn bracket(samples: &[Sample], target: f64) -> (Option<Sample>, Option<Sample>) {
    let mut acceptable: Option<Sample> = None;
    let mut unacceptable: Option<Sample> = None;

    for &sample in samples {
        if sample.score >= target {
            match acceptable {
                Some(best) if sample.cq <= best.cq => {}
                _ => acceptable = Some(sample),
            }
        } else {
            match unacceptable {
                Some(best) if sample.cq >= best.cq => {}
                _ => unacceptable = Some(sample),
            }
        }
    }

    (acceptable, unacceptable)
}

fn best_acceptable(samples: &[Sample], target: f64) -> Option<Sample> {
    samples
        .iter()
        .copied()
        .filter(|sample| sample.score >= target)
        .max_by_key(|sample| sample.cq)
}

fn fallback(config: &SearchConfig, samples: Vec<Sample>) -> SearchOutcome {
    let cq = best_acceptable(&samples, config.target)
        .map(|sample| sample.cq)
        .unwrap_or(config.min);

    SearchOutcome {
        cq,
        samples,
        converged: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    #[should_panic(expected = "evaluator score must be finite")]
    async fn rejects_nonfinite_evaluation() {
        search(&config(10, 50, 90.0), &[], |_| async { f64::NAN }, |_| {}).await;
    }

    #[tokio::test]
    #[should_panic(expected = "sample score must be finite")]
    async fn rejects_nonfinite_seed() {
        search(
            &config(10, 50, 90.0),
            &[Sample {
                cq: 20,
                score: f64::INFINITY,
            }],
            |_| async { 95.0 },
            |_| {},
        )
        .await;
    }

    #[tokio::test]
    #[should_panic(expected = "tolerance must be finite and nonnegative")]
    async fn rejects_negative_tolerance() {
        let mut settings = config(10, 50, 90.0);
        settings.tolerance = -0.5;
        search(&settings, &[], |_| async { 95.0 }, |_| {}).await;
    }

    fn config(min: u32, max: u32, target: f64) -> SearchConfig {
        SearchConfig {
            min,
            max,
            target,
            tolerance: 0.0,
            max_new_evaluations: 10,
        }
    }

    fn brute_force(min: u32, max: u32, target: f64, score: impl Fn(u32) -> f64) -> u32 {
        (min..=max)
            .filter(|cq| score(*cq) >= target)
            .max()
            .unwrap_or(min)
    }

    async fn run(
        config: &SearchConfig,
        initial: &[Sample],
        score: impl Fn(u32) -> f64,
    ) -> (SearchOutcome, usize) {
        let calls = Arc::new(AtomicUsize::new(0));
        let score = Arc::new(score);
        let outcome = search(
            config,
            initial,
            {
                let calls = calls.clone();
                let score = score.clone();
                move |cq| {
                    let calls = calls.clone();
                    let score = score.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        score(cq)
                    }
                }
            },
            |_| {},
        )
        .await;
        (outcome, calls.load(Ordering::SeqCst))
    }

    fn linear(cq: u32) -> f64 {
        100.0 - (cq as f64 - 10.0) * 20.0 / 50.0
    }

    #[tokio::test]
    async fn finds_largest_acceptable_cq_on_a_linear_curve() {
        let cfg = config(10, 60, 90.0);
        let expected = brute_force(cfg.min, cfg.max, cfg.target, linear);
        let (outcome, calls) = run(&cfg, &[], linear).await;

        assert_eq!(outcome.cq, expected);
        assert!(outcome.converged);
        assert_eq!(calls, outcome.samples.len());
        assert!(calls <= 8, "used {calls} evaluations");
    }

    #[tokio::test]
    async fn works_with_seeds_on_the_same_side() {
        let cfg = config(10, 60, 90.0);
        let expected = brute_force(cfg.min, cfg.max, cfg.target, linear);
        let initial = [
            Sample {
                cq: 10,
                score: linear(10),
            },
            Sample {
                cq: 15,
                score: linear(15),
            },
        ];
        let (outcome, _) = run(&cfg, &initial, linear).await;

        assert_eq!(outcome.cq, expected);
        assert!(outcome.converged);
    }

    #[tokio::test]
    async fn handles_plateaus_without_dividing_by_zero() {
        let score = |cq: u32| {
            if cq <= 30 {
                100.0
            } else {
                100.0 - (cq as f64 - 30.0) * 2.0
            }
        };
        let cfg = config(10, 60, 95.0);
        let expected = brute_force(cfg.min, cfg.max, cfg.target, score);
        let (outcome, _) = run(&cfg, &[], score).await;

        assert_eq!(outcome.cq, expected);
        assert!(outcome.converged);
    }

    #[tokio::test]
    async fn unreachable_target_returns_min() {
        let score = |_cq: u32| 80.0;
        let cfg = config(10, 60, 95.0);
        let (outcome, _) = run(&cfg, &[], score).await;

        assert_eq!(outcome.cq, 10);
        assert!(outcome.converged);
    }

    #[tokio::test]
    async fn target_met_everywhere_returns_max() {
        let score = |_cq: u32| 99.0;
        let cfg = config(10, 60, 95.0);
        let (outcome, _) = run(&cfg, &[], score).await;

        assert_eq!(outcome.cq, 60);
        assert!(outcome.converged);
    }

    #[tokio::test]
    async fn never_evaluates_the_same_cq_twice() {
        let cfg = config(10, 60, 90.0);
        let (outcome, calls) = run(&cfg, &[], linear).await;

        let mut cqs: Vec<u32> = outcome.samples.iter().map(|sample| sample.cq).collect();
        cqs.sort_unstable();
        cqs.dedup();
        assert_eq!(cqs.len(), outcome.samples.len());
        assert_eq!(calls, outcome.samples.len());
    }

    #[tokio::test]
    async fn min_equal_to_max_is_supported() {
        let cfg = config(30, 30, 95.0);
        let initial = [Sample {
            cq: 30,
            score: 96.0,
        }];
        let (outcome, _) = run(&cfg, &initial, |_| 96.0).await;

        assert_eq!(outcome.cq, 30);
        assert!(outcome.converged);
    }

    #[tokio::test]
    async fn out_of_range_and_duplicate_seeds_are_ignored() {
        let cfg = config(10, 60, 90.0);
        let expected = brute_force(cfg.min, cfg.max, cfg.target, linear);
        let initial = [
            Sample {
                cq: 0,
                score: 100.0,
            },
            Sample {
                cq: 20,
                score: linear(20),
            },
            Sample { cq: 20, score: 0.0 },
            Sample { cq: 70, score: 0.0 },
        ];
        let (outcome, _) = run(&cfg, &initial, linear).await;

        assert_eq!(outcome.cq, expected);
        assert!(outcome
            .samples
            .iter()
            .all(|sample| (cfg.min..=cfg.max).contains(&sample.cq)));
    }

    #[tokio::test]
    async fn tolerance_can_stop_early() {
        let cfg = SearchConfig {
            min: 10,
            max: 60,
            target: 90.0,
            tolerance: 0.01,
            max_new_evaluations: 10,
        };
        let score = |cq: u32| 90.006 - (cq as f64 - 40.0) * 0.1;
        let initial = [Sample {
            cq: 40,
            score: 90.006,
        }];
        let (outcome, calls) = run(&cfg, &initial, score).await;

        assert_eq!(outcome.cq, 40);
        assert!(outcome.converged);
        assert_eq!(calls, 0);
    }

    #[tokio::test]
    async fn empty_initial_respects_the_evaluation_budget() {
        let cfg = SearchConfig {
            min: 10,
            max: 60,
            target: 90.0,
            tolerance: 0.0,
            max_new_evaluations: 0,
        };
        let (outcome, calls) = run(&cfg, &[], linear).await;

        assert_eq!(calls, 0);
        assert_eq!(outcome.cq, cfg.min);
        assert!(!outcome.converged);
    }

    #[tokio::test]
    async fn reports_unique_in_range_seeds_and_new_probes_in_order() {
        let cfg = config(10, 20, 85.0);
        let initial = [
            Sample {
                cq: 9,
                score: f64::NAN,
            },
            Sample {
                cq: 12,
                score: 88.0,
            },
            Sample {
                cq: 12,
                score: f64::NAN,
            },
            Sample {
                cq: 20,
                score: 80.0,
            },
        ];
        let mut reported = Vec::new();
        // The evaluator is Fn, so record calls through interior mutability.
        let calls = std::cell::RefCell::new(Vec::new());
        let outcome = search(
            &cfg,
            &initial,
            |cq| {
                calls.borrow_mut().push(cq);
                async move { 100.0 - cq as f64 }
            },
            |sample| reported.push(*sample),
        )
        .await;
        let evaluated = calls.into_inner();

        assert_eq!(outcome.cq, 15);
        assert!(outcome.converged);
        assert_eq!(reported, outcome.samples);
        assert_eq!(&reported[..2], &[initial[1], initial[3]]);
        assert_eq!(
            evaluated,
            reported[2..]
                .iter()
                .map(|sample| sample.cq)
                .collect::<Vec<_>>()
        );
        let mut unique = evaluated.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), evaluated.len());
        assert!(evaluated.iter().all(|cq| *cq != 12 && *cq != 20));
    }

    #[tokio::test]
    async fn seeded_search_budget_returns_best_known_acceptable_without_extra_calls() {
        let initial = [
            Sample {
                cq: 10,
                score: 100.0,
            },
            Sample {
                cq: 20,
                score: 90.0,
            },
            Sample {
                cq: 40,
                score: 70.0,
            },
        ];
        let mut cfg = config(10, 50, 85.0);
        cfg.max_new_evaluations = 0;
        let (outcome, calls) = run(&cfg, &initial, |cq| 110.0 - cq as f64).await;
        assert_eq!(calls, 0);
        assert_eq!(outcome.cq, 20);
        assert_eq!(outcome.samples, initial);
        assert!(!outcome.converged);

        cfg.max_new_evaluations = 1;
        let (outcome, calls) = run(&cfg, &initial, |cq| 110.0 - cq as f64).await;
        assert_eq!(calls, 1);
        assert_eq!(outcome.samples.len(), initial.len() + 1);
        assert!(outcome
            .samples
            .iter()
            .any(|sample| sample.cq == outcome.cq && sample.score >= cfg.target));
    }

    #[tokio::test]
    async fn boundary_probes_consume_budget_and_prove_edge_cases() {
        for (score, boundary, expected) in [(80.0, 10, 10), (100.0, 50, 50)] {
            let initial = [Sample { cq: 30, score }];
            let mut cfg = config(10, 50, 90.0);
            cfg.max_new_evaluations = 0;
            let (limited, calls) = run(&cfg, &initial, |_| score).await;
            assert_eq!(calls, 0);
            assert!(!limited.converged);

            cfg.max_new_evaluations = 1;
            let (outcome, calls) = run(&cfg, &initial, |_| score).await;
            assert_eq!(calls, 1);
            assert_eq!(
                outcome.samples[1],
                Sample {
                    cq: boundary,
                    score
                }
            );
            assert_eq!(outcome.cq, expected);
            assert!(outcome.converged);
        }
    }

    #[tokio::test]
    async fn zero_tolerance_resolves_exact_target_plateau() {
        let cfg = config(0, 20, 90.0);
        let score = |cq| {
            if cq < 5 {
                95.0
            } else if cq <= 15 {
                90.0
            } else {
                85.0
            }
        };
        let initial = [
            Sample { cq: 5, score: 90.0 },
            Sample {
                cq: 20,
                score: 85.0,
            },
        ];
        let (outcome, _) = run(&cfg, &initial, score).await;
        assert_eq!(outcome.cq, 15);
        assert!(outcome.converged);
    }

    #[tokio::test]
    async fn tolerance_does_not_accept_scores_below_target() {
        let mut cfg = config(10, 20, 90.0);
        cfg.tolerance = 0.5;
        let initial = [Sample {
            cq: 15,
            score: 89.9,
        }];
        let (outcome, calls) = run(&cfg, &initial, |_| 89.9).await;
        assert_eq!(calls, 1);
        assert_eq!(outcome.cq, 10);
        assert!(outcome.converged);
        assert_eq!(outcome.samples[1].cq, 10);
    }

    #[tokio::test]
    async fn non_monotone_seed_bracket_falls_back_to_best_acceptable() {
        let cfg = config(10, 50, 90.0);
        let initial = [
            Sample {
                cq: 20,
                score: 80.0,
            },
            Sample {
                cq: 40,
                score: 95.0,
            },
        ];
        let (outcome, calls) = run(&cfg, &initial, |_| 90.0).await;
        assert_eq!(calls, 0);
        assert_eq!(outcome.cq, 40);
        assert!(!outcome.converged);
    }

    #[tokio::test]
    async fn supports_ranges_near_u32_max() {
        let cfg = config(u32::MAX - 20, u32::MAX, 0.0);
        let threshold = u32::MAX - 7;
        let (outcome, _) = run(&cfg, &[], |cq| (threshold as i64 - cq as i64) as f64).await;
        assert_eq!(outcome.cq, threshold);
        assert!(outcome.converged);
    }

    #[tokio::test]
    #[should_panic(expected = "search range is empty")]
    async fn rejects_reversed_domain() {
        search(&config(20, 10, 90.0), &[], |_| async { 90.0 }, |_| {}).await;
    }

    #[tokio::test]
    #[should_panic(expected = "target must be finite")]
    async fn rejects_nonfinite_target() {
        search(
            &config(10, 20, f64::INFINITY),
            &[],
            |_| async { 90.0 },
            |_| {},
        )
        .await;
    }

    #[tokio::test]
    #[should_panic(expected = "tolerance must be finite and nonnegative")]
    async fn rejects_nonfinite_tolerance() {
        let mut cfg = config(10, 20, 90.0);
        cfg.tolerance = f64::NAN;
        search(&cfg, &[], |_| async { 90.0 }, |_| {}).await;
    }

    #[tokio::test]
    async fn agrees_with_brute_force_across_monotone_curves() {
        let cfg = config(10, 60, 0.0);

        for target in [82.5_f64, 90.0, 95.5, 99.0] {
            for scale in [0.25_f64, 0.5, 1.0, 2.0] {
                let score = move |cq: u32| 100.0 - (cq as f64 - 10.0) * scale;
                let mut cfg = cfg;
                cfg.target = target;

                let expected = brute_force(cfg.min, cfg.max, cfg.target, score);
                let (outcome, calls) = run(&cfg, &[], score).await;

                assert_eq!(
                    outcome.cq, expected,
                    "target={target} scale={scale} used {calls} evaluations"
                );
                assert!(calls <= 10, "target={target} scale={scale} used {calls}");
            }
        }
    }
}
