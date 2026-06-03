//! Tiny LRU cache with optional TTL — Rust port of `src/lib/rag/lru-cache.ts`.
//!
//! Backed by [`indexmap::IndexMap`] so we can move-to-back on access using
//! [`IndexMap::move_index`], avoiding a hand-rolled doubly-linked list.
//! Used by the query-embedding cache; not a general-purpose utility — keep
//! the public surface minimal until a second caller appears (CLAUDE.md §3.3).

use std::hash::Hash;
use std::time::{Duration, Instant};

use indexmap::IndexMap;

/// Bounded LRU cache. Eviction policy: oldest insertion (= front of the
/// [`IndexMap`]) is dropped when `len > max` after a `put`.
///
/// When `ttl` is `Some(d)`, `get` lazily evicts entries older than `d` and
/// returns `None` for them.
pub struct LruCache<K: Hash + Eq, V: Clone> {
    max: usize,
    ttl: Option<Duration>,
    /// Values stored as `(value, inserted_at)` — same shape as the TS source.
    map: IndexMap<K, (V, Instant)>,
}

impl<K: Hash + Eq, V: Clone> LruCache<K, V> {
    /// Construct a new cache. `max` is required (0 means "store nothing" —
    /// we don't second-guess callers; they pick a sensible default).
    pub fn new(max: usize, ttl: Option<Duration>) -> Self {
        Self {
            max,
            ttl,
            map: IndexMap::new(),
        }
    }

    /// Look up a key. On hit: returns a clone and promotes the entry to the
    /// most-recent slot. On TTL expiry: lazily evicts and returns `None`.
    pub fn get(&mut self, key: &K) -> Option<V> {
        let idx = self.map.get_index_of(key)?;
        // SAFETY: idx came from get_index_of, so the index is in-range.
        let (_, (value, at)) = self.map.get_index(idx).expect("idx in range");
        if let Some(ttl) = self.ttl {
            if at.elapsed() > ttl {
                // Expired — drop the stale entry and report miss.
                self.map.shift_remove_index(idx);
                return None;
            }
        }
        let value = value.clone();
        // Promote to most-recent slot (back of the map).
        let last = self.map.len() - 1;
        if idx != last {
            self.map.move_index(idx, last);
        }
        Some(value)
    }

    /// Insert or overwrite `key`. After insert, evicts the oldest entry when
    /// over capacity. Re-inserting an existing key moves it to most-recent.
    pub fn put(&mut self, key: K, value: V) {
        // Remove first so a re-insert moves the entry to the back. Mirrors
        // `Map.delete` + `Map.set` in TS.
        self.map.shift_remove(&key);
        self.map.insert(key, (value, Instant::now()));
        if self.map.len() > self.max {
            self.map.shift_remove_index(0);
        }
    }

    /// Current entry count (post-eviction snapshot; expired entries that
    /// haven't been probed by `get` may still be counted).
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}
