//! Prometheus metrics.
//!
//! The worker set is fixed at startup, so every per-worker metric handle is
//! resolved once into [`WorkerMetrics`] and cloned into the request path.
//! `prometheus-client` metrics share their storage across clones, so the hot
//! path touches an atomic (counters, gauges) without going through the label
//! map. Histograms still take a short mutex per observation; whether that
//! matters is a question for R0.5, which measures the router's own overhead
//! rather than assuming it.

use prometheus_client::encoding::text::encode;
use prometheus_client::encoding::{EncodeLabelSet, EncodeLabelValue};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{exponential_buckets, Histogram};
use prometheus_client::registry::{Registry, Unit};

/// How a request ended.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelValue)]
pub enum Outcome {
    /// The response body was delivered in full.
    Completed,
    /// The client went away before the response finished.
    Cancelled,
    /// The upstream connection or stream failed.
    Failed,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct RequestLabels {
    worker: String,
    outcome: Outcome,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct WorkerLabels {
    worker: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct DecisionLabels {
    worker: String,
    policy: String,
}

/// Bucket bounds for latency histograms, in seconds. Exponential from 1ms so
/// the interesting range for TTFT is covered without a wide flat tail.
fn latency_buckets() -> impl Iterator<Item = f64> {
    exponential_buckets(0.001, 2.0, 16)
}

pub struct Metrics {
    registry: Registry,
    requests: Family<RequestLabels, Counter>,
    decisions: Family<DecisionLabels, Counter>,
    in_flight: Gauge,
    rejected: Counter,
    ttfb: Family<WorkerLabels, Histogram>,
    e2e: Family<WorkerLabels, Histogram>,
}

impl std::fmt::Debug for Metrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Metrics").finish_non_exhaustive()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = <Registry>::with_prefix("warmpath");

        let requests = Family::<RequestLabels, Counter>::default();
        registry.register(
            "requests",
            "Requests forwarded to a worker, by how they ended",
            requests.clone(),
        );

        let decisions = Family::<DecisionLabels, Counter>::default();
        registry.register(
            "routing_decisions",
            "Routing decisions, by chosen worker and the policy that chose it",
            decisions.clone(),
        );

        let in_flight = Gauge::default();
        registry.register(
            "in_flight_requests",
            "Requests dispatched to a worker whose response has not finished",
            in_flight.clone(),
        );

        let rejected = Counter::default();
        registry.register(
            "rejected_requests",
            "Requests rejected by the router before any worker was chosen",
            rejected.clone(),
        );

        let ttfb = Family::<WorkerLabels, Histogram>::new_with_constructor(|| {
            Histogram::new(latency_buckets())
        });
        registry.register_with_unit(
            "time_to_first_byte",
            "Delay from dispatch to the first response byte",
            Unit::Seconds,
            ttfb.clone(),
        );

        let e2e = Family::<WorkerLabels, Histogram>::new_with_constructor(|| {
            Histogram::new(latency_buckets())
        });
        registry.register_with_unit(
            "end_to_end_latency",
            "Delay from dispatch to the last response byte",
            Unit::Seconds,
            e2e.clone(),
        );

        Self {
            registry,
            requests,
            decisions,
            in_flight,
            rejected,
            ttfb,
            e2e,
        }
    }

    /// Resolve every handle a request path needs for one worker, once.
    ///
    /// Each lookup is bound to its own `let` before the next one runs.
    /// `Family::get_or_create` hands back a guard holding a read lock on the
    /// label map and takes a write lock when the label set is new, so two live
    /// guards on the same family deadlock the calling thread. Temporaries in a
    /// struct literal all live until the end of that statement, which is
    /// exactly the shape that deadlocks.
    pub fn for_worker(&self, worker: &str, policy: &str) -> WorkerMetrics {
        let labels = |outcome: Outcome| RequestLabels {
            worker: worker.to_string(),
            outcome,
        };
        let worker_labels = WorkerLabels {
            worker: worker.to_string(),
        };
        let decision_labels = DecisionLabels {
            worker: worker.to_string(),
            policy: policy.to_string(),
        };

        let completed = self
            .requests
            .get_or_create(&labels(Outcome::Completed))
            .clone();
        let cancelled = self
            .requests
            .get_or_create(&labels(Outcome::Cancelled))
            .clone();
        let failed = self
            .requests
            .get_or_create(&labels(Outcome::Failed))
            .clone();
        let chosen = self.decisions.get_or_create(&decision_labels).clone();
        let ttfb = self.ttfb.get_or_create(&worker_labels).clone();
        let e2e = self.e2e.get_or_create(&worker_labels).clone();

        WorkerMetrics {
            completed,
            cancelled,
            failed,
            chosen,
            ttfb,
            e2e,
            in_flight: self.in_flight.clone(),
        }
    }

    pub fn record_rejection(&self) {
        self.rejected.inc();
    }

    /// Render the registry in Prometheus text format.
    pub fn encode(&self) -> Result<String, std::fmt::Error> {
        let mut buffer = String::new();
        encode(&mut buffer, &self.registry)?;
        Ok(buffer)
    }
}

/// Every metric handle the request path needs for one worker.
#[derive(Clone, Debug)]
pub struct WorkerMetrics {
    completed: Counter,
    cancelled: Counter,
    failed: Counter,
    chosen: Counter,
    ttfb: Histogram,
    e2e: Histogram,
    in_flight: Gauge,
}

impl WorkerMetrics {
    pub fn record_dispatch(&self) {
        self.chosen.inc();
        self.in_flight.inc();
    }

