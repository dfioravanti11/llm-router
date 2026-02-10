//! A block-level prefix cache, simulated.
//!
//! This is what makes cache-aware routing observable without a GPU. A request
//! whose prefix is already here skips the prefill for those blocks, so its time
//! to first token drops. Route the same prefix back to the same worker and the
//! difference shows up in the latency the benchmark measures.
//!
//! # Deliberately not shared with the router's index
//!
//! The router's block index models this behaviour, and it would be easy to have
//! both call the same code. That would be a mistake. The index exists to
//! predict what a worker's cache is doing; if the prediction and the thing
//! being predicted are the same function, the prediction is trivially perfect
//! and proves nothing.
//!
//! Keeping them apart means a disagreement is visible. It also means the
//! predicted-versus-actual hit rate comparison has something to compare, which
//! is the check that stops the router from grading its own homework. That check
//! only becomes real evidence at R0.5 against vLLM, where the worker's cache is
//! not a model at all.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use warmpath_core::BlockHash;

/// Counters mirroring the ones vLLM exports, in blocks rather than requests.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct CacheStats {
    /// Blocks looked up, whether or not they were present.
    pub prefix_cache_queries: u64,
    /// Blocks that were present.
    pub prefix_cache_hits: u64,
    /// Blocks held right now.
    pub cached_blocks: usize,
    /// Blocks dropped to stay inside capacity.
    pub evicted_blocks: u64,
}

impl CacheStats {
    pub fn hit_rate(&self) -> f64 {
        if self.prefix_cache_queries == 0 {
            0.0
        } else {
            self.prefix_cache_hits as f64 / self.prefix_cache_queries as f64
        }
    }
}

#[derive(Debug)]
struct Node {
    clock: u64,
    parent: Option<BlockHash>,
    children: usize,
}

/// Blocks held by one worker, evicted leaf-first and least-recently-used.
///
/// Leaf-first because a block with a cached continuation cannot be dropped
/// without stranding it: prefix lookup walks from the head and stops at the
/// first gap, so evicting a block in the middle silently orphans everything
/// behind it.
#[derive(Debug)]
pub struct PrefixCache {
    capacity_blocks: usize,
    entries: HashMap<BlockHash, Node>,
    leaves: BTreeMap<u64, BlockHash>,
    clock: u64,
    stats: CacheStats,
}

impl PrefixCache {
    pub fn new(capacity_blocks: usize) -> Self {
        Self {
            capacity_blocks,
            entries: HashMap::new(),
            leaves: BTreeMap::new(),
            clock: 0,
            stats: CacheStats::default(),
        }
    }

    /// Leading blocks of `chain` already held.
    pub fn cached_prefix(&self, chain: &[BlockHash]) -> usize {
        chain
            .iter()
            .take_while(|hash| self.entries.contains_key(hash))
            .count()
    }

