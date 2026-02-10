//! Choosing a worker.
//!
//! [`choose`] is a pure function of the index's answer, the observed load, and
//! a rotation cursor. Keeping it free of locks and clocks means every branch
//! can be tested directly, and it satisfies the reproducibility rule the
//! benchmark depends on: identical index state and identical cursor produce an
//! identical decision, so an A/B run is a comparison rather than a coincidence.

use serde::Serialize;

use crate::config::{AffinityConfig, Policy};

/// What the policy gets to look at.
#[derive(Debug, Clone, Copy)]
pub struct RoutingInputs<'a> {
    /// Blocks in this request's prompt. Length is the denominator of every
    /// match ratio.
    pub prompt_blocks: usize,
    /// Leading blocks each worker is believed to hold.
    pub matched: &'a [usize],
    /// Requests already dispatched to each worker and not yet finished.
    ///
    /// R0.3 uses the router's own in-flight count. R0.4 replaces it with the
    /// worker's reported queue depth and KV utilization, which is the signal
    /// that actually reflects the engine rather than the proxy.
    pub load: &'a [usize],
}

/// Why a request went where it went.
///
/// Recorded per request and exported as a metric label. A routing result that
/// cannot be attributed to a reason is not diagnosable, and telling
/// "affinity worked" apart from "affinity was overridden" is most of the
/// analysis at R0.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecisionReason {
    /// Pinned to the first worker.
    First,
    /// Even rotation, ignoring everything else.
    RoundRobin,
    /// Chosen because it holds the prompt's prefix.
    Affinity,
    /// Affinity yielded, because the fleet was too far out of balance.
    BalanceOverride,
    /// No worker held enough of the prefix to be worth preferring, so load
    /// decided.
    CacheMiss,
}

impl DecisionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionReason::First => "first",
            DecisionReason::RoundRobin => "round-robin",
            DecisionReason::Affinity => "affinity",
            DecisionReason::BalanceOverride => "balance-override",
            DecisionReason::CacheMiss => "cache-miss",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    pub worker: usize,
    pub reason: DecisionReason,
    /// Blocks the chosen worker was believed to already hold.
    pub matched_blocks: usize,
    /// `matched_blocks` over the prompt's block count, or zero for a prompt
    /// too short to have blocks.
    pub match_ratio: f64,
}

/// Pick a worker.
///
/// `cursor` supplies the rotation for policies that need an arbitrary but
/// deterministic choice.
pub fn choose(
    policy: Policy,
    config: &AffinityConfig,
    inputs: RoutingInputs<'_>,
    cursor: usize,
) -> Decision {
    let worker_count = inputs.load.len();
    debug_assert!(worker_count > 0, "there is always at least one worker");
    debug_assert_eq!(inputs.matched.len(), worker_count);

    match policy {
        Policy::First => decide(0, DecisionReason::First, inputs),
        Policy::RoundRobin => decide(cursor % worker_count, DecisionReason::RoundRobin, inputs),
        Policy::PrefixAffinity => prefix_affinity(config, inputs, cursor),
        Policy::PrefixAffinityBalanced => balanced(config, inputs, cursor),
    }
}

/// Longest match wins, and nothing else gets a vote.
///
/// This is the naive policy on purpose. It is the one that hotspots when a
/// prefix gets hot, and R0.4 exists to find a workload where it loses to the
/// balanced variant. Keeping it honest, rather than quietly adding a little
/// load-awareness, is what makes that comparison mean anything.
fn prefix_affinity(config: &AffinityConfig, inputs: RoutingInputs<'_>, cursor: usize) -> Decision {
    let best = best_match(inputs);

    if !is_worth_preferring(config, inputs, best.matched) {
        return decide(
            least_loaded(inputs, cursor),
            DecisionReason::CacheMiss,
            inputs,
        );
    }

    decide(best.worker, DecisionReason::Affinity, inputs)
}

