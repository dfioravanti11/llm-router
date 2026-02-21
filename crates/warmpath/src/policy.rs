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
    /// Each worker's queue depth, as the worker reports it: requests it has
    /// admitted and not finished, running and waiting together.
    ///
    /// This is the worker's own view rather than the router's in-flight count,
    /// so it includes work the engine has queued internally and survives the
    /// router restarting.
    pub load: &'a [usize],
    /// Fraction of each worker's KV cache in use, between zero and one.
    ///
    /// Queue depth alone cannot express memory pressure. A worker holding a
    /// request's whole prefix but with no KV headroom should be able to lose to
    /// one with room, and this is the number that lets it.
    pub kv_utilization: &'a [f64],
    /// Whether each worker is currently answering. Unhealthy workers are never
    /// chosen unless every worker is unhealthy.
    pub healthy: &'a [bool],
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
    /// Fewest requests queued at the worker.
    LeastLoaded,
    /// The less loaded of two candidates.
    PowerOfTwo,
    /// The session's usual worker was reused.
    Session,
    /// A second attempt, after the first worker never answered.
    Retry,
    /// Every worker was failing health checks, so one was chosen anyway. A
    /// request refused is worse than a request sent somewhere doubtful.
    NoHealthyWorker,
}

impl DecisionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionReason::First => "first",
            DecisionReason::RoundRobin => "round-robin",
            DecisionReason::Affinity => "affinity",
            DecisionReason::BalanceOverride => "balance-override",
            DecisionReason::CacheMiss => "cache-miss",
            DecisionReason::LeastLoaded => "least-loaded",
            DecisionReason::PowerOfTwo => "power-of-two",
            DecisionReason::Session => "session",
            DecisionReason::Retry => "retry",
            DecisionReason::NoHealthyWorker => "no-healthy-worker",
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

    // A fleet with nothing healthy still has to serve traffic. Refusing would
    // turn a monitoring blip into an outage, and the health signal is a poll
    // result rather than a certainty.
    if !inputs.healthy.iter().any(|healthy| *healthy) {
        return decide(
            cursor % worker_count,
            DecisionReason::NoHealthyWorker,
            inputs,
        );
    }

    match policy {
        Policy::First => decide(first_healthy(inputs), DecisionReason::First, inputs),
        Policy::RoundRobin => decide(
            round_robin(inputs, cursor),
            DecisionReason::RoundRobin,
            inputs,
        ),
        Policy::LeastLoaded => decide(
            least_loaded(inputs, cursor),
            DecisionReason::LeastLoaded,
            inputs,
        ),
        Policy::PowerOfTwo => decide(
            power_of_two(inputs, cursor),
            DecisionReason::PowerOfTwo,
            inputs,
        ),
        Policy::PrefixAffinity => prefix_affinity(config, inputs, cursor),
        Policy::PrefixAffinityBalanced => balanced(config, inputs, cursor),
    }
}

/// The lowest-numbered healthy worker.
fn first_healthy(inputs: RoutingInputs<'_>) -> usize {
    (0..inputs.load.len())
        .find(|worker| inputs.healthy[*worker])
        .unwrap_or(0)
}

/// Even rotation, skipping workers that are not answering.
fn round_robin(inputs: RoutingInputs<'_>, cursor: usize) -> usize {
    let worker_count = inputs.load.len();
    (0..worker_count)
        .map(|offset| (cursor + offset) % worker_count)
        .find(|worker| inputs.healthy[*worker])
        .unwrap_or(cursor % worker_count)
}

/// Two candidates, the less loaded one wins.
///
/// Scanning the whole fleet is cheap at this size, so speed is not the reason
/// this exists. It is that power-of-two-choices is what a reader expects in the
/// baseline field, and omitting it would make the comparison look like it
/// avoided a strong baseline.
fn power_of_two(inputs: RoutingInputs<'_>, cursor: usize) -> usize {
    let worker_count = inputs.load.len();
    if worker_count == 1 {
        return 0;
    }

    // Both candidates come from the cursor rather than a random source, so a
    // run reproduces exactly.
    let first = cursor % worker_count;
    let mut second = (cursor / worker_count + 1) % worker_count;
    if second == first {
        second = (first + 1) % worker_count;
    }

    [first, second]
        .into_iter()
        .filter(|worker| inputs.healthy[*worker])
        .min_by_key(|worker| inputs.load[*worker])
        .unwrap_or_else(|| round_robin(inputs, cursor))
}

