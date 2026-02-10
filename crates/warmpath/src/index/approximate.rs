//! The approximate backend: cache state inferred from the router's own
//! dispatches, with modelled eviction.
//!
//! # Why a flat map and not a radix tree
//!
//! The obvious structure is a radix tree over block-hash sequences, and that is
//! what the design notes call for. It turns out not to be needed, because the
//! hash chain already does the tree's job: block hash *i* is computed from its
//! parent, so it encodes every token before it. Two prompts share hash *i* only
//! if they agree on the whole prefix.
//!
//! So a flat map from hash to owners answers prefix queries exactly as a tree
//! would, in one lookup per block, with far less to get wrong. The tree
//! structure buys something a flat map cannot give: knowing which blocks
//! descend from which. Reuse-aware eviction would want that (Appendix A3), and
//! this module can grow it then, measured against LRU rather than assumed
//! better.
//!
//! # Modelled eviction
//!
//! Without eviction the index drifts into overconfidence: it keeps claiming a
//! worker holds blocks that worker dropped hours ago, and every routing
//! decision built on that is wrong in the same direction. Each worker gets a
//! block budget and least-recently-used eviction, which is a model of what the
//! engine does, not a report of it. Being approximately right beats being
//! confidently stale.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use warmpath_core::BlockHash;

use super::{BlockIndex, IndexStats, MAX_WORKERS};

/// Bitset of worker indices.
type WorkerSet = u64;

#[derive(Debug)]
pub struct ApproximateIndex {
    state: RwLock<State>,
    worker_count: usize,
    /// Committed blocks a single worker may hold before eviction starts.
    block_budget: usize,
    evicted: AtomicU64,
}

#[derive(Debug)]
struct State {
    /// Committed owners, one bitset per block hash.
    owners: HashMap<BlockHash, WorkerSet>,
    /// Blocks attributed to in-flight requests.
    reserved: HashMap<BlockHash, Reserved>,
    workers: Vec<WorkerBlocks>,
    /// Monotonic counter standing in for time in the LRU ordering.
    clock: u64,
}

/// In-flight attribution for one block.
#[derive(Debug, Default)]
struct Reserved {
    /// Workers with at least one outstanding reservation. Read on the matching
    /// path, which is why it is kept alongside the counts rather than derived.
    mask: WorkerSet,
    counts: HashMap<u32, u32>,
}

/// One worker's committed blocks, evicted leaf-first and least-recently-used.
///
/// # Why leaf-first, and not plain LRU
///
/// Plain LRU over blocks is wrong here, and wrong in the worst direction. A
/// chain is only reachable from its head: prefix matching walks blocks in
/// order and stops at the first one missing. Evicting a chain's oldest block
/// evicts its *first* block, which strands every block behind it. The worker
/// keeps paying to store them and the index can never match them again, so the
/// modelled hit rate collapses to zero while the modelled memory stays full.
///
/// Engines do not behave that way. A block with a cached child cannot be
/// dropped without stranding that child, so eviction takes leaves first and
/// picks the least recently used among them. Modelling that keeps a hot shared
/// prefix alive while one-off tails are reclaimed, which is the entire
/// behaviour cache-aware routing is trying to exploit.
///
/// Under pressure a chain is therefore eaten from the tail, and prefix matching
/// degrades a block at a time instead of falling off a cliff.
#[derive(Debug, Default)]
struct WorkerBlocks {
    entries: HashMap<BlockHash, Entry>,
    /// Blocks with no cached child, keyed by last use so the oldest is first.
    /// Only these are eligible for eviction.
    leaves: BTreeMap<u64, BlockHash>,
}

#[derive(Debug)]
struct Entry {
    clock: u64,
    parent: Option<BlockHash>,
    children: usize,
}