    pub fn record_first_byte(&self, seconds: f64) {
        self.ttfb.observe(seconds);
    }

    pub fn record_completion(&self, seconds: f64) {
        self.e2e.observe(seconds);
        self.completed.inc();
        self.in_flight.dec();
    }

    pub fn record_cancellation(&self) {
        self.cancelled.inc();
        self.in_flight.dec();
    }

    pub fn record_failure(&self) {
        self.failed.inc();
        self.in_flight.dec();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_output_carries_the_registered_families() {
        let metrics = Metrics::new();
        let worker = metrics.for_worker("w0", "round-robin");

        worker.record_dispatch();
        worker.record_first_byte(0.012);
        worker.record_completion(0.250);

        let encoded = metrics.encode().expect("registry should encode");

        assert!(encoded.contains("warmpath_requests_total"), "{encoded}");
        assert!(
            encoded.contains(r#"worker="w0",outcome="Completed""#),
            "{encoded}"
        );
        assert!(
            encoded.contains(
                r#"warmpath_routing_decisions_total{worker="w0",policy="round-robin"} 1"#
            ),
            "{encoded}"
        );
        assert!(
            encoded.contains("warmpath_time_to_first_byte_seconds"),
            "{encoded}"
        );
    }

    #[test]
    fn in_flight_returns_to_zero_on_every_terminal_outcome() {
        let metrics = Metrics::new();
        let worker = metrics.for_worker("w0", "round-robin");

        worker.record_dispatch();
        worker.record_completion(0.1);
        worker.record_dispatch();
        worker.record_cancellation();
        worker.record_dispatch();
        worker.record_failure();

        let encoded = metrics.encode().expect("registry should encode");
        assert!(
            encoded.contains("warmpath_in_flight_requests 0"),
            "{encoded}"
        );
    }

    #[test]
    fn handles_cloned_for_the_same_worker_share_storage() {
        let metrics = Metrics::new();
        metrics.for_worker("w0", "round-robin").record_dispatch();
        metrics.for_worker("w0", "round-robin").record_dispatch();

        let encoded = metrics.encode().expect("registry should encode");
        assert!(
            encoded.contains(
                r#"warmpath_routing_decisions_total{worker="w0",policy="round-robin"} 2"#
            ),
            "{encoded}"
        );
    }
}