/// Longest match wins, and nothing else gets a vote.
///
/// This is the naive policy on purpose. It is the one that hotspots when a
/// prefix gets hot, and the skewed workload exists to show it losing to the
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
    let healthy_loads = || {
        inputs
            .load
            .iter()
            .enumerate()
            .filter(|(worker, _)| inputs.healthy[*worker])
            .map(|(_, load)| *load)
    };
    let max_load = healthy_loads().max().unwrap_or(0);
    let min_load = healthy_loads().min().unwrap_or(0);

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
    let mut chosen = first_healthy(inputs);
    let mut best_score = f64::NEG_INFINITY;
    for worker in 0..inputs.load.len() {
        if !inputs.healthy[worker] {
            continue;
        }
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

    // Queue headroom is relative to the busiest worker, so the scale adapts to
    // the fleet instead of needing an absolute capacity the router cannot know.
    let queue_headroom = 1.0 - (inputs.load[worker] as f64 / (max_load as f64 + 1.0));
    // Memory headroom is absolute, because the worker reports utilization as a
    // fraction of its own capacity. A worker at 95% KV has almost no room for
    // another sequence however short its queue happens to look.
    let memory_headroom = 1.0 - inputs.kv_utilization[worker].clamp(0.0, 1.0);
    let headroom = queue_headroom.min(memory_headroom);

    (1.0 - config.load_weight) * ratio + config.load_weight * headroom
}

struct BestMatch {
    worker: usize,
    matched: usize,
}

