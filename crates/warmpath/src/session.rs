//! Session affinity.
//!
//! A multi-turn conversation should keep going back to the worker that already
//! holds its history. Prefix affinity mostly achieves that on its own, because
//! turn N+1 shares turn N's whole prefix and the index therefore points at the
//! same worker. Session affinity is kept as a separate, composable mechanism
//! for the cases where that reasoning does not hold: a policy that ignores the
//! index, a prompt too short to have blocks, or a history the worker evicted
//! between turns.
//!
//! Whether it adds anything on top of prefix affinity is a measurable question
//! rather than an assumption, and the comparison harness can answer it.
//!
//! The mapping is a hint, never a guarantee. It yields to health and to the
//! balance override, because a session pinned to a worker that is failing or
//! saturated is worth less than a request that completes.

use std::collections::HashMap;
use std::sync::Mutex;

/// Session to worker, with a bound on how much is remembered.
///
/// The bound matters: session ids come from clients, so an unbounded map is a
/// memory leak with an external trigger. When the map is full the oldest half
/// is dropped, which costs those sessions their affinity and nothing else.
#[derive(Debug)]
pub struct SessionAffinity {
    inner: Mutex<Inner>,
    capacity: usize,
}

#[derive(Debug, Default)]
struct Inner {
    /// Session id to the worker it last used, and when that was.
    entries: HashMap<String, Entry>,
    clock: u64,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    worker: usize,
    seen: u64,
}

impl SessionAffinity {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            capacity,
        }
    }

    /// The worker this session last used.
    pub fn get(&self, session: &str) -> Option<usize> {
        if self.capacity == 0 {
            return None;
        }
        let inner = self.lock();
        inner.entries.get(session).map(|entry| entry.worker)
    }

    /// Record where this session's latest request went.
    pub fn remember(&self, session: &str, worker: usize) {
        if self.capacity == 0 || session.is_empty() {
            return;
        }

        let mut inner = self.lock();
        inner.clock += 1;
        let clock = inner.clock;

        if let Some(entry) = inner.entries.get_mut(session) {
            entry.worker = worker;
            entry.seen = clock;
            return;
        }

        if inner.entries.len() >= self.capacity {
            inner.evict_oldest_half();
        }
        inner.entries.insert(
            session.to_string(),
            Entry {
                worker,
                seen: clock,
            },
        );
    }

    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Recovers from a poisoned lock rather than propagating the panic, for the
    /// same reason the block index does: losing this map costs cache affinity,
    /// never correctness.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Inner {
    /// Drop the least recently seen half.
    ///
    /// Halving rather than evicting one at a time, so the scan is paid once per
    /// many insertions instead of on every insertion past the limit.
    fn evict_oldest_half(&mut self) {
        let target = self.entries.len() / 2;
        if target == 0 {
            self.entries.clear();
            return;
        }

        let mut seen: Vec<u64> = self.entries.values().map(|entry| entry.seen).collect();
        seen.sort_unstable();
        let cutoff = seen[target];
        self.entries.retain(|_, entry| entry.seen >= cutoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_session_has_no_worker() {
        let sessions = SessionAffinity::new(16);
        assert_eq!(sessions.get("nobody"), None);
        assert!(sessions.is_empty());
    }

    #[test]
    fn a_session_returns_to_the_worker_it_used() {
        let sessions = SessionAffinity::new(16);
        sessions.remember("s1", 2);

        assert_eq!(sessions.get("s1"), Some(2));
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn a_session_moves_when_its_request_is_routed_elsewhere() {
        let sessions = SessionAffinity::new(16);
        sessions.remember("s1", 2);
        sessions.remember("s1", 0);

        assert_eq!(sessions.get("s1"), Some(0));
        assert_eq!(sessions.len(), 1, "the session should not be duplicated");
    }

    #[test]
    fn sessions_are_independent() {
        let sessions = SessionAffinity::new(16);
        sessions.remember("a", 0);
        sessions.remember("b", 1);

        assert_eq!(sessions.get("a"), Some(0));
        assert_eq!(sessions.get("b"), Some(1));
    }

    #[test]
    fn a_zero_capacity_map_remembers_nothing() {
        let sessions = SessionAffinity::new(0);
        sessions.remember("s1", 1);

        assert_eq!(sessions.get("s1"), None);
        assert!(sessions.is_empty());
    }

    #[test]
    fn an_empty_session_id_is_ignored() {
        let sessions = SessionAffinity::new(16);
        sessions.remember("", 1);
        assert!(sessions.is_empty());
    }

    /// Session ids come from clients, so the map has to be bounded or a client
    /// can grow it without limit.
    #[test]
    fn the_map_stays_inside_its_capacity() {
        let sessions = SessionAffinity::new(64);
        for index in 0..1_000 {
            sessions.remember(&format!("s{index}"), index % 3);
        }

        assert!(
            sessions.len() <= 64,
            "the map grew to {} entries",
            sessions.len()
        );
    }

    #[test]
    fn eviction_keeps_the_most_recently_seen_sessions() {
        let sessions = SessionAffinity::new(8);
        for index in 0..8 {
            sessions.remember(&format!("old{index}"), 0);
        }
        // Touching one old session makes it recent again.
        sessions.remember("old0", 1);
        for index in 0..4 {
            sessions.remember(&format!("new{index}"), 2);
        }

        assert_eq!(sessions.get("old0"), Some(1), "a touched session survived");
        for index in 0..4 {
            assert_eq!(sessions.get(&format!("new{index}")), Some(2));
        }
    }
}