impl WorkerBlocks {
    /// Record that this worker holds `hash`, whose predecessor in the chain is
    /// `parent`.
    fn touch(&mut self, hash: BlockHash, parent: Option<BlockHash>, clock: u64) {
        match self.entries.get_mut(&hash) {
            Some(entry) => {
                let previous = entry.clock;
                entry.clock = clock;
                if entry.children == 0 {
                    self.leaves.remove(&previous);
                    self.leaves.insert(clock, hash);
                }
            }
            None => {
                self.entries.insert(
                    hash,
                    Entry {
                        clock,
                        parent,
                        children: 0,
                    },
                );
                self.leaves.insert(clock, hash);

                if let Some(parent) = parent {
                    if let Some(parent_entry) = self.entries.get_mut(&parent) {
                        parent_entry.children += 1;
                        if parent_entry.children == 1 {
                            // No longer a leaf, so no longer evictable.
                            self.leaves.remove(&parent_entry.clock);
                        }
                    }
                }
            }
        }
    }

    /// Drop the least recently used leaf, returning it.
    fn evict_one(&mut self) -> Option<BlockHash> {
        let (clock, hash) = self.leaves.iter().next().map(|(k, v)| (*k, *v))?;
        self.leaves.remove(&clock);

        let entry = self.entries.remove(&hash)?;
        if let Some(parent) = entry.parent {
            if let Some(parent_entry) = self.entries.get_mut(&parent) {
                parent_entry.children -= 1;
                if parent_entry.children == 0 {
                    // The parent just became the new tail of its chain.
                    self.leaves.insert(parent_entry.clock, parent);
                }
            }
        }

        Some(hash)
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

impl ApproximateIndex {
    pub fn new(worker_count: usize, block_budget: usize) -> Self {
        assert!(
            worker_count > 0 && worker_count <= MAX_WORKERS,
            "worker count must be between 1 and {MAX_WORKERS}, got {worker_count}"
        );

        Self {
            state: RwLock::new(State {
                owners: HashMap::new(),
                reserved: HashMap::new(),
                workers: (0..worker_count).map(|_| WorkerBlocks::default()).collect(),
                clock: 0,
            }),
            worker_count,
            block_budget,
            evicted: AtomicU64::new(0),
        }
    }

    pub fn worker_count(&self) -> usize {
        self.worker_count
    }

    /// Read the state, recovering if a previous holder panicked.
    ///
    /// The index is an approximation whose worst failure is a cache miss, so
    /// taking possibly-inconsistent state is strictly better than turning one
    /// panic into a permanently broken router.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn all_workers(&self) -> WorkerSet {
        if self.worker_count == MAX_WORKERS {
            WorkerSet::MAX
        } else {
            (1 << self.worker_count) - 1
        }
    }
}

impl BlockIndex for ApproximateIndex {
    fn match_prefix(&self, blocks: &[BlockHash], matched: &mut [usize]) {
        debug_assert!(matched.len() >= self.worker_count);
        matched[..self.worker_count].fill(0);

        if blocks.is_empty() {
            return;
        }

        let state = self.read();
        // Workers still matching every block seen so far. It only ever shrinks,
        // so a worker's answer can be written once, at the block where it
        // dropped out.
        let mut alive = self.all_workers();

        for (position, hash) in blocks.iter().enumerate() {
            let committed = state.owners.get(hash).copied().unwrap_or(0);
            let reserved = state
                .reserved
                .get(hash)
                .map(|entry| entry.mask)
                .unwrap_or(0);

            let dropped = alive & !(committed | reserved);
            if dropped != 0 {
                for worker in set_bits(dropped) {
                    matched[worker] = position;
                }
                alive &= !dropped;
                if alive == 0 {
                    return;
                }
            }
        }

        for worker in set_bits(alive) {
            matched[worker] = blocks.len();
        }
    }

