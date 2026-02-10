//! Block hash chains.
//!
//! A prompt is cut into fixed-size blocks of token ids, and each block gets a
//! hash computed from its parent's hash and its own tokens. Chaining is what
//! makes the hash identify a *prefix* rather than a block: two prompts share
//! block hash *i* only if they agree on every token up to the end of block *i*.
//!
//! That property is doing real work downstream. It lets the block index be a
//! flat map from hash to worker rather than a tree, and it lets a cache hit be
//! decided one block at a time without ever comparing token sequences.
//!
//! # What this is not
//!
//! These hashes are internally consistent, not byte-compatible with vLLM's.
//! Routing only needs requests to be comparable with each other, so the router
//! works correctly either way. Compatibility matters for one thing: checking
//! the router's predicted hit rate against vLLM's own
//! `prefix_cache_queries` / `prefix_cache_hits` counters. That check happens at
//! R0.5 against a running vLLM, which is the first point where the real
//! algorithm can be confirmed rather than guessed, and the hash function here
//! is expected to be replaced then.

use serde::{Deserialize, Serialize};
use xxhash_rust::xxh64::Xxh64;

/// Token ids per block. Matches vLLM's default.
pub const DEFAULT_BLOCK_SIZE: usize = 16;

/// Seed for the block hash. Fixed so a hash is reproducible across processes
/// and across runs, which the router and the mock worker both depend on.
const HASH_SEED: u64 = 0x7761_726d_7061_7468;

/// Hash of one block, and of every token before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BlockHash(pub u64);

impl std::fmt::Display for BlockHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// Cut a token sequence into blocks and hash the chain.
///
/// Only whole blocks are hashed. A trailing partial block is dropped, because a
/// worker cannot serve a cache hit on a block it has not finished filling, so
/// counting it would inflate every match.
pub fn hash_chain(token_ids: &[u32], block_size: usize) -> Vec<BlockHash> {
    assert!(block_size > 0, "block size must be positive");

    let block_count = token_ids.len() / block_size;
    let mut hashes = Vec::with_capacity(block_count);
    let mut parent: Option<BlockHash> = None;

    for index in 0..block_count {
        let start = index * block_size;
        let block = &token_ids[start..start + block_size];
        let hash = hash_block(parent, block);
        hashes.push(hash);
        parent = Some(hash);
    }

    hashes
}

/// Hash one block against its parent.
fn hash_block(parent: Option<BlockHash>, token_ids: &[u32]) -> BlockHash {
    let mut hasher = Xxh64::new(HASH_SEED);

    // The root is distinguished from a parent that happens to hash to zero, so
    // a prompt cannot be confused with a suffix of another prompt.
    match parent {
        Some(BlockHash(value)) => {
            hasher.update(&[1u8]);
            hasher.update(&value.to_le_bytes());
        }
        None => hasher.update(&[0u8]),
    }

    for token in token_ids {
        hasher.update(&token.to_le_bytes());
    }

    BlockHash(hasher.digest())
}

/// How many leading blocks two chains share.
pub fn shared_prefix_len(left: &[BlockHash], right: &[BlockHash]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(count: usize) -> Vec<u32> {
        (0..count as u32).collect()
    }

    #[test]
    fn a_short_prompt_produces_no_blocks() {
        assert!(hash_chain(&tokens(15), 16).is_empty());
        assert!(hash_chain(&[], 16).is_empty());
    }

    #[test]
    fn a_trailing_partial_block_is_dropped() {
        // 40 tokens at block size 16 is two whole blocks and a partial one.
        assert_eq!(hash_chain(&tokens(40), 16).len(), 2);
        assert_eq!(hash_chain(&tokens(32), 16).len(), 2);
    }

    #[test]
    fn hashing_is_deterministic() {
        assert_eq!(hash_chain(&tokens(64), 16), hash_chain(&tokens(64), 16));
    }

    #[test]
    fn a_shared_prefix_produces_shared_block_hashes() {
        let mut shared = tokens(64);
        let mut divergent = shared.clone();

        // Diverge inside the third block, so the first two chain hashes match
        // and everything after differs.
        shared[35] = 900;
        divergent[35] = 901;

        let left = hash_chain(&shared, 16);
        let right = hash_chain(&divergent, 16);

        assert_eq!(left.len(), 4);
        assert_eq!(shared_prefix_len(&left, &right), 2);
        assert_ne!(left[2], right[2]);
        assert_ne!(left[3], right[3]);
    }

    #[test]
    fn a_change_in_the_first_block_invalidates_the_whole_chain() {
        let mut changed = tokens(64);
        changed[0] = 999;

        let original = hash_chain(&tokens(64), 16);
        let changed = hash_chain(&changed, 16);

        assert_eq!(shared_prefix_len(&original, &changed), 0);
    }

    #[test]
    fn a_prompt_is_not_confused_with_a_suffix_of_another() {
        // Blocks two onward of the longer prompt carry the same tokens as
        // blocks one onward of the shorter one. Chaining has to keep them
        // apart, or a request could claim a cache hit on a prefix it never
        // sent.
        let long = hash_chain(&tokens(64), 16);
        let short = hash_chain(&tokens(64)[16..], 16);

        assert_eq!(short.len(), 3);
        assert_ne!(long[1], short[0]);
        assert_eq!(shared_prefix_len(&long[1..], &short), 0);
    }

    #[test]
    fn extending_a_prompt_preserves_the_existing_chain() {
        // The property multi-turn conversations depend on: turn two keeps every
        // block hash from turn one.
        let first_turn = hash_chain(&tokens(32), 16);
        let second_turn = hash_chain(&tokens(80), 16);

        assert_eq!(first_turn.len(), 2);
        assert_eq!(second_turn.len(), 5);
        assert_eq!(&second_turn[..2], &first_turn[..]);
    }

    #[test]
    fn block_size_changes_the_chain() {
        assert_ne!(hash_chain(&tokens(64), 16), hash_chain(&tokens(64), 32));
    }

    #[test]
    fn shared_prefix_len_handles_the_edges() {
        let chain = hash_chain(&tokens(64), 16);

        assert_eq!(shared_prefix_len(&chain, &chain), 4);
        assert_eq!(shared_prefix_len(&chain, &[]), 0);
        assert_eq!(shared_prefix_len(&[], &chain), 0);
        assert_eq!(shared_prefix_len(&chain, &chain[..2]), 2);
    }

    #[test]
    fn block_hashes_render_as_fixed_width_hex() {
        assert_eq!(BlockHash(0xdead_beef).to_string(), "00000000deadbeef");
    }
}