/// The worker holding the most of this prompt's prefix, ties going to the
/// less loaded one and then to the cursor.
fn best_match(inputs: RoutingInputs<'_>) -> BestMatch {
    let mut worker = (0..inputs.load.len())
        .find(|worker| inputs.healthy[*worker])
        .unwrap_or(0);
    let mut matched = inputs.matched[worker];

    for candidate in 0..inputs.matched.len() {
        if !inputs.healthy[candidate] {
            continue;
        }
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
/// always landing on the lowest index. Unhealthy workers are skipped.
fn least_loaded(inputs: RoutingInputs<'_>, cursor: usize) -> usize {
    let worker_count = inputs.load.len();
    let start = round_robin(inputs, cursor);
    let mut chosen = start;
    let mut lowest = inputs.load[chosen];

    for offset in 1..worker_count {
        let candidate = (cursor + offset) % worker_count;
        if inputs.healthy[candidate] && inputs.load[candidate] < lowest {
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

    /// Every worker healthy and no memory pressure, which is the condition
    /// most of these tests want to hold constant while varying one thing.
    const IDLE_KV: [f64; 8] = [0.0; 8];
    const ALL_HEALTHY: [bool; 8] = [true; 8];

    fn inputs<'a>(
        prompt_blocks: usize,
        matched: &'a [usize],
        load: &'a [usize],
    ) -> RoutingInputs<'a> {
        RoutingInputs {
            prompt_blocks,
            matched,
            load,
            kv_utilization: &IDLE_KV[..load.len()],
            healthy: &ALL_HEALTHY[..load.len()],
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
            Policy::LeastLoaded,
            Policy::PowerOfTwo,
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

    fn inputs_with<'a>(
        prompt_blocks: usize,
        matched: &'a [usize],
        load: &'a [usize],
        kv_utilization: &'a [f64],
        healthy: &'a [bool],
    ) -> RoutingInputs<'a> {
        RoutingInputs {
            prompt_blocks,
            matched,
            load,
            kv_utilization,
            healthy,
        }
    }

    #[test]
    fn least_loaded_picks_the_shortest_queue() {
        let decision = choose(
            Policy::LeastLoaded,
            &config(),
            inputs(10, &[10, 0, 0], &[5, 9, 1]),
            0,
        );

        assert_eq!(decision.worker, 2);
        assert_eq!(decision.reason, DecisionReason::LeastLoaded);
    }

    #[test]
    fn power_of_two_picks_the_lighter_of_its_two_candidates() {
        // Whichever pair the cursor selects, the chosen worker is never the
        // most loaded one, which is the property the policy promises.
        for cursor in 0..12 {
            let decision = choose(
                Policy::PowerOfTwo,
                &config(),
                inputs(10, &[0, 0, 0, 0], &[1, 9, 2, 3]),
                cursor,
            );
            assert_ne!(decision.worker, 1, "cursor {cursor} chose the busiest");
            assert_eq!(decision.reason, DecisionReason::PowerOfTwo);
        }
    }

    #[test]
    fn power_of_two_spreads_across_the_fleet() {
        let chosen: std::collections::HashSet<usize> = (0..24)
            .map(|cursor| {
                choose(
                    Policy::PowerOfTwo,
                    &config(),
                    inputs(10, &[0, 0, 0, 0], &[0, 0, 0, 0]),
                    cursor,
                )
                .worker
            })
            .collect();

        assert!(chosen.len() > 1, "every request went to the same worker");
    }

    #[test]
    fn an_unhealthy_worker_is_never_chosen() {
        let healthy = [true, false, true];
        let kv = [0.0, 0.0, 0.0];

        for policy in [
            Policy::First,
            Policy::RoundRobin,
            Policy::LeastLoaded,
            Policy::PowerOfTwo,
            Policy::PrefixAffinity,
            Policy::PrefixAffinityBalanced,
        ] {
            for cursor in 0..6 {
                // Worker 1 holds the whole prefix and has the shortest queue,
                // so every policy would want it if it were answering.
                let decision = choose(
                    policy,
                    &config(),
                    inputs_with(10, &[0, 10, 0], &[4, 0, 4], &kv, &healthy),
                    cursor,
                );
                assert_ne!(
                    decision.worker, 1,
                    "{policy:?} routed to an ejected worker at cursor {cursor}"
                );
            }
        }
    }

    #[test]
    fn a_fleet_with_nothing_healthy_still_serves_somewhere() {
        // Refusing would turn a monitoring blip into an outage. The reason is
        // recorded so the decision is visible rather than silent.
        let healthy = [false, false];
        let kv = [0.0, 0.0];

        let decision = choose(
            Policy::PrefixAffinityBalanced,
            &config(),
            inputs_with(10, &[0, 10], &[0, 0], &kv, &healthy),
            1,
        );

        assert_eq!(decision.reason, DecisionReason::NoHealthyWorker);
        assert!(decision.worker < 2);
    }

    #[test]
    fn a_worker_out_of_kv_headroom_loses_despite_holding_the_prefix() {
        // Both queues are empty, so queue depth cannot decide this and the only
        // thing separating the two workers is memory pressure. That is the
        // signal queue depth alone could never express, and the reason worker
        // state is polled at all.
        let heavy = AffinityConfig {
            load_weight: 0.6,
            ..config()
        };
        let kv = [0.05, 0.98];
        let healthy = [true, true];

        let decision = choose(
            Policy::PrefixAffinityBalanced,
            &heavy,
            inputs_with(10, &[4, 10], &[0, 0], &kv, &healthy),
            0,
        );

        assert_eq!(
            decision.worker, 0,
            "the worker at 98% KV should have lost the tie"
        );
    }

    #[test]
    fn kv_headroom_does_not_override_a_healthy_match() {
        // The same fleet with room to spare picks the longer match.
        let kv = [0.05, 0.10];
        let healthy = [true, true];

        let decision = choose(
            Policy::PrefixAffinityBalanced,
            &config(),
            inputs_with(10, &[4, 10], &[0, 0], &kv, &healthy),
            0,
        );

        assert_eq!(decision.worker, 1);
        assert_eq!(decision.reason, DecisionReason::Affinity);
    }

    #[test]
    fn reason_labels_are_stable() {
        assert_eq!(DecisionReason::Affinity.as_str(), "affinity");
        assert_eq!(DecisionReason::BalanceOverride.as_str(), "balance-override");
        assert_eq!(DecisionReason::CacheMiss.as_str(), "cache-miss");
        assert_eq!(DecisionReason::LeastLoaded.as_str(), "least-loaded");
        assert_eq!(DecisionReason::PowerOfTwo.as_str(), "power-of-two");
        assert_eq!(DecisionReason::Session.as_str(), "session");
        assert_eq!(DecisionReason::Retry.as_str(), "retry");
        assert_eq!(
            DecisionReason::NoHealthyWorker.as_str(),
            "no-healthy-worker"
        );
    }
}
