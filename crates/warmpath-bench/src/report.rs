//! Turning records into a run report, and writing it to disk.
//!
//! A report carries everything needed to judge and reproduce the run: the
//! config, the seed, the git SHA, the counts, the latency summaries, and an
//! explicit verdict on whether the run is publishable.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::record::{RequestRecord, RunConfig};
use crate::stats::{
    new_histogram, omission_gaps, LatencySummary, OmissionGap, REPORTED_PERCENTILES,
};

/// Error rate above which a run is not worth publishing.
const MAX_ERROR_FRACTION: f64 = 0.01;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub run_id: String,
    pub started_at_unix_ms: u64,
    pub git_sha: Option<String>,
    pub wall_clock_secs: f64,
    pub config: RunConfig,
    pub validity: Validity,
    pub counts: Counts,
    pub latency: Latencies,
    /// Coordinated-omission gap on time to first token, per percentile.
    pub omission_gap_ttft: Vec<OmissionGap>,
}

/// Whether the run's numbers can be published.
///
/// The spec's rule: a run where the generator itself was the bottleneck is
/// invalid, not published. The generator therefore has to notice, which is what
/// dispatch lag measures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Validity {
    pub valid: bool,
    pub reasons: Vec<String>,
    pub p99_dispatch_lag_us: u64,
    pub max_dispatch_lag_us: u64,
    pub error_fraction: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Counts {
    /// Requests issued, including warmup.
    pub issued: u64,
    /// Requests inside the warmup window, excluded from the summaries.
    pub warmup: u64,
    /// Requests outside warmup, whatever their outcome.
    pub measured: u64,
    /// Measured requests that returned 2xx and were read to the end.
    pub succeeded: u64,
    pub failed: u64,
    /// Successful measured requests divided by the measurement window.
    ///
    /// The window excludes warmup, because the requests counted above exclude
    /// it too. Dividing by the whole wall clock would deflate the rate by the
    /// warmup fraction and make two runs with different warmups
    /// incomparable.
    pub achieved_rate_per_second: f64,
    /// Seconds the rate above is measured over.
    pub measurement_window_secs: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Latencies {
    /// Time to first token, measured from when the request was due. The
    /// honest number.
    pub ttft_from_intended: Option<LatencySummary>,
    /// Time to first token, measured from when it was actually sent. What a
    /// generator without a schedule would report.
    pub ttft_from_dispatch: Option<LatencySummary>,
    pub e2e_from_intended: Option<LatencySummary>,
    pub e2e_from_dispatch: Option<LatencySummary>,
    /// How late the generator was on each request.
    pub dispatch_lag: Option<LatencySummary>,
}

impl RunReport {
    pub fn build(
        config: &RunConfig,
        records: &[RequestRecord],
        wall_clock: Duration,
        started_at: SystemTime,
    ) -> Self {
        let measured: Vec<&RequestRecord> =
            records.iter().filter(|record| !record.warmup).collect();

        let succeeded = measured.iter().filter(|record| record.succeeded()).count() as u64;
        let failed = measured.len() as u64 - succeeded;
        let error_fraction = if measured.is_empty() {
            0.0
        } else {
            failed as f64 / measured.len() as f64
        };

        let latency = Latencies {
            ttft_from_intended: summarise(&measured, |record| record.ttft_us),
            ttft_from_dispatch: summarise(&measured, |record| record.ttft_from_dispatch_us),
            e2e_from_intended: summarise(&measured, |record| record.e2e_us),
            e2e_from_dispatch: summarise(&measured, |record| record.e2e_from_dispatch_us),
            // Lag is taken over every request, warmup included: the generator
            // falling behind early is still the generator falling behind.
            dispatch_lag: summarise_all(records, |record| Some(record.dispatch_lag_us)),
        };

        let (p99_lag, max_lag) = match &latency.dispatch_lag {
            Some(summary) => (summary.value_at(99.0).unwrap_or(0), summary.max_us),
            None => (0, 0),
        };

        let mut reasons = Vec::new();
        let lag_budget_us = (config.max_dispatch_lag_ms * 1_000.0) as u64;
        if p99_lag > lag_budget_us {
            reasons.push(format!(
                "generator fell behind schedule: p99 dispatch lag {:.1}ms exceeds the {:.1}ms budget",
                p99_lag as f64 / 1_000.0,
                config.max_dispatch_lag_ms
            ));
        }
        if error_fraction > MAX_ERROR_FRACTION {
            reasons.push(format!(
                "error rate {:.2}% exceeds the {:.2}% budget",
                error_fraction * 100.0,
                MAX_ERROR_FRACTION * 100.0
            ));
        }
        if measured.is_empty() {
            reasons.push("no requests outside the warmup window".to_string());
        }

        let omission_gap_ttft = match (&latency.ttft_from_intended, &latency.ttft_from_dispatch) {
            (Some(intended), Some(dispatch)) => omission_gaps(intended, dispatch),
            _ => Vec::new(),
        };

        let started_at_unix_ms = started_at
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_millis() as u64)
            .unwrap_or(0);

        // The window the measured requests actually arrived in: everything
        // after warmup, up to however long the run really took.
        let measurement_window = (wall_clock.as_secs_f64() - config.warmup_secs).max(0.0);

        Self {
            run_id: run_id(config, started_at_unix_ms),
            started_at_unix_ms,
            git_sha: git_sha(),
            wall_clock_secs: wall_clock.as_secs_f64(),
            config: config.clone(),
            validity: Validity {
                valid: reasons.is_empty(),
                reasons,
                p99_dispatch_lag_us: p99_lag,
                max_dispatch_lag_us: max_lag,
                error_fraction,
            },
            counts: Counts {
                issued: records.len() as u64,
                warmup: records.len() as u64 - measured.len() as u64,
                measured: measured.len() as u64,
                succeeded,
                failed,
                achieved_rate_per_second: if measurement_window > 0.0 {
                    succeeded as f64 / measurement_window
                } else {
                    0.0
                },
                measurement_window_secs: measurement_window,
            },
            latency,
            omission_gap_ttft,
        }
    }

    /// One-line verdict for a terminal.
    pub fn headline(&self) -> String {
        let ttft = self
            .latency
            .ttft_from_intended
            .as_ref()
            .and_then(|summary| summary.value_at(99.0))
            .map(|value| format!("{:.1}ms", value as f64 / 1_000.0))
            .unwrap_or_else(|| "n/a".to_string());

        format!(
            "{} {} rate={:.0}/s measured={} p99 ttft={} {}",
            self.run_id,
            self.config.mode.as_str(),
            self.config.rate_per_second,
            self.counts.measured,
            ttft,
            if self.validity.valid {
                "valid"
            } else {
                "INVALID"
            }
        )
    }
}