/// Affinity, until affinity would hurt more than it helps.
fn balanced(config: &AffinityConfig, inputs: RoutingInputs<'_>, cursor: usize) -> Decision {
    let max_load = inputs.load.iter().copied().max().unwrap_or(0);
    let min_load = inputs.load.iter().copied().min().unwrap_or(0);

    // An imbalanced fleet is a routing emergency: cache locality is worth
    // nothing if the worker holding the prefix is the one that cannot keep up.
    // Both thresholds have to trip. The absolute one keeps a fleet at
    // one-versus-zero in flight from being called imbalanced, where the ratio
    // is infinite and meaningless.
    let imbalanced = max_load > config.balance_abs_threshold
        && (max_load as f64) > config.balance_rel_threshold * (min_load as f64);
    if imbalanced {
        return decide(
            least_loaded(inputs, cursor),
            DecisionReason::BalanceOverride,
            inputs,
        );
    }

    let best = best_match(inputs);
    if !is_worth_preferring(config, inputs, best.matched) {
        return decide(
            least_loaded(inputs, cursor),
            DecisionReason::CacheMiss,
            inputs,
        );
    }

    // Score every worker on cache locality and headroom together, so a big
    // match on a busy worker can still lose to a smaller match on an idle one.
    let mut chosen = 0;
    let mut best_score = f64::NEG_INFINITY;
    for worker in 0..inputs.load.len() {
        let score = affinity_score(config, inputs, worker, max_load);
        if score > best_score {
            best_score = score;
            chosen = worker;
        }
    }

    let reason = if inputs.matched[chosen] > 0 {
        DecisionReason::Affinity
    } else {
        DecisionReason::CacheMiss
    };
    decide(chosen, reason, inputs)
}

fn affinity_score(
    config: &AffinityConfig,
    inputs: RoutingInputs<'_>,
    worker: usize,
    max_load: usize,
) -> f64 {
    let ratio = match_ratio(inputs, worker);
    // Headroom is relative to the busiest worker, so the scale adapts to the
    // fleet instead of needing an absolute capacity the router does not know.
    // R0.4 replaces this with real KV utilization.
    let headroom = 1.0 - (inputs.load[worker] as f64 / (max_load as f64 + 1.0));

    (1.0 - config.load_weight) * ratio + config.load_weight * headroom
}

struct BestMatch {
    worker: usize,
    matched: usize,
}

/// The worker holding the most of this prompt's prefix, ties going to the
/// less loaded one and then to the cursor.
fn best_match(inputs: RoutingInputs<'_>) -> BestMatch {
    let mut worker = 0;
    let mut matched = 0;

    for candidate in 0..inputs.matched.len() {
        let blocks = inputs.matched[candidate];
        let better =
            blocks > matched || (blocks == matched && inputs.load[candidate] < inputs.load[worker]);
        if better {
            worker = candidate;
            matched = blocks;
        }
    }

    BestMatch { worker, matched }
}

/// Whether a match is large enough to route on.
///
/// A two-block match on a hundred-block prompt saves almost no prefill and
/// still drags the request onto a specific worker. Below the threshold the
/// match is treated as no match at all, which is what keeps affinity from
/// quietly turning into a bad load balancer.
fn is_worth_preferring(config: &AffinityConfig, inputs: RoutingInputs<'_>, matched: usize) -> bool {
    if inputs.prompt_blocks == 0 || matched == 0 {
        return false;
    }
    (matched as f64 / inputs.prompt_blocks as f64) >= config.cache_threshold
}

/// Least loaded, ties going to the cursor so the choice rotates instead of
/// always landing on the lowest index.
fn least_loaded(inputs: RoutingInputs<'_>, cursor: usize) -> usize {
    let worker_count = inputs.load.len();
    let mut chosen = cursor % worker_count;
    let mut lowest = inputs.load[chosen];

    for offset in 1..worker_count {
        let candidate = (cursor + offset) % worker_count;
        if inputs.load[candidate] < lowest {
            lowest = inputs.load[candidate];
            chosen = candidate;
        }
    }

    chosen
}

fn match_ratio(inputs: RoutingInputs<'_>, worker: usize) -> f64 {
    if inputs.prompt_blocks == 0 {
        return 0.0;
    }
    inputs.matched[worker] as f64 / inputs.prompt_blocks as f64
}