    fn reserve_blocks(&self, worker: usize, blocks: &[BlockHash]) {
        if blocks.is_empty() || worker >= self.worker_count {
            return;
        }

        let mut state = self.write();
        for hash in blocks {
            let entry = state.reserved.entry(*hash).or_default();
            entry.mask |= 1 << worker;
            *entry.counts.entry(worker as u32).or_insert(0) += 1;
        }
    }

    fn commit(&self, worker: usize, blocks: &[BlockHash]) {
        if blocks.is_empty() || worker >= self.worker_count {
            return;
        }

        let mut state = self.write();
        drop_reservations(&mut state, worker, blocks);

        let mut parent = None;
        for hash in blocks {
            state.clock += 1;
            let clock = state.clock;
            *state.owners.entry(*hash).or_insert(0) |= 1 << worker;
            state.workers[worker].touch(*hash, parent, clock);
            parent = Some(*hash);
        }

        // Evicting after the whole chain lands, rather than block by block,
        // keeps a long prompt from evicting its own leading blocks while it is
        // still being inserted.
        let mut evicted = 0u64;
        while state.workers[worker].len() > self.block_budget {
            let Some(dropped) = state.workers[worker].evict_one() else {
                break;
            };
            if let Some(owners) = state.owners.get_mut(&dropped) {
                *owners &= !(1 << worker);
                if *owners == 0 {
                    state.owners.remove(&dropped);
                }
            }
            evicted += 1;
        }
        if evicted > 0 {
            self.evicted.fetch_add(evicted, Ordering::Relaxed);
        }
    }

    fn release(&self, worker: usize, blocks: &[BlockHash]) {
        if blocks.is_empty() || worker >= self.worker_count {
            return;
        }

        let mut state = self.write();
        drop_reservations(&mut state, worker, blocks);
    }

