//! Request bodies.
//!
//! Bodies are built before the run starts, so nothing but the send itself
//! happens on the dispatch path. R0.2 generates independent prompts; the
//! prefix-sharing structure that makes cache-aware routing interesting arrives
//! with R0.3, alongside the index that can exploit it.

use bytes::Bytes;
use serde_json::json;

use crate::record::RunConfig;
use crate::schedule::Rng;

/// Vocabulary the generated prompts draw from. Fixed so a seed fully
/// determines the bodies.
const VOCABULARY: &[&str] = &[
    "router",
    "cache",
    "prefix",
    "block",
    "worker",
    "queue",
    "token",
    "latency",
    "replica",
    "prompt",
    "index",
    "hash",
    "policy",
    "affinity",
    "tail",
    "throughput",
    "eviction",
    "session",
    "batch",
    "stream",
];

/// Build one body per request, deterministically from the config's seed.
pub fn build_bodies(config: &RunConfig, count: usize) -> Vec<Bytes> {
    // A generator separate from the schedule's, so changing the arrival rate
    // does not change the prompts and vice versa.
    let mut rng = Rng::new(config.seed ^ 0x5049_4E47_5041_5448);

    (0..count)
        .map(|index| {
            let prompt = prompt_text(&mut rng, config.prompt_words, index);
            let payload = json!({
                "model": config.model,
                "stream": config.stream,
                "max_tokens": config.max_tokens,
                "messages": [{ "role": "user", "content": prompt }],
            });
            Bytes::from(serde_json::to_vec(&payload).expect("request payload should serialize"))
        })
        .collect()
}

/// A prompt of `words` words, ending in the request index so no two requests
/// in a run share a full prompt.
fn prompt_text(rng: &mut Rng, words: usize, index: usize) -> String {
    let mut prompt = String::with_capacity(words * 8 + 16);
    for position in 0..words {
        if position > 0 {
            prompt.push(' ');
        }
        let choice = (rng.next_u64() % VOCABULARY.len() as u64) as usize;
        prompt.push_str(VOCABULARY[choice]);
    }
    prompt.push_str(" #");
    prompt.push_str(&index.to_string());
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Mode;

    fn config(seed: u64) -> RunConfig {
        RunConfig {
            target: "http://127.0.0.1:8080".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            model: "mock-model".to_string(),
            mode: Mode::OpenLoop,
            rate_per_second: 10.0,
            concurrency: 1,
            duration_secs: 1.0,
            warmup_secs: 0.0,
            seed,
            prompt_words: 12,
            max_tokens: 8,
            stream: true,
            max_dispatch_lag_ms: 10.0,
        }
    }

    #[test]
    fn the_same_seed_produces_the_same_bodies() {
        assert_eq!(build_bodies(&config(42), 20), build_bodies(&config(42), 20));
    }

    #[test]
    fn a_different_seed_produces_different_bodies() {
        assert_ne!(build_bodies(&config(42), 20), build_bodies(&config(43), 20));
    }

    #[test]
    fn every_body_is_distinct_and_well_formed() {
        let bodies = build_bodies(&config(7), 50);
        assert_eq!(bodies.len(), 50);

        let mut seen = std::collections::HashSet::new();
        for (index, body) in bodies.iter().enumerate() {
            assert!(seen.insert(body.clone()), "body {index} was a duplicate");

            let parsed: serde_json::Value =
                serde_json::from_slice(body).expect("body should be valid JSON");
            assert_eq!(parsed["model"], "mock-model");
            assert_eq!(parsed["max_tokens"], 8);
            assert_eq!(parsed["stream"], true);

            let content = parsed["messages"][0]["content"]
                .as_str()
                .expect("prompt should be a string");
            assert!(
                content.ends_with(&format!(" #{index}")),
                "prompt did not end with its index: {content}"
            );
            assert_eq!(content.split_whitespace().count(), 13);
        }
    }
}
