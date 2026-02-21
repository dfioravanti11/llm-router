//! Worker metrics, in vLLM's shape.
//!
//! The metric names are vLLM's, not this project's, so the router's poller and
//! its parser are exercised against the format they will meet at R0.5. Anything
//! else would mean writing the integration twice and testing it once.
//!
//! The numbers behind the names are a model, not a measurement. What the mock
//! can honestly report is how many requests it is serving, how many are
//! waiting, and how full its simulated cache is.

use std::fmt::Write;

use crate::Counters;

/// Render the worker's state in Prometheus text format.
pub fn render(counters: &Counters, max_concurrency: usize, cache_blocks: usize) -> String {
    let running = counters.active.saturating_sub(counters.queued).max(0);

    // vLLM reports the fraction of KV cache blocks currently allocated to
    // running sequences. Here that is how much of the serving capacity is in
    // use, which is the same quantity a mock can defend: a worker with every
    // slot busy has no room for another sequence's KV.
    let kv_usage = if max_concurrency == 0 {
        0.0
    } else {
        (running as f64 / max_concurrency as f64).clamp(0.0, 1.0)
    };

    // Occupancy of the prefix cache itself, which is a different thing from KV
    // pressure and useful when reading a run.
    let cache_usage = if cache_blocks == 0 {
        0.0
    } else {
        (counters.cache.cached_blocks as f64 / cache_blocks as f64).clamp(0.0, 1.0)
    };

    let mut out = String::with_capacity(1024);
    metric(
        &mut out,
        "vllm:num_requests_running",
        "gauge",
        "Requests currently being served",
        running as f64,
    );
    metric(
        &mut out,
        "vllm:num_requests_waiting",
        "gauge",
        "Requests waiting for a serving slot",
        counters.queued.max(0) as f64,
    );
    metric(
        &mut out,
        "vllm:gpu_cache_usage_perc",
        "gauge",
        "Fraction of KV cache in use by running sequences",
        kv_usage,
    );
    metric(
        &mut out,
        "vllm:prefix_cache_queries_total",
        "counter",
        "Prefix cache block lookups",
        counters.cache.prefix_cache_queries as f64,
    );
    metric(
        &mut out,
        "vllm:prefix_cache_hits_total",
        "counter",
        "Prefix cache block lookups that hit",
        counters.cache.prefix_cache_hits as f64,
    );
    metric(
        &mut out,
        "warmpath_mock:prefix_cache_usage_perc",
        "gauge",
        "Fraction of the simulated prefix cache holding blocks",
        cache_usage,
    );

    out
}

fn metric(out: &mut String, name: &str, kind: &str, help: &str, value: f64) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
    let _ = writeln!(out, "{name} {value}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CacheStats;

    fn counters(active: i64, queued: i64) -> Counters {
        Counters {
            active,
            queued,
            available_slots: 0,
            started: 0,
            completed: 0,
            cancelled: 0,
            cache: CacheStats {
                prefix_cache_queries: 100,
                prefix_cache_hits: 40,
                cached_blocks: 32,
                evicted_blocks: 0,
            },
        }
    }

    fn value_of(rendered: &str, name: &str) -> f64 {
        rendered
            .lines()
            .find(|line| line.starts_with(name) && !line.starts_with('#'))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("no value for {name} in:\n{rendered}"))
    }

    #[test]
    fn running_excludes_the_requests_still_waiting() {
        // Ten in the worker, four of them queued, so six are being served.
        let rendered = render(&counters(10, 4), 8, 64);

        assert_eq!(value_of(&rendered, "vllm:num_requests_running"), 6.0);
        assert_eq!(value_of(&rendered, "vllm:num_requests_waiting"), 4.0);
    }

    #[test]
    fn kv_usage_is_the_share_of_serving_slots_in_use() {
        let rendered = render(&counters(4, 0), 8, 64);
        assert!((value_of(&rendered, "vllm:gpu_cache_usage_perc") - 0.5).abs() < 1e-9);
    }

    #[test]
    fn kv_usage_stays_inside_zero_and_one_when_the_worker_is_swamped() {
        let rendered = render(&counters(40, 30), 8, 64);
        let usage = value_of(&rendered, "vllm:gpu_cache_usage_perc");
        assert!((0.0..=1.0).contains(&usage), "{usage}");
    }

    #[test]
    fn the_prefix_cache_counters_are_reported_under_vllm_names() {
        let rendered = render(&counters(1, 0), 8, 64);

        assert_eq!(
            value_of(&rendered, "vllm:prefix_cache_queries_total"),
            100.0
        );
        assert_eq!(value_of(&rendered, "vllm:prefix_cache_hits_total"), 40.0);
        assert!((value_of(&rendered, "warmpath_mock:prefix_cache_usage_perc") - 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_disabled_cache_does_not_divide_by_zero() {
        let rendered = render(&counters(1, 0), 0, 0);

        assert_eq!(value_of(&rendered, "vllm:gpu_cache_usage_perc"), 0.0);
        assert_eq!(
            value_of(&rendered, "warmpath_mock:prefix_cache_usage_perc"),
            0.0
        );
    }

    #[test]
    fn every_metric_carries_help_and_type() {
        let rendered = render(&counters(1, 0), 8, 64);

        for name in [
            "vllm:num_requests_running",
            "vllm:num_requests_waiting",
            "vllm:gpu_cache_usage_perc",
            "vllm:prefix_cache_queries_total",
            "vllm:prefix_cache_hits_total",
        ] {
            assert!(rendered.contains(&format!("# HELP {name} ")), "{name}");
            assert!(rendered.contains(&format!("# TYPE {name} ")), "{name}");
        }
    }
}
