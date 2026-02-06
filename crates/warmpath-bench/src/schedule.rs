//! Arrival schedules and the seeded generator behind them.
//!
//! The schedule is computed in full before the first request is sent. That is
//! the whole open-loop guarantee: request *i* is due at a time fixed before the
//! run started, so a slow response cannot push later arrivals out and hide the
//! queueing it caused.

use std::time::Duration;

/// Deterministic pseudo-random generator.
///
/// xorshift64*: small, fast, and good enough for arrival times. It is here
/// rather than a dependency because a benchmark that cannot reproduce its own
/// schedule from a seed is not reproducible, and pinning that behaviour to this
/// file makes it easy to check.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Seed the generator. Zero is remapped, because xorshift is stuck at zero.
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in the open interval (0, 1).
    ///
    /// Zero is excluded because the exponential draw takes a logarithm.
    pub fn next_f64(&mut self) -> f64 {
        // 53 bits gives a value in [0, 1); shifting up by one ulp opens the
        // lower bound without touching the upper one.
        let bits = self.next_u64() >> 11;
        let unit = bits as f64 / (1u64 << 53) as f64;
        if unit <= 0.0 {
            f64::MIN_POSITIVE
        } else {
            unit
        }
    }

    /// Draw from an exponential distribution with the given rate, by inverse
    /// transform sampling.
    pub fn exponential(&mut self, rate: f64) -> f64 {
        -self.next_f64().ln() / rate
    }
}

/// Offsets from the start of a run at which requests are due.
///
/// Inter-arrival times are exponential, which makes arrivals a Poisson process
/// at `rate_per_second`. Poisson rather than uniform spacing because real
/// traffic clusters, and clustering is what produces the queueing that tail
/// latency is made of.
pub fn poisson_schedule(rate_per_second: f64, duration: Duration, rng: &mut Rng) -> Vec<Duration> {
    assert!(
        rate_per_second > 0.0 && rate_per_second.is_finite(),
        "arrival rate must be positive and finite, got {rate_per_second}"
    );

    let horizon = duration.as_secs_f64();
    let mut offsets = Vec::with_capacity((rate_per_second * horizon) as usize + 1);
    let mut elapsed = 0.0_f64;

    loop {
        elapsed += rng.exponential(rate_per_second);
        if elapsed >= horizon {
            return offsets;
        }
        offsets.push(Duration::from_secs_f64(elapsed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_gives_the_same_schedule() {
        let a = poisson_schedule(50.0, Duration::from_secs(5), &mut Rng::new(7));
        let b = poisson_schedule(50.0, Duration::from_secs(5), &mut Rng::new(7));

        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn different_seeds_give_different_schedules() {
        let a = poisson_schedule(50.0, Duration::from_secs(5), &mut Rng::new(7));
        let b = poisson_schedule(50.0, Duration::from_secs(5), &mut Rng::new(8));

        assert_ne!(a, b);
    }

    #[test]
    fn offsets_are_non_decreasing_and_inside_the_horizon() {
        let duration = Duration::from_secs(3);
        let schedule = poisson_schedule(200.0, duration, &mut Rng::new(1234));

        assert!(!schedule.is_empty());
        for pair in schedule.windows(2) {
            assert!(pair[0] <= pair[1], "schedule went backwards: {pair:?}");
        }
        assert!(
            *schedule.last().expect("non-empty") < duration,
            "an offset landed past the horizon"
        );
    }

    #[test]
    fn arrival_count_approaches_rate_times_duration() {
        // A Poisson process over 60s at 100/s has mean 6000 arrivals and
        // standard deviation sqrt(6000) ≈ 77. Ten standard deviations is a
        // wide enough band that this cannot flake, and tight enough that a
        // wrong rate would fail it.
        let schedule = poisson_schedule(100.0, Duration::from_secs(60), &mut Rng::new(99));

        let expected = 6000.0_f64;
        let tolerance = 10.0 * expected.sqrt();
        let observed = schedule.len() as f64;

        assert!(
            (observed - expected).abs() < tolerance,
            "expected about {expected} arrivals, got {observed}"
        );
    }

    #[test]
    fn mean_inter_arrival_time_matches_the_rate() {
        let rate = 250.0;
        let schedule = poisson_schedule(rate, Duration::from_secs(40), &mut Rng::new(2024));

        let total: f64 = schedule
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).as_secs_f64())
            .sum();
        let mean = total / (schedule.len() - 1) as f64;

        assert!(
            (mean - 1.0 / rate).abs() < 0.1 / rate,
            "mean interval {mean} is far from {}",
            1.0 / rate
        );
    }

    #[test]
    fn uniform_draws_stay_inside_the_open_unit_interval() {
        let mut rng = Rng::new(5);
        for _ in 0..100_000 {
            let value = rng.next_f64();
            assert!(value > 0.0 && value < 1.0, "draw out of range: {value}");
        }
    }

    #[test]
    fn a_zero_seed_still_produces_a_usable_stream() {
        let mut rng = Rng::new(0);
        let first = rng.next_u64();
        let second = rng.next_u64();

        assert_ne!(first, 0);
        assert_ne!(first, second);
    }

    #[test]
    fn exponential_draws_have_the_expected_mean() {
        let mut rng = Rng::new(31337);
        let rate = 4.0;
        let samples = 200_000;
        let mean: f64 = (0..samples).map(|_| rng.exponential(rate)).sum::<f64>() / samples as f64;

        assert!(
            (mean - 1.0 / rate).abs() < 0.01,
            "mean {mean} is far from {}",
            1.0 / rate
        );
    }
}
