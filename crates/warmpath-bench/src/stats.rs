//! Latency summaries and across-run confidence intervals.
//!
//! Two separate jobs live here, and conflating them is a common way to publish
//! a wrong number.
//!
//! Within one run, latencies are recorded in an HdrHistogram and summarised by
//! percentile. A percentile from a single run is a point estimate with no error
//! bar: repeating the run will move it.
//!
//! Across runs, each run contributes one observation per metric — its own p99,
//! say — and those observations get a median and a confidence interval. The
//! interval describes the spread of run-level p99s, which is the thing a reader
//! actually wants to know when asking whether two configurations differ.

use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};

/// Percentiles reported for every latency metric.
pub const REPORTED_PERCENTILES: [f64; 7] = [50.0, 75.0, 90.0, 95.0, 99.0, 99.9, 99.99];

/// Percentile summary of one latency distribution, in microseconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatencySummary {
    pub count: u64,
    pub min_us: u64,
    pub max_us: u64,
    pub mean_us: f64,
    /// Percentile value pairs, in the order of [`REPORTED_PERCENTILES`].
    pub percentiles: Vec<PercentilePoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PercentilePoint {
    pub percentile: f64,
    pub value_us: u64,
}

impl LatencySummary {
    /// Summarise a histogram. Returns `None` for an empty histogram rather than
    /// reporting zeros, so an empty run cannot be mistaken for a fast one.
    pub fn from_histogram(histogram: &Histogram<u64>) -> Option<Self> {
        if histogram.is_empty() {
            return None;
        }

        Some(Self {
            count: histogram.len(),
            min_us: histogram.min(),
            max_us: histogram.max(),
            mean_us: histogram.mean(),
            percentiles: REPORTED_PERCENTILES
                .iter()
                .map(|&percentile| PercentilePoint {
                    percentile,
                    value_us: histogram.value_at_percentile(percentile),
                })
                .collect(),
        })
    }

    /// Value at one of the reported percentiles.
    pub fn value_at(&self, percentile: f64) -> Option<u64> {
        self.percentiles
            .iter()
            .find(|point| (point.percentile - percentile).abs() < f64::EPSILON)
            .map(|point| point.value_us)
    }
}

/// Build a histogram sized for latencies from one microsecond to ten minutes,
/// with three significant figures of precision.
pub fn new_histogram() -> Histogram<u64> {
    Histogram::new_with_bounds(1, 600_000_000, 3).expect("histogram bounds are valid")
}

/// A metric measured once per run, summarised across runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    /// Number of runs contributing an observation.
    pub runs: usize,
    /// Per-run observations, in run order.
    pub values: Vec<f64>,
    /// Median of the observations. Reported as the point estimate because it
    /// does not move when one run goes badly.
    pub median: f64,
    pub mean: f64,
    /// Sample standard deviation, with Bessel's correction. `None` for a single
    /// run, where there is no spread to estimate.
    pub std_dev: Option<f64>,
    /// Half-width of the two-sided 95% confidence interval on the mean.
    pub ci95_half_width: Option<f64>,
}

impl Aggregate {
    /// Summarise per-run observations.
    ///
    /// The confidence interval is Student's t on the mean of run-level values,
    /// which is appropriate here for a specific reason: each run is one
    /// independent draw, and the quantity being averaged is a run summary
    /// rather than an individual latency. Applying a normal approximation to
    /// individual latencies would be wrong, because latency distributions are
    /// heavy-tailed and their percentiles are not sample means.
    ///
    /// Returns `None` when given no observations.
    pub fn from_values(values: &[f64]) -> Option<Self> {
        if values.is_empty() {
            return None;
        }

        let runs = values.len();
        let mean = values.iter().sum::<f64>() / runs as f64;

        let (std_dev, ci95_half_width) = if runs < 2 {
            (None, None)
        } else {
            let variance = values
                .iter()
                .map(|value| {
                    let deviation = value - mean;
                    deviation * deviation
                })
                .sum::<f64>()
                / (runs - 1) as f64;
            let std_dev = variance.sqrt();
            let half_width = t_critical_95(runs - 1) * std_dev / (runs as f64).sqrt();
            (Some(std_dev), Some(half_width))
        };

        Some(Self {
            runs,
            values: values.to_vec(),
            median: median(values),
            mean,
            std_dev,
            ci95_half_width,
        })
    }

