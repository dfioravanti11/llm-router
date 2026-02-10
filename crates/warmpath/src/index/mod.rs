//! The block index: which worker is likely to hold which prefix blocks.
//!
//! The approximate backend here infers cache state from the router's own
//! dispatches and needs nothing from the worker. It is the only backend the
//! committed scope calls for.
//!
//! The trait exists because a second backend, built from vLLM's `BlockStored`
//! and `BlockRemoved` events, is the most interesting optional follow-on
//! (Appendix A2), and marking the seam costs almost nothing. If that never
//! happens this is mild over-engineering, which is worth naming rather than
//! pretending otherwise.
//!
//! # The index is always wrong
//!
//! Even the event-driven backend lags reality. No correctness invariant may
//! depend on the index being right. A wrong entry costs a cache miss, which is
//! a slower request, never a failed one. That rule is what lets everything here
//! recover from a poisoned lock instead of propagating a panic, and what lets
//! token hash collisions be shrugged off upstream.

pub mod approximate;

use std::sync::Arc;

use serde::Serialize;
use warmpath_core::BlockHash;

pub use approximate::ApproximateIndex;

/// Largest fleet the worker bitset can describe.
///
/// The project's non-goals cap it at a handful of replicas, and a `u64` makes
/// prefix matching a few register operations instead of a per-worker loop.
pub const MAX_WORKERS: usize = 64;

pub trait BlockIndex: Send + Sync + std::fmt::Debug {
    /// Fill `matched[w]` with how many leading blocks worker `w` is believed to
    /// hold.
    ///
    /// Cost is proportional to the number of prefix blocks plus the number of
    /// workers, not their product. Each worker's answer is written exactly once,
    /// at the block where it stopped matching.
    fn match_prefix(&self, blocks: &[BlockHash], matched: &mut [usize]);

    /// Provisionally attribute blocks to a worker for a dispatched request.
    fn reserve_blocks(&self, worker: usize, blocks: &[BlockHash]);

    /// Turn a reservation into a committed entry.
    fn commit(&self, worker: usize, blocks: &[BlockHash]);

    /// Drop a reservation that will never complete.
    fn release(&self, worker: usize, blocks: &[BlockHash]);

    fn stats(&self) -> IndexStats;
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct IndexStats {
    /// Distinct block hashes with at least one committed owner.
    pub blocks: usize,
    /// Committed blocks per worker.
    pub blocks_per_worker: Vec<usize>,
    /// Block-and-worker pairs currently reserved by in-flight requests.
    pub reserved: usize,
    /// Blocks dropped to stay inside a worker's budget.
    pub evicted: u64,
}

/// Blocks an in-flight request will produce, attributed to its worker for the
/// duration.
///
/// Two requests carrying the same long prefix can arrive a millisecond apart,
/// before either has finished and taught the index anything. Without this they
/// both score as misses and get spread across the fleet, which is precisely the
/// scattering the router exists to prevent. Burst traffic would defeat the
/// whole mechanism.
///
/// The reservation is released on drop, so a cancelled or failed request cleans
/// up without a separate path. This mirrors the response body's `StreamGuard`
/// deliberately: the same shape means the same failure mode cannot appear in
/// one and not the other.
pub struct Reservation {
    index: Arc<dyn BlockIndex>,
    worker: usize,
    blocks: Arc<[BlockHash]>,
    settled: bool,
}

impl Reservation {
    pub fn new(index: Arc<dyn BlockIndex>, worker: usize, blocks: Arc<[BlockHash]>) -> Self {
        index.reserve_blocks(worker, &blocks);
        Self {
            index,
            worker,
            blocks,
            settled: false,
        }
    }

    /// The request finished, so what it produced is really on that worker now.
    pub fn confirm(&mut self) {
        if !self.settled {
            self.settled = true;
            self.index.commit(self.worker, &self.blocks);
        }
    }

    pub fn worker(&self) -> usize {
        self.worker
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation")
            .field("worker", &self.worker)
            .field("blocks", &self.blocks.len())
            .field("settled", &self.settled)
            .finish()
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.settled {
            self.settled = true;
            self.index.release(self.worker, &self.blocks);
        }
    }
}