    fn stats(&self) -> IndexStats {
        let state = self.read();
        IndexStats {
            blocks: state.owners.len(),
            blocks_per_worker: state.workers.iter().map(WorkerBlocks::len).collect(),
            reserved: state
                .reserved
                .values()
                .map(|entry| entry.counts.len())
                .sum(),
            evicted: self.evicted.load(Ordering::Relaxed),
        }
    }
}

/// Decrement one worker's reservation on each block, clearing empty entries.
fn drop_reservations(state: &mut State, worker: usize, blocks: &[BlockHash]) {
    for hash in blocks {
        let Some(entry) = state.reserved.get_mut(hash) else {
            continue;
        };

        let mut worker_is_done = false;
        if let Some(count) = entry.counts.get_mut(&(worker as u32)) {
            *count -= 1;
            if *count == 0 {
                worker_is_done = true;
            }
        }
        if worker_is_done {
            entry.counts.remove(&(worker as u32));
            entry.mask &= !(1 << worker);
        }
        if entry.counts.is_empty() {
            state.reserved.remove(hash);
        }
    }
}

/// Worker indices in a bitset, lowest first.
fn set_bits(mut mask: WorkerSet) -> impl Iterator<Item = usize> {
    std::iter::from_fn(move || {
        if mask == 0 {
            return None;
        }
        let index = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        Some(index)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::index::Reservation;

    fn chain(seed: u64, len: usize) -> Vec<BlockHash> {
        (0..len)
            .map(|i| BlockHash(seed * 1_000 + i as u64))
            .collect()
    }

    fn matched(index: &ApproximateIndex, blocks: &[BlockHash]) -> Vec<usize> {
        let mut out = vec![0; index.worker_count()];
        index.match_prefix(blocks, &mut out);
        out
    }

    #[test]
    fn an_empty_index_matches_nothing() {
        let index = ApproximateIndex::new(3, 1_000);
        assert_eq!(matched(&index, &chain(1, 5)), vec![0, 0, 0]);
    }

    #[test]
    fn an_empty_prompt_matches_nothing() {
        let index = ApproximateIndex::new(3, 1_000);
        index.commit(0, &chain(1, 5));
        assert_eq!(matched(&index, &[]), vec![0, 0, 0]);
    }

    #[test]
    fn a_committed_chain_matches_in_full_on_its_own_worker() {
        let index = ApproximateIndex::new(3, 1_000);
        let blocks = chain(1, 5);
        index.commit(1, &blocks);

        assert_eq!(matched(&index, &blocks), vec![0, 5, 0]);
    }

    #[test]
    fn matching_stops_at_the_first_block_a_worker_lacks() {
        let index = ApproximateIndex::new(2, 1_000);
        let blocks = chain(1, 6);

        index.commit(0, &blocks);
        index.commit(1, &blocks[..3]);

        assert_eq!(matched(&index, &blocks), vec![6, 3]);
    }

    #[test]
    fn a_gap_in_the_middle_truncates_the_match() {
        let index = ApproximateIndex::new(1, 1_000);
        let blocks = chain(1, 6);

        // Blocks 0, 1, 3, 4 committed; block 2 never was. A prefix match has to
        // stop at the gap rather than counting the blocks past it.
        index.commit(0, &blocks[..2]);
        index.commit(0, &blocks[3..]);

        assert_eq!(matched(&index, &blocks), vec![2]);
    }

    #[test]
    fn a_reservation_makes_blocks_matchable_before_the_request_finishes() {
        let index = Arc::new(ApproximateIndex::new(2, 1_000));
        let blocks: Arc<[BlockHash]> = chain(1, 4).into();

        // The burst case: nothing is committed, one request is in flight, and a
        // second request with the same prefix arrives. It has to see the first
        // one's worker as a match, or the two get scattered.
        assert_eq!(matched(&index, &blocks), vec![0, 0]);
        let _reservation = Reservation::new(index.clone(), 1, blocks.clone());
        assert_eq!(matched(&index, &blocks), vec![0, 4]);
    }

    #[test]
    fn dropping_a_reservation_takes_the_blocks_back() {
        let index = Arc::new(ApproximateIndex::new(2, 1_000));
        let blocks: Arc<[BlockHash]> = chain(1, 4).into();

        {
            let _reservation = Reservation::new(index.clone(), 0, blocks.clone());
            assert_eq!(matched(&index, &blocks), vec![4, 0]);
        }

        assert_eq!(
            matched(&index, &blocks),
            vec![0, 0],
            "a cancelled request must not leave blocks behind"
        );
        assert_eq!(index.stats().reserved, 0);
    }

    #[test]
    fn confirming_a_reservation_commits_the_blocks() {
        let index = Arc::new(ApproximateIndex::new(2, 1_000));
        let blocks: Arc<[BlockHash]> = chain(1, 4).into();

        {
            let mut reservation = Reservation::new(index.clone(), 0, blocks.clone());
            reservation.confirm();
        }

        assert_eq!(matched(&index, &blocks), vec![4, 0]);
        assert_eq!(index.stats().reserved, 0);
        assert_eq!(index.stats().blocks, 4);
    }

    #[test]
    fn concurrent_reservations_on_one_worker_are_counted_not_flagged() {
        let index = Arc::new(ApproximateIndex::new(2, 1_000));
        let blocks: Arc<[BlockHash]> = chain(1, 3).into();

        let first = Reservation::new(index.clone(), 0, blocks.clone());
        let second = Reservation::new(index.clone(), 0, blocks.clone());

        drop(first);
        assert_eq!(
            matched(&index, &blocks),
            vec![3, 0],
            "one of two in-flight requests ending should not clear the blocks"
        );

        drop(second);
        assert_eq!(matched(&index, &blocks), vec![0, 0]);
    }

    #[test]
    fn eviction_keeps_a_worker_inside_its_budget() {
        let index = ApproximateIndex::new(1, 10);

        index.commit(0, &chain(1, 8));
        assert_eq!(index.stats().blocks_per_worker[0], 8);

        index.commit(0, &chain(2, 8));
        assert_eq!(index.stats().blocks_per_worker[0], 10);
        assert_eq!(index.stats().evicted, 6);
    }

    #[test]
    fn eviction_eats_a_chain_from_the_tail_not_the_head() {
        let index = ApproximateIndex::new(1, 6);
        let old = chain(1, 4);
        let fresh = chain(2, 4);

        index.commit(0, &old);
        index.commit(0, &fresh);

        // Six slots, eight blocks. The older chain gives up two, and it gives
        // up its last two, not its first two. Under plain LRU this would read
        // zero, because losing block 0 strands the rest.
        assert_eq!(matched(&index, &old), vec![2]);
        assert_eq!(matched(&index, &fresh), vec![4]);
        assert_eq!(index.stats().evicted, 2);
    }

    #[test]
    fn a_recently_used_prefix_outlives_a_colder_one() {
        let index = ApproximateIndex::new(1, 6);
        let cold = chain(1, 4);
        let hot = chain(2, 4);

        index.commit(0, &cold);
        index.commit(0, &hot);
        // Use the hot prefix again, so the cold one holds the oldest leaves.
        index.commit(0, &hot);
        index.commit(0, &chain(3, 2));

        assert_eq!(
            matched(&index, &hot),
            vec![4],
            "the most recently used prefix should survive intact"
        );
        assert!(
            matched(&index, &cold)[0] < 4,
            "the colder prefix should have given up the space"
        );
    }

    #[test]
    fn a_prefix_with_a_cached_continuation_is_not_evicted_before_it() {
        let index = ApproximateIndex::new(1, 5);
        let shared = chain(1, 3);
        let mut extended = shared.clone();
        extended.extend(chain(9, 2));

        index.commit(0, &shared);
        index.commit(0, &extended);
        // Force one eviction. The shared head has a cached child, so the tail
        // has to go first.
        index.commit(0, &chain(2, 1));

        assert_eq!(matched(&index, &shared), vec![3]);
    }

    #[test]
    fn a_block_shared_by_two_workers_survives_one_of_them_evicting_it() {
        let index = ApproximateIndex::new(2, 4);
        let shared = chain(1, 4);

        index.commit(0, &shared);
        index.commit(1, &shared);
        // Push worker 0 over its budget so it drops the shared chain.
        index.commit(0, &chain(2, 4));

        assert_eq!(matched(&index, &shared), vec![0, 4]);
        assert_eq!(index.stats().blocks, 8);
    }

    #[test]
    fn a_budget_smaller_than_one_chain_keeps_the_matchable_head() {
        let index = ApproximateIndex::new(1, 3);
        let blocks = chain(1, 10);

        index.commit(0, &blocks);

        // Only three of ten blocks fit, and they are the three that a prefix
        // match can actually use. Keeping the tail instead would leave the
        // worker holding blocks no request can ever match.
        assert_eq!(index.stats().blocks_per_worker[0], 3);
        assert_eq!(matched(&index, &blocks), vec![3]);
    }

    #[test]
    fn committing_to_an_unknown_worker_is_ignored() {
        let index = ApproximateIndex::new(2, 100);
        index.commit(7, &chain(1, 4));
        assert_eq!(index.stats().blocks, 0);
    }

    #[test]
    fn set_bits_walks_a_mask_in_order() {
        assert_eq!(set_bits(0).collect::<Vec<_>>(), Vec::<usize>::new());
        assert_eq!(set_bits(0b1011).collect::<Vec<_>>(), vec![0, 1, 3]);
        assert_eq!(set_bits(1 << 63).collect::<Vec<_>>(), vec![63]);
    }

    #[test]
    fn a_full_fleet_uses_every_bit_of_the_mask() {
        let index = ApproximateIndex::new(MAX_WORKERS, 100);
        assert_eq!(index.all_workers(), u64::MAX);

        let blocks = chain(1, 2);
        index.commit(63, &blocks);
        assert_eq!(matched(&index, &blocks)[63], 2);
    }
}
