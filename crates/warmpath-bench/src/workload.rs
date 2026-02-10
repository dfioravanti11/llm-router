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
///
/// When the config asks for shared prefixes, a pool of them is generated once
/// and each request draws one, carried as a system message ahead of its own
/// varying question. That shape is what makes prefix caching worth anything:
/// the leading blocks repeat across requests while the tail does not.
pub fn build_bodies(config: &RunConfig, count: usize) -> Vec<Bytes> {
    // A generator separate from the schedule's, so changing the arrival rate
    // does not change the prompts and vice versa.
    let mut rng = Rng::new(config.seed ^ 0x5049_4E47_5041_5448);

    let prefixes: Vec<String> = if config.shared_prefix_words > 0 && config.prefix_pool > 0 {
        (0..config.prefix_pool)
            .map(|_| words(&mut rng, config.shared_prefix_words))
            .collect()
    } else {
        Vec::new()
    };

    (0..count)
        .map(|index| {
            let question = prompt_text(&mut rng, config.prompt_words, index);

            let messages = if prefixes.is_empty() {
                json!([{ "role": "user", "content": question }])
            } else {
                let choice = (rng.next_u64() % prefixes.len() as u64) as usize;
                json!([
                    { "role": "system", "content": prefixes[choice] },
                    { "role": "user", "content": question },
                ])
            };

            let payload = json!({
                "model": config.model,
                "stream": config.stream,
                "max_tokens": config.max_tokens,
                "messages": messages,
            });
            Bytes::from(serde_json::to_vec(&payload).expect("request payload should serialize"))
        })
        .collect()
}

/// `count` words drawn from the vocabulary.
fn words(rng: &mut Rng, count: usize) -> String {
    let mut text = String::with_capacity(count * 8);
    for position in 0..count {
        if position > 0 {
            text.push(' ');
        }
        let choice = (rng.next_u64() % VOCABULARY.len() as u64) as usize;
        text.push_str(VOCABULARY[choice]);
    }
    text
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
            label: String::new(),
            prompt_words: 12,
            shared_prefix_words: 0,
            prefix_pool: 0,
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
    fn requests_share_a_prefix_when_the_config_asks_for_one() {
        let mut config = config(5);
        config.shared_prefix_words = 64;
        config.prefix_pool = 2;

        let bodies = build_bodies(&config, 40);
        let mut prefixes = std::collections::HashSet::new();

        for body in &bodies {
            let parsed: serde_json::Value = serde_json::from_slice(body).expect("valid JSON");
            let messages = parsed["messages"].as_array().expect("messages");
            assert_eq!(messages.len(), 2);
            assert_eq!(messages[0]["role"], "system");

            let prefix = messages[0]["content"].as_str().expect("prefix text");
            assert_eq!(prefix.split_whitespace().count(), 64);
            prefixes.insert(prefix.to_string());
        }

        assert_eq!(
            prefixes.len(),
            2,
            "forty requests should have drawn from a pool of exactly two prefixes"
        );
    }

    #[test]
    fn a_zero_length_prefix_leaves_prompts_independent() {
        let mut config = config(5);
        config.shared_prefix_words = 0;
        config.prefix_pool = 4;

        for body in build_bodies(&config, 10) {
            let parsed: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
            assert_eq!(parsed["messages"].as_array().expect("messages").len(), 1);
        }
    }

    #[test]
    fn the_prefix_pool_is_reproducible_from_the_seed() {
        let mut config = config(11);
        config.shared_prefix_words = 32;
        config.prefix_pool = 3;

        assert_eq!(build_bodies(&config, 25), build_bodies(&config, 25));
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