fn summarise(
    records: &[&RequestRecord],
    extract: impl Fn(&RequestRecord) -> Option<u64>,
) -> Option<LatencySummary> {
    let mut histogram = new_histogram();
    for record in records {
        if let Some(value) = extract(record) {
            // Values above the histogram's ceiling are clamped rather than
            // dropped: losing the slowest requests is exactly the bias this
            // harness exists to avoid.
            histogram.saturating_record(value);
        }
    }
    LatencySummary::from_histogram(&histogram)
}

fn summarise_all(
    records: &[RequestRecord],
    extract: impl Fn(&RequestRecord) -> Option<u64>,
) -> Option<LatencySummary> {
    let borrowed: Vec<&RequestRecord> = records.iter().collect();
    summarise(&borrowed, extract)
}

fn run_id(config: &RunConfig, started_at_unix_ms: u64) -> String {
    format!(
        "{}-{}-r{:.0}-s{}",
        started_at_unix_ms,
        config.mode.as_str(),
        config.rate_per_second,
        config.seed
    )
}

/// Current commit, when the harness is running inside a git checkout.
fn git_sha() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// Write the report, the raw records, and a percentile series for plotting.
pub fn write_run(
    directory: &Path,
    report: &RunReport,
    records: &[RequestRecord],
) -> anyhow::Result<PathBuf> {
    let run_directory = directory.join(&report.run_id);
    std::fs::create_dir_all(&run_directory)?;

    std::fs::write(
        run_directory.join("report.json"),
        serde_json::to_vec_pretty(report)?,
    )?;

    let mut jsonl = String::with_capacity(records.len() * 192);
    for record in records {
        jsonl.push_str(&serde_json::to_string(record)?);
        jsonl.push('\n');
    }
    std::fs::write(run_directory.join("records.jsonl"), jsonl)?;

    std::fs::write(
        run_directory.join("percentiles.csv"),
        percentile_csv(&report.latency),
    )?;

    Ok(run_directory)
}