    /// Serve a request: report how much of its prefix was already cached, then
    /// hold the whole chain.
    ///
    /// Returns the number of leading blocks that were hits, which is what the
    /// worker gets to skip prefilling.
    pub fn admit(&mut self, chain: &[BlockHash]) -> usize {
        if self.capacity_blocks == 0 || chain.is_empty() {
            return 0;
        }

        let hits = self.cached_prefix(chain);
        self.stats.prefix_cache_queries += chain.len() as u64;
        self.stats.prefix_cache_hits += hits as u64;

        let mut parent = None;
        for hash in chain {
            self.touch(*hash, parent);
            parent = Some(*hash);
        }
        self.evict_to_capacity();

        self.stats.cached_blocks = self.entries.len();
        hits
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            cached_blocks: self.entries.len(),
            ..self.stats
        }
    }

    fn touch(&mut self, hash: BlockHash, parent: Option<BlockHash>) {
        self.clock += 1;
        let clock = self.clock;

        match self.entries.get_mut(&hash) {
            Some(node) => {
                let previous = node.clock;
                node.clock = clock;
                if node.children == 0 {
                    self.leaves.remove(&previous);
                    self.leaves.insert(clock, hash);
                }
            }
            None => {
                self.entries.insert(
                    hash,
                    Node {
                        clock,
                        parent,
                        children: 0,
                    },
                );
                self.leaves.insert(clock, hash);

                if let Some(parent) = parent {
                    if let Some(node) = self.entries.get_mut(&parent) {
                        node.children += 1;
                        if node.children == 1 {
                            self.leaves.remove(&node.clock);
                        }
                    }
                }
            }
        }
    }

    fn evict_to_capacity(&mut self) {
        while self.entries.len() > self.capacity_blocks {
            let Some((clock, hash)) = self.leaves.iter().next().map(|(k, v)| (*k, *v)) else {
                break;
            };
            self.leaves.remove(&clock);

            let Some(node) = self.entries.remove(&hash) else {
                break;
            };
            if let Some(parent) = node.parent {
                if let Some(parent_node) = self.entries.get_mut(&parent) {
                    parent_node.children -= 1;
                    if parent_node.children == 0 {
                        self.leaves.insert(parent_node.clock, parent);
                    }
                }
            }
            self.stats.evicted_blocks += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(seed: u64, len: usize) -> Vec<BlockHash> {
        (0..len)
            .map(|i| BlockHash(seed * 1_000 + i as u64))
            .collect()
    }

    #[test]
    fn a_cold_cache_hits_nothing() {
        let mut cache = PrefixCache::new(100);
        assert_eq!(cache.admit(&chain(1, 8)), 0);
        assert_eq!(cache.stats().prefix_cache_hits, 0);
        assert_eq!(cache.stats().prefix_cache_queries, 8);
    }

    #[test]
    fn repeating_a_prompt_hits_in_full() {
        let mut cache = PrefixCache::new(100);
        let blocks = chain(1, 8);

        cache.admit(&blocks);
        assert_eq!(cache.admit(&blocks), 8);

        let stats = cache.stats();
        assert_eq!(stats.prefix_cache_queries, 16);
        assert_eq!(stats.prefix_cache_hits, 8);
        assert!((stats.hit_rate() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_shared_prefix_hits_up_to_the_point_it_diverges() {
        let mut cache = PrefixCache::new(100);
        let mut first = chain(1, 4);
        first.extend(chain(2, 4));
        let mut second = chain(1, 4);
        second.extend(chain(3, 4));

        cache.admit(&first);
        assert_eq!(cache.admit(&second), 4);
    }

    #[test]
    fn an_unrelated_prompt_hits_nothing() {
        let mut cache = PrefixCache::new(100);
        cache.admit(&chain(1, 8));
        assert_eq!(cache.admit(&chain(2, 8)), 0);
    }

    #[test]
    fn a_disabled_cache_never_hits() {
        let mut cache = PrefixCache::new(0);
        let blocks = chain(1, 8);

        cache.admit(&blocks);
        assert_eq!(cache.admit(&blocks), 0);
        assert_eq!(cache.stats().cached_blocks, 0);
    }

    #[test]
    fn capacity_is_respected() {
        let mut cache = PrefixCache::new(10);
        cache.admit(&chain(1, 8));
        cache.admit(&chain(2, 8));

        assert_eq!(cache.stats().cached_blocks, 10);
        assert_eq!(cache.stats().evicted_blocks, 6);
    }

    #[test]
    fn eviction_takes_the_tail_so_a_shared_head_keeps_hitting() {
        let mut cache = PrefixCache::new(6);
        let shared = chain(1, 4);

        cache.admit(&shared);
        cache.admit(&chain(2, 4));

        // Two blocks had to go, and they came off the older chain's tail.
        // Losing its head instead would drop the hit rate to zero.
        assert_eq!(cache.cached_prefix(&shared), 2);
    }

    #[test]
    fn a_prompt_larger_than_the_cache_keeps_its_head() {
        let mut cache = PrefixCache::new(3);
        let blocks = chain(1, 10);

        cache.admit(&blocks);

        assert_eq!(cache.stats().cached_blocks, 3);
        assert_eq!(cache.cached_prefix(&blocks), 3);
    }

    #[test]
    fn an_empty_chain_is_not_counted_as_a_query() {
        let mut cache = PrefixCache::new(100);
        assert_eq!(cache.admit(&[]), 0);
        assert_eq!(cache.stats().prefix_cache_queries, 0);
    }
}
