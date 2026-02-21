//! Aggregating several runs into a publishable result.
//!
//! A single run is never enough. The spec's rule is at least three independent
//! runs per configuration, reported as a median with a confidence interval, so
//! this module takes run reports and produces exactly that. Invalid runs are
//! excluded from the statistics and reported separately, because dropping them
//! quietly is how a harness starts lying.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::report::RunReport;
use crate::stats::{Aggregate, REPORTED_PERCENTILES};

/// Metrics aggregated across runs, keyed by a stable name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Campaign {
    /// Reports handed in, valid or not.
    pub runs: usize,
    /// Reports that contributed to the statistics.
    pub valid_runs: usize,
    /// Why each excluded run was excluded.
    pub excluded: Vec<ExcludedRun>,
    pub git_shas: Vec<String>,
    pub metrics: BTreeMap<String, Aggregate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExcludedRun {
    pub run_id: String,
    pub reasons: Vec<String>,
}

impl Campaign {
    pub fn from_reports(reports: &[RunReport]) -> anyhow::Result<Self> {
        anyhow::ensure!(!reports.is_empty(), "no run reports to aggregate");

        let (valid, invalid): (Vec<&RunReport>, Vec<&RunReport>) =
            reports.iter().partition(|report| report.validity.valid);

        anyhow::ensure!(
            !valid.is_empty(),
            "every run was invalid; nothing can be published from this campaign"
        );

        let mut metrics: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for report in &valid {
            let latency = &report.latency;
            collect(
                &mut metrics,
                "ttft_from_intended",
                &latency.ttft_from_intended,
            );
            collect(
                &mut metrics,
                "ttft_from_dispatch",
                &latency.ttft_from_dispatch,
            );
            collect(
                &mut metrics,
                "e2e_from_intended",
                &latency.e2e_from_intended,
            );
            collect(
                &mut metrics,
                "e2e_from_dispatch",
                &latency.e2e_from_dispatch,
            );
            collect(&mut metrics, "dispatch_lag", &latency.dispatch_lag);

            metrics
                .entry("achieved_rate_per_second".to_string())
                .or_default()
                .push(report.counts.achieved_rate_per_second);
        }

        let mut git_shas: Vec<String> = valid
            .iter()
            .filter_map(|report| report.git_sha.clone())
            .collect();
        git_shas.sort();
        git_shas.dedup();

        Ok(Self {
            runs: reports.len(),
            valid_runs: valid.len(),
            excluded: invalid
                .iter()
                .map(|report| ExcludedRun {
                    run_id: report.run_id.clone(),
                    reasons: report.validity.reasons.clone(),
                })
                .collect(),
            git_shas,
            metrics: metrics
                .into_iter()
                .filter_map(|(name, values)| {
                    Aggregate::from_values(&values).map(|aggregate| (name, aggregate))
                })
                .collect(),
        })
    }

    /// Whether the campaign meets the spec's bar for publishing: at least three
    /// valid runs, all from the same commit.
    pub fn publishable(&self) -> Result<(), String> {
        if self.valid_runs < 3 {
            return Err(format!(
                "{} valid run(s); at least 3 are needed to report a confidence interval",
                self.valid_runs
            ));
        }
        if self.git_shas.len() > 1 {
            return Err(format!(
                "runs came from {} different commits: {}",
                self.git_shas.len(),
                self.git_shas.join(", ")
            ));
        }
        Ok(())
    }

    /// Table for a terminal, one line per metric.
    pub fn table(&self) -> String {
        let mut out = format!(
            "{:<34} {:>12} {:>12} {:>22}\n",
            "metric", "median", "mean", "95% CI"
        );
        for (name, aggregate) in &self.metrics {
            let interval = match aggregate.ci95() {
                Some((low, high)) => format!("[{low:.1}, {high:.1}]"),
                None => "n/a (1 run)".to_string(),
            };
            out.push_str(&format!(
                "{:<34} {:>12.1} {:>12.1} {:>22}\n",
                name, aggregate.median, aggregate.mean, interval
            ));
        }
        out
    }
}