    /// Inclusive bounds of the 95% confidence interval on the mean.
    pub fn ci95(&self) -> Option<(f64, f64)> {
        self.ci95_half_width
            .map(|half| (self.mean - half, self.mean + half))
    }
}

/// Median of a slice. The input is copied so the caller's order is preserved.
pub fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

/// Two-sided 95% critical value of Student's t for the given degrees of
/// freedom.
///
/// Table lookup rather than a computed inverse CDF: the benchmark runs a
/// handful of repetitions, so only small degrees of freedom matter, and a table
/// is easy to check against any statistics reference. Beyond 30 the
/// distribution is close enough to normal that 1.96 is used.
fn t_critical_95(degrees_of_freedom: usize) -> f64 {
    const TABLE: [f64; 30] = [
        12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160,
        2.145, 2.131, 2.120, 2.110, 2.101, 2.093, 2.086, 2.080, 2.074, 2.069, 2.064, 2.060, 2.056,
        2.052, 2.048, 2.045, 2.042,
    ];

    match degrees_of_freedom {
        0 => f64::INFINITY,
        df if df <= TABLE.len() => TABLE[df - 1],
        _ => 1.96,
    }
}

/// How far a reported percentile moves when latency is measured from the
/// moment a request was actually sent rather than from the moment it was
/// supposed to be sent.
///
/// This is the coordinated-omission gap. A closed-loop generator can only
/// report the `from_dispatch` number, because it has no schedule to be late
/// against. Under overload the two diverge, and the direction is always the
/// same: measuring from dispatch hides the queueing the generator itself
/// suffered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OmissionGap {
    pub percentile: f64,
    pub from_intended_us: u64,
    pub from_dispatch_us: u64,
    /// `from_intended / from_dispatch`. One means no gap.
    pub ratio: f64,
}