/// Percentile series for every latency metric, long format, ready to plot as a
/// CDF without any further processing.
fn percentile_csv(latency: &Latencies) -> String {
    let mut csv = String::from("metric,percentile,value_us\n");

    let metrics: [(&str, &Option<LatencySummary>); 5] = [
        ("ttft_from_intended", &latency.ttft_from_intended),
        ("ttft_from_dispatch", &latency.ttft_from_dispatch),
        ("e2e_from_intended", &latency.e2e_from_intended),
        ("e2e_from_dispatch", &latency.e2e_from_dispatch),
        ("dispatch_lag", &latency.dispatch_lag),
    ];

    for (name, summary) in metrics {
        let Some(summary) = summary else { continue };
        for percentile in REPORTED_PERCENTILES {
            if let Some(value) = summary.value_at(percentile) {
                csv.push_str(&format!("{name},{percentile},{value}\n"));
            }
        }
    }

    csv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Mode;

    fn config() -> RunConfig {
        RunConfig {
            target: "http://127.0.0.1:8080".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            model: "mock-model".to_string(),
            mode: Mode::OpenLoop,
            rate_per_second: 100.0,
            concurrency: 1,
            duration_secs: 10.0,
            warmup_secs: 1.0,
            seed: 1,
            prompt_words: 8,
            max_tokens: 8,
            stream: true,
            max_dispatch_lag_ms: 10.0,
        }
    }

    /// One healthy record: on time, fast, successful.
    fn record(index: u64, intended_us: u64, lag_us: u64, ttft_us: u64) -> RequestRecord {
        RequestRecord {
            index,
            intended_offset_us: intended_us,
            dispatch_offset_us: intended_us + lag_us,
            dispatch_lag_us: lag_us.max(1),
            ttft_us: Some(ttft_us + lag_us),
            ttft_from_dispatch_us: Some(ttft_us),
            e2e_us: Some(ttft_us + lag_us + 1_000),
            e2e_from_dispatch_us: Some(ttft_us + 1_000),
            status: Some(200),
            response_bytes: 512,
            error: None,
            warmup: intended_us < 1_000_000,
        }
    }

    fn build(records: &[RequestRecord]) -> RunReport {
        RunReport::build(
            &config(),
            records,
            Duration::from_secs(10),
            SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_000),
        )
    }

    #[test]
    fn a_punctual_run_is_valid() {
        let records: Vec<RequestRecord> = (0..500)
            .map(|index| record(index, 1_000_000 + index * 10_000, 200, 5_000))
            .collect();
        let report = build(&records);

        assert!(report.validity.valid, "{:?}", report.validity.reasons);
        assert_eq!(report.counts.measured, 500);
        assert_eq!(report.counts.succeeded, 500);
        assert_eq!(report.counts.failed, 0);
    }

    #[test]
    fn achieved_rate_is_measured_over_the_window_not_the_wall_clock() {
        // 900 successful requests in a 10s run with a 1s warmup. The window is
        // 9 seconds, so the rate is 100/s, not 90/s.
        let records: Vec<RequestRecord> = (0..900)
            .map(|index| record(index, 1_000_000 + index * 10_000, 100, 5_000))
            .collect();
        let report = build(&records);

        assert_eq!(report.counts.succeeded, 900);
        assert!(
            (report.counts.measurement_window_secs - 9.0).abs() < 1e-9,
            "window was {}",
            report.counts.measurement_window_secs
        );
        assert!(
            (report.counts.achieved_rate_per_second - 100.0).abs() < 1e-6,
            "rate was {}",
            report.counts.achieved_rate_per_second
        );
    }

    #[test]
    fn warmup_requests_are_counted_but_not_summarised() {
        let mut records: Vec<RequestRecord> = (0..100)
            .map(|index| record(index, index * 5_000, 100, 5_000))
            .collect();
        records
            .extend((100..300).map(|index| record(index, 1_000_000 + index * 5_000, 100, 5_000)));

        let report = build(&records);

        assert_eq!(report.counts.issued, 300);
        assert_eq!(report.counts.warmup, 100);
        assert_eq!(report.counts.measured, 200);
        assert_eq!(
            report
                .latency
                .ttft_from_intended
                .as_ref()
                .expect("summary")
                .count,
            200
        );
    }

    #[test]
    fn a_generator_that_fell_behind_marks_the_run_invalid() {
        // 50ms of lag against a 10ms budget.
        let records: Vec<RequestRecord> = (0..200)
            .map(|index| record(index, 1_000_000 + index * 10_000, 50_000, 5_000))
            .collect();
        let report = build(&records);

        assert!(!report.validity.valid);
        assert!(
            report.validity.reasons[0].contains("fell behind schedule"),
            "{:?}",
            report.validity.reasons
        );
        assert!(report.validity.p99_dispatch_lag_us >= 49_000);
    }

    #[test]
    fn too_many_errors_mark_the_run_invalid() {
        let mut records: Vec<RequestRecord> = (0..200)
            .map(|index| record(index, 1_000_000 + index * 10_000, 100, 5_000))
            .collect();
        for record in records.iter_mut().take(10) {
            record.error = Some("connection reset".to_string());
            record.e2e_us = None;
        }

        let report = build(&records);

        assert!(!report.validity.valid);
        assert!(
            report
                .validity
                .reasons
                .iter()
                .any(|reason| reason.contains("error rate")),
            "{:?}",
            report.validity.reasons
        );
        assert_eq!(report.counts.failed, 10);
    }

    #[test]
    fn a_run_with_nothing_past_warmup_is_invalid() {
        let records: Vec<RequestRecord> = (0..10)
            .map(|index| record(index, index * 1_000, 100, 5_000))
            .collect();
        let report = build(&records);

        assert!(!report.validity.valid);
        assert!(report.latency.ttft_from_intended.is_none());
    }

    #[test]
    fn lag_shows_up_as_an_omission_gap_on_every_percentile() {
        let records: Vec<RequestRecord> = (0..300)
            .map(|index| record(index, 1_000_000 + index * 10_000, 40_000, 5_000))
            .collect();
        let report = build(&records);

        assert!(!report.omission_gap_ttft.is_empty());
        for gap in &report.omission_gap_ttft {
            assert!(
                gap.ratio > 1.0,
                "p{} hid the lag: ratio {}",
                gap.percentile,
                gap.ratio
            );
        }
    }

    #[test]
    fn percentile_csv_covers_every_metric_that_has_data() {
        let records: Vec<RequestRecord> = (0..200)
            .map(|index| record(index, 1_000_000 + index * 10_000, 100, 5_000))
            .collect();
        let csv = percentile_csv(&build(&records).latency);

        assert!(csv.starts_with("metric,percentile,value_us\n"));
        for metric in [
            "ttft_from_intended",
            "ttft_from_dispatch",
            "e2e_from_intended",
            "e2e_from_dispatch",
            "dispatch_lag",
        ] {
            assert!(csv.contains(&format!("{metric},99,")), "missing {metric}");
        }
    }

    #[test]
    fn a_report_round_trips_through_json() {
        let records: Vec<RequestRecord> = (0..50)
            .map(|index| record(index, 1_000_000 + index * 10_000, 100, 5_000))
            .collect();
        let report = build(&records);

        let encoded = serde_json::to_string(&report).expect("report should serialize");
        let decoded: RunReport = serde_json::from_str(&encoded).expect("report should deserialize");

        assert_eq!(decoded.run_id, report.run_id);
        assert_eq!(decoded.counts.measured, report.counts.measured);
        assert_eq!(decoded.validity.valid, report.validity.valid);
    }
}