/// Push this run's value for every reported percentile of one metric.
fn collect(
    metrics: &mut BTreeMap<String, Vec<f64>>,
    name: &str,
    summary: &Option<crate::stats::LatencySummary>,
) {
    let Some(summary) = summary else { return };

    for percentile in REPORTED_PERCENTILES {
        if let Some(value) = summary.value_at(percentile) {
            metrics
                .entry(format!("{name}_p{percentile}_us"))
                .or_default()
                .push(value as f64);
        }
    }
    metrics
        .entry(format!("{name}_mean_us"))
        .or_default()
        .push(summary.mean_us);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Mode, RequestRecord, RunConfig};
    use std::time::{Duration, SystemTime};

    fn config(seed: u64) -> RunConfig {
        RunConfig {
            target: "http://127.0.0.1:8080".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            model: "mock-model".to_string(),
            mode: Mode::OpenLoop,
            rate_per_second: 100.0,
            concurrency: 1,
            duration_secs: 10.0,
            warmup_secs: 0.0,
            seed,
            label: String::new(),
            prompt_words: 8,
            shared_prefix_words: 0,
            prefix_pool: 0,
            hot_prefix_share: 0.0,
            session_turns: 0,
            max_tokens: 8,
            stream: true,
            max_dispatch_lag_ms: 10.0,
        }
    }

    /// A synthetic run whose every request took `ttft_us`, so its percentiles
    /// are known exactly and the aggregate can be hand-checked.
    fn report(seed: u64, ttft_us: u64, lag_us: u64) -> RunReport {
        let records: Vec<RequestRecord> = (0..200)
            .map(|index| RequestRecord {
                index,
                intended_offset_us: index * 10_000,
                dispatch_offset_us: index * 10_000 + lag_us,
                dispatch_lag_us: lag_us.max(1),
                ttft_us: Some(ttft_us),
                ttft_from_dispatch_us: Some(ttft_us),
                e2e_us: Some(ttft_us * 2),
                e2e_from_dispatch_us: Some(ttft_us * 2),
                status: Some(200),
                response_bytes: 256,
                error: None,
                warmup: false,
            })
            .collect();

        RunReport::build(
            &config(seed),
            &records,
            Duration::from_secs(10),
            SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_000 + seed),
        )
    }

    #[test]
    fn aggregating_nothing_is_an_error() {
        assert!(Campaign::from_reports(&[]).is_err());
    }

    #[test]
    fn three_runs_produce_an_interval_around_the_known_value() {
        // p99 TTFT of 10ms, 20ms, 30ms across three runs. Mean 20ms,
        // sample std dev 10ms, t(2) = 4.303, so the half-width is
        // 4.303 * 10000 / sqrt(3) = 24843.6 microseconds.
        let campaign = Campaign::from_reports(&[
            report(1, 10_000, 100),
            report(2, 20_000, 100),
            report(3, 30_000, 100),
        ])
        .expect("should aggregate");

        assert_eq!(campaign.runs, 3);
        assert_eq!(campaign.valid_runs, 3);
        assert!(campaign.excluded.is_empty());

        let p99 = campaign
            .metrics
            .get("ttft_from_intended_p99_us")
            .expect("p99 metric");
        assert!((p99.median - 20_000.0).abs() < 20.0, "{}", p99.median);
        assert!((p99.mean - 20_000.0).abs() < 20.0, "{}", p99.mean);

        let half = p99.ci95_half_width.expect("half width");
        assert!((half - 24_843.6).abs() < 50.0, "half width was {half}");
    }

    #[test]
    fn invalid_runs_are_excluded_and_named() {
        // 50ms lag against a 10ms budget makes the third run invalid.
        let campaign = Campaign::from_reports(&[
            report(1, 10_000, 100),
            report(2, 12_000, 100),
            report(3, 11_000, 50_000),
        ])
        .expect("should aggregate");

        assert_eq!(campaign.runs, 3);
        assert_eq!(campaign.valid_runs, 2);
        assert_eq!(campaign.excluded.len(), 1);
        assert!(
            campaign.excluded[0].reasons[0].contains("fell behind schedule"),
            "{:?}",
            campaign.excluded[0].reasons
        );

        let p99 = campaign
            .metrics
            .get("ttft_from_intended_p99_us")
            .expect("p99 metric");
        assert_eq!(p99.runs, 2, "the invalid run should not contribute");
    }

    #[test]
    fn a_campaign_of_only_invalid_runs_is_an_error() {
        let error = Campaign::from_reports(&[report(1, 10_000, 60_000)])
            .expect_err("should refuse to aggregate");

        assert!(error.to_string().contains("every run was invalid"));
    }

    #[test]
    fn fewer_than_three_valid_runs_is_not_publishable() {
        let campaign = Campaign::from_reports(&[report(1, 10_000, 100), report(2, 12_000, 100)])
            .expect("should aggregate");

        let error = campaign
            .publishable()
            .expect_err("should not be publishable");
        assert!(error.contains("at least 3"), "{error}");
    }

    #[test]
    fn three_valid_runs_from_one_commit_are_publishable() {
        let mut reports = vec![
            report(1, 10_000, 100),
            report(2, 12_000, 100),
            report(3, 11_000, 100),
        ];
        for report in &mut reports {
            report.git_sha = Some("abc123".to_string());
        }

        let campaign = Campaign::from_reports(&reports).expect("should aggregate");
        assert_eq!(campaign.git_shas, ["abc123"]);
        assert!(campaign.publishable().is_ok());
    }

    #[test]
    fn runs_from_different_commits_are_not_publishable() {
        let mut reports = vec![
            report(1, 10_000, 100),
            report(2, 12_000, 100),
            report(3, 11_000, 100),
        ];
        reports[0].git_sha = Some("abc123".to_string());
        reports[1].git_sha = Some("abc123".to_string());
        reports[2].git_sha = Some("def456".to_string());

        let campaign = Campaign::from_reports(&reports).expect("should aggregate");
        let error = campaign
            .publishable()
            .expect_err("should not be publishable");
        assert!(error.contains("different commits"), "{error}");
    }

    #[test]
    fn the_table_names_every_aggregated_metric() {
        let campaign = Campaign::from_reports(&[
            report(1, 10_000, 100),
            report(2, 12_000, 100),
            report(3, 11_000, 100),
        ])
        .expect("should aggregate");

        let table = campaign.table();
        assert!(table.contains("ttft_from_intended_p99_us"), "{table}");
        assert!(table.contains("achieved_rate_per_second"), "{table}");
    }
}