/// Compare two summaries of the same run at every reported percentile.
pub fn omission_gaps(
    from_intended: &LatencySummary,
    from_dispatch: &LatencySummary,
) -> Vec<OmissionGap> {
    REPORTED_PERCENTILES
        .iter()
        .filter_map(|&percentile| {
            let intended = from_intended.value_at(percentile)?;
            let dispatch = from_dispatch.value_at(percentile)?;
            Some(OmissionGap {
                percentile,
                from_intended_us: intended,
                from_dispatch_us: dispatch,
                ratio: if dispatch == 0 {
                    f64::NAN
                } else {
                    intended as f64 / dispatch as f64
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn histogram_of(values: &[u64]) -> Histogram<u64> {
        let mut histogram = new_histogram();
        for &value in values {
            histogram.record(value).expect("value should be in range");
        }
        histogram
    }

    #[test]
    fn an_empty_histogram_summarises_to_nothing() {
        assert!(LatencySummary::from_histogram(&new_histogram()).is_none());
    }

    #[test]
    fn percentiles_track_a_known_distribution() {
        // 1..=1000 microseconds, one sample each.
        let values: Vec<u64> = (1..=1000).collect();
        let summary =
            LatencySummary::from_histogram(&histogram_of(&values)).expect("should summarise");

        assert_eq!(summary.count, 1000);
        assert_eq!(summary.min_us, 1);
        assert_eq!(summary.max_us, 1000);
        assert!((summary.mean_us - 500.5).abs() < 1.0, "{}", summary.mean_us);

        // Three significant figures means percentile values land within 0.1% of
        // the true value, so compare with a tolerance rather than exactly.
        let p50 = summary.value_at(50.0).expect("p50 should exist");
        assert!((499..=501).contains(&p50), "p50 was {p50}");
        let p99 = summary.value_at(99.0).expect("p99 should exist");
        assert!((989..=992).contains(&p99), "p99 was {p99}");
    }

    #[test]
    fn median_handles_both_parities() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
        assert_eq!(median(&[7.0]), 7.0);
    }

    #[test]
    fn a_single_run_has_no_interval() {
        let aggregate = Aggregate::from_values(&[10.0]).expect("should aggregate");

        assert_eq!(aggregate.runs, 1);
        assert_eq!(aggregate.mean, 10.0);
        assert_eq!(aggregate.median, 10.0);
        assert!(aggregate.std_dev.is_none());
        assert!(aggregate.ci95_half_width.is_none());
        assert!(aggregate.ci95().is_none());
    }

    #[test]
    fn three_runs_produce_a_hand_checkable_interval() {
        // Mean 20, sample std dev 5, n = 3, t(2) = 4.303.
        // half-width = 4.303 * 5 / sqrt(3) = 12.4218...
        let aggregate = Aggregate::from_values(&[15.0, 20.0, 25.0]).expect("should aggregate");

        assert_eq!(aggregate.mean, 20.0);
        assert_eq!(aggregate.median, 20.0);
        assert!((aggregate.std_dev.expect("std dev") - 5.0).abs() < 1e-9);

        let half = aggregate.ci95_half_width.expect("half width");
        assert!((half - 12.4218).abs() < 1e-3, "half width was {half}");

        let (low, high) = aggregate.ci95().expect("interval");
        assert!((low - 7.5782).abs() < 1e-3, "low was {low}");
        assert!((high - 32.4218).abs() < 1e-3, "high was {high}");
    }

    #[test]
    fn identical_runs_produce_a_zero_width_interval() {
        let aggregate = Aggregate::from_values(&[42.0, 42.0, 42.0]).expect("should aggregate");

        assert_eq!(aggregate.std_dev, Some(0.0));
        assert_eq!(aggregate.ci95_half_width, Some(0.0));
    }

    #[test]
    fn more_runs_of_the_same_spread_narrow_the_interval() {
        let three = Aggregate::from_values(&[10.0, 20.0, 30.0]).expect("should aggregate");
        let nine = Aggregate::from_values(&[10.0, 20.0, 30.0, 10.0, 20.0, 30.0, 10.0, 20.0, 30.0])
            .expect("should aggregate");

        let three_half = three.ci95_half_width.expect("half width");
        let nine_half = nine.ci95_half_width.expect("half width");
        assert!(
            nine_half < three_half,
            "nine runs ({nine_half}) should be tighter than three ({three_half})"
        );
    }

    #[test]
    fn t_table_matches_published_values() {
        assert!((t_critical_95(1) - 12.706).abs() < 1e-9);
        assert!((t_critical_95(2) - 4.303).abs() < 1e-9);
        assert!((t_critical_95(10) - 2.228).abs() < 1e-9);
        assert!((t_critical_95(30) - 2.042).abs() < 1e-9);
        assert!((t_critical_95(120) - 1.96).abs() < 1e-9);
    }

    #[test]
    fn no_gap_when_the_generator_kept_up() {
        let values: Vec<u64> = (1..=100).collect();
        let summary =
            LatencySummary::from_histogram(&histogram_of(&values)).expect("should summarise");

        for gap in omission_gaps(&summary, &summary) {
            assert!(
                (gap.ratio - 1.0).abs() < 1e-9,
                "p{} ratio was {}",
                gap.percentile,
                gap.ratio
            );
        }
    }

    #[test]
    fn a_late_generator_shows_up_as_a_ratio_above_one() {
        // Every request waited an extra 100ms in the generator before it was
        // actually sent. Measuring from dispatch hides all of it.
        let from_dispatch: Vec<u64> = (1..=100).map(|value| value * 1_000).collect();
        let from_intended: Vec<u64> = from_dispatch.iter().map(|value| value + 100_000).collect();

        let gaps = omission_gaps(
            &LatencySummary::from_histogram(&histogram_of(&from_intended))
                .expect("should summarise"),
            &LatencySummary::from_histogram(&histogram_of(&from_dispatch))
                .expect("should summarise"),
        );

        for gap in &gaps {
            assert!(
                gap.ratio > 1.0,
                "p{} should show a gap, ratio was {}",
                gap.percentile,
                gap.ratio
            );
            assert!(gap.from_intended_us > gap.from_dispatch_us);
        }
    }
}
