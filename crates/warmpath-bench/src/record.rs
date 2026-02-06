//! Per-request records and the run configuration that produced them.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How arrivals are driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum Mode {
    /// Arrival times are fixed before the run starts. A slow response never
    /// delays a later arrival.
    OpenLoop,
    /// A fixed number of callers each send, wait for the full response, and
    /// send again. Present so the coordinated-omission gap can be demonstrated
    /// against a generator that has the defect, not merely described.
    ClosedLoop,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::OpenLoop => "open-loop",
            Mode::ClosedLoop => "closed-loop",
        }
    }
}

/// Everything needed to reproduce a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunConfig {
    /// Base URL of the endpoint under test, for example `http://127.0.0.1:8080`.
    pub target: String,
    pub endpoint: String,
    pub model: String,
    pub mode: Mode,
    /// Arrivals per second. Open loop only.
    pub rate_per_second: f64,
    /// Concurrent callers. Closed loop only.
    pub concurrency: usize,
    pub duration_secs: f64,
    /// Leading window excluded from the reported summary.
    pub warmup_secs: f64,
    pub seed: u64,
    /// Words in the generated prompt.
    pub prompt_words: usize,
    pub max_tokens: usize,
    pub stream: bool,
    /// A run whose p99 dispatch lag exceeds this is marked invalid.
    pub max_dispatch_lag_ms: f64,
}

impl RunConfig {
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.duration_secs)
    }

    pub fn warmup(&self) -> Duration {
        Duration::from_secs_f64(self.warmup_secs)
    }

    pub fn url(&self) -> String {
        format!(
            "{}{}",
            self.target.trim_end_matches('/'),
            if self.endpoint.starts_with('/') {
                self.endpoint.clone()
            } else {
                format!("/{}", self.endpoint)
            }
        )
    }
}

/// One request, as observed by the generator.
///
/// Every latency is recorded twice: once from the time the request was *due*,
/// and once from the time it was actually sent. The first is the honest
/// number. The second is what a generator without a schedule would report, and
/// keeping both means the coordinated-omission gap falls out of any run rather
/// than needing a separate experiment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    pub index: u64,
    /// When the request was due, as an offset from the start of the run.
    pub intended_offset_us: u64,
    /// When the request was actually sent, as an offset from the start.
    pub dispatch_offset_us: u64,
    /// How late the generator was. Zero in closed loop, where a request is due
    /// exactly when the caller is free to send it.
    pub dispatch_lag_us: u64,
    /// Time to first byte, measured from the intended time.
    pub ttft_us: Option<u64>,
    /// Time to first byte, measured from actual dispatch.
    pub ttft_from_dispatch_us: Option<u64>,
    /// Time to last byte, measured from the intended time.
    pub e2e_us: Option<u64>,
    /// Time to last byte, measured from actual dispatch.
    pub e2e_from_dispatch_us: Option<u64>,
    pub status: Option<u16>,
    pub response_bytes: u64,
    pub error: Option<String>,
    /// Excluded from the reported summary.
    pub warmup: bool,
}

impl RequestRecord {
    /// A request counts as successful when it returned 2xx and the body was
    /// read to the end.
    pub fn succeeded(&self) -> bool {
        self.error.is_none()
            && self.e2e_us.is_some()
            && matches!(self.status, Some(status) if (200..300).contains(&status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(endpoint: &str, target: &str) -> RunConfig {
        RunConfig {
            target: target.to_string(),
            endpoint: endpoint.to_string(),
            model: "mock-model".to_string(),
            mode: Mode::OpenLoop,
            rate_per_second: 10.0,
            concurrency: 1,
            duration_secs: 1.0,
            warmup_secs: 0.0,
            seed: 1,
            prompt_words: 16,
            max_tokens: 8,
            stream: true,
            max_dispatch_lag_ms: 10.0,
        }
    }

    #[test]
    fn url_joins_without_doubling_or_dropping_a_slash() {
        assert_eq!(
            config("/v1/chat/completions", "http://host:8080").url(),
            "http://host:8080/v1/chat/completions"
        );
        assert_eq!(
            config("v1/chat/completions", "http://host:8080/").url(),
            "http://host:8080/v1/chat/completions"
        );
    }

    #[test]
    fn success_needs_a_status_a_body_and_no_error() {
        let base = RequestRecord {
            index: 0,
            intended_offset_us: 0,
            dispatch_offset_us: 0,
            dispatch_lag_us: 0,
            ttft_us: Some(10),
            ttft_from_dispatch_us: Some(10),
            e2e_us: Some(20),
            e2e_from_dispatch_us: Some(20),
            status: Some(200),
            response_bytes: 100,
            error: None,
            warmup: false,
        };
        assert!(base.succeeded());

        let mut errored = base.clone();
        errored.error = Some("connection reset".to_string());
        assert!(!errored.succeeded());

        let mut server_error = base.clone();
        server_error.status = Some(502);
        assert!(!server_error.succeeded());

        let mut truncated = base.clone();
        truncated.e2e_us = None;
        assert!(!truncated.succeeded());
    }
}