fn decide(worker: usize, reason: DecisionReason, inputs: RoutingInputs<'_>) -> Decision {
    Decision {
        worker,
        reason,
        matched_blocks: inputs.matched[worker],
        match_ratio: match_ratio(inputs, worker),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AffinityConfig {
        AffinityConfig {
            cache_threshold: 0.2,
            balance_abs_threshold: 4,
            balance_rel_threshold: 2.0,
            load_weight: 0.3,
        }
    }

    fn inputs<'a>(
        prompt_blocks: usize,
        matched: &'a [usize],
        load: &'a [usize],
    ) -> RoutingInputs<'a> {
        RoutingInputs {
            prompt_blocks,
            matched,
            load,
        }
    }

    #[test]
    fn first_always_picks_worker_zero() {
        let decision = choose(
            Policy::First,
            &config(),
            inputs(10, &[0, 10, 0], &[9, 0, 0]),
            7,
        );
        assert_eq!(decision.worker, 0);
        assert_eq!(decision.reason, DecisionReason::First);
    }

    #[test]
    fn round_robin_follows_the_cursor_and_ignores_everything_else() {
        for cursor in 0..6 {
            let decision = choose(
                Policy::RoundRobin,
                &config(),
                inputs(10, &[10, 0, 0], &[0, 0, 99]),
                cursor,
            );
            assert_eq!(decision.worker, cursor % 3);
            assert_eq!(decision.reason, DecisionReason::RoundRobin);
        }
    }

    #[test]
    fn affinity_picks_the_worker_holding_the_prefix() {
        let decision = choose(
            Policy::PrefixAffinity,
            &config(),
            inputs(10, &[2, 8, 0], &[0, 0, 0]),
            0,
        );

        assert_eq!(decision.worker, 1);
        assert_eq!(decision.reason, DecisionReason::Affinity);
        assert_eq!(decision.matched_blocks, 8);
        assert!((decision.match_ratio - 0.8).abs() < 1e-9);
    }

    #[test]
    fn affinity_ignores_load_entirely() {
        // The naive policy, behaving naively. Worker 1 is buried and still wins
        // on match length. R0.4 turns this into a measured failure.
        let decision = choose(
            Policy::PrefixAffinity,
            &config(),
            inputs(10, &[2, 8, 0], &[0, 500, 0]),
            0,
        );
        assert_eq!(decision.worker, 1);
        assert_eq!(decision.reason, DecisionReason::Affinity);
    }

    #[test]
    fn a_match_below_the_threshold_is_treated_as_no_match() {
        // One block of ten is a 0.1 ratio against a 0.2 threshold.
        let decision = choose(
            Policy::PrefixAffinity,
            &config(),
            inputs(10, &[1, 0, 0], &[5, 2, 9]),
            0,
        );

        assert_eq!(decision.reason, DecisionReason::CacheMiss);
        assert_eq!(decision.worker, 1, "should fall back to the least loaded");
    }

    #[test]
    fn a_prompt_with_no_blocks_routes_on_load() {
        let decision = choose(
            Policy::PrefixAffinityBalanced,
            &config(),
            inputs(0, &[0, 0], &[3, 1]),
            0,
        );

        assert_eq!(decision.reason, DecisionReason::CacheMiss);
        assert_eq!(decision.worker, 1);
        assert_eq!(decision.match_ratio, 0.0);
    }

    #[test]
    fn balanced_prefers_the_prefix_when_the_fleet_is_even() {
        let decision = choose(
            Policy::PrefixAffinityBalanced,
            &config(),
            inputs(10, &[0, 9, 0], &[2, 2, 2]),
            0,
        );

        assert_eq!(decision.worker, 1);
        assert_eq!(decision.reason, DecisionReason::Affinity);
    }

    #[test]
    fn balanced_overrides_affinity_when_the_fleet_is_lopsided() {
        // Worker 1 holds the whole prefix and is nine deep against zero.
        let decision = choose(
            Policy::PrefixAffinityBalanced,
            &config(),
            inputs(10, &[0, 10, 0], &[0, 9, 0]),
            0,
        );

        assert_eq!(decision.reason, DecisionReason::BalanceOverride);
        assert_ne!(decision.worker, 1);
    }

    #[test]
    fn the_absolute_threshold_stops_a_quiet_fleet_looking_imbalanced() {
        // One in flight against zero is an infinite ratio and means nothing.
        // Affinity should still win.
        let decision = choose(
            Policy::PrefixAffinityBalanced,
            &config(),
            inputs(10, &[0, 10, 0], &[0, 1, 0]),
            0,
        );

        assert_eq!(decision.reason, DecisionReason::Affinity);
        assert_eq!(decision.worker, 1);
    }

    #[test]
    fn the_relative_threshold_stops_a_uniformly_busy_fleet_looking_imbalanced() {
        // Everyone is deep, but evenly so. There is nothing to rebalance to.
        let decision = choose(
            Policy::PrefixAffinityBalanced,
            &config(),
            inputs(10, &[0, 10, 0], &[20, 22, 21]),
            0,
        );

        assert_eq!(decision.reason, DecisionReason::Affinity);
        assert_eq!(decision.worker, 1);
    }

    #[test]
    fn a_large_match_on_a_busy_worker_can_lose_to_a_smaller_one_on_an_idle_worker() {
        // Below the imbalance trigger, so scoring decides. This is the
        // requirement in its own words: a large match on a saturated worker
        // must be able to lose.
        let heavy = AffinityConfig {
            load_weight: 0.8,
            ..config()
        };
        let decision = choose(
            Policy::PrefixAffinityBalanced,
            &heavy,
            inputs(10, &[5, 10, 0], &[0, 4, 0]),
            0,
        );

        assert_ne!(decision.worker, 1, "the buried worker should have lost");
        assert_eq!(decision.worker, 0);
        assert_eq!(decision.reason, DecisionReason::Affinity);
    }

    #[test]
    fn weighting_load_at_zero_makes_balanced_scoring_pick_the_longest_match() {
        let cache_only = AffinityConfig {
            load_weight: 0.0,
            ..config()
        };
        let decision = choose(
            Policy::PrefixAffinityBalanced,
            &cache_only,
            inputs(10, &[5, 10, 0], &[0, 4, 0]),
            0,
        );

        assert_eq!(decision.worker, 1);
    }

    #[test]
    fn ties_on_match_length_go_to_the_less_loaded_worker() {
        let decision = choose(
            Policy::PrefixAffinity,
            &config(),
            inputs(10, &[6, 6, 6], &[3, 1, 8]),
            0,
        );
        assert_eq!(decision.worker, 1);
    }

    #[test]
    fn a_cache_miss_fallback_rotates_rather_than_piling_onto_one_worker() {
        // Every worker equally idle and equally cold. Successive requests must
        // spread, or a cold start would hammer worker zero.
        let chosen: Vec<usize> = (0..6)
            .map(|cursor| {
                choose(
                    Policy::PrefixAffinityBalanced,
                    &config(),
                    inputs(10, &[0, 0, 0], &[0, 0, 0]),
                    cursor,
                )
                .worker
            })
            .collect();

        assert_eq!(chosen, vec![0, 1, 2, 0, 1, 2]);
    }

    #[test]
    fn the_same_inputs_always_produce_the_same_decision() {
        let make = || {
            choose(
                Policy::PrefixAffinityBalanced,
                &config(),
                inputs(10, &[3, 7, 1], &[2, 2, 5]),
                4,
            )
        };
        assert_eq!(make(), make());
    }

    #[test]
    fn a_single_worker_fleet_always_picks_it() {
        for policy in [
            Policy::First,
            Policy::RoundRobin,
            Policy::PrefixAffinity,
            Policy::PrefixAffinityBalanced,
        ] {
            let decision = choose(policy, &config(), inputs(10, &[0], &[99]), 3);
            assert_eq!(
                decision.worker, 0,
                "{policy:?} picked a worker that is not there"
            );
        }
    }

    #[test]
    fn reason_labels_are_stable() {
        assert_eq!(DecisionReason::Affinity.as_str(), "affinity");
        assert_eq!(DecisionReason::BalanceOverride.as_str(), "balance-override");
        assert_eq!(DecisionReason::CacheMiss.as_str(), "cache-miss");
    }
}
