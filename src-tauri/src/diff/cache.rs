//! Bytes-bounded LRU cache for per-file diffs. Shared, cheap to clone (everything is behind
//! `Arc<Mutex<_>>`) so a command can clone the handle into a `spawn_blocking` closure.
//! Invalidation is by key: the file key carries the file's size+mtime signature, so an edit
//! (even an unstaged one) yields a fresh key and the stale entry simply ages out. (Summaries
//! are cached separately, keyed on the watcher generation, by [`super::snapshot`].)

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use super::parse::FileDiff;

/// Total bytes of cached per-file diffs to keep.
const FILES_BUDGET: usize = 16 * 1024 * 1024;

/// Cache key for a single file's diff.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileKey {
    pub worktree: String,
    pub path: String,
    /// `size:mtime` — moves on an unstaged edit, so a mtime-only miss can't serve stale text.
    pub file_sig: String,
    pub head_oid: String,
    /// Merge-base oid — the diff base moves when `main` advances even if the file and HEAD
    /// are unchanged, so it must be part of the key (the summary fingerprint already hashes it).
    pub base_oid: String,
    pub ignore_ws: bool,
}

/// A tiny bytes-bounded LRU. `order` holds keys oldest→newest; `get` promotes, `put` evicts
/// from the front until under budget. O(n) promotion is fine at these cache sizes.
struct Lru<K: Eq + Hash + Clone, V: Clone> {
    map: HashMap<K, (V, usize)>,
    order: VecDeque<K>,
    bytes: usize,
    budget: usize,
}

impl<K: Eq + Hash + Clone, V: Clone> Lru<K, V> {
    fn new(budget: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            budget,
        }
    }

    fn promote(&mut self, key: &K) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            if let Some(k) = self.order.remove(pos) {
                self.order.push_back(k);
            }
        }
    }

    fn get(&mut self, key: &K) -> Option<V> {
        let v = self.map.get(key).map(|(v, _)| v.clone());
        if v.is_some() {
            self.promote(key);
        }
        v
    }

    fn put(&mut self, key: K, value: V, size: usize) {
        if let Some((_, old)) = self.map.remove(&key) {
            self.bytes -= old;
            if let Some(pos) = self.order.iter().position(|k| *k == key) {
                self.order.remove(pos);
            }
        }
        self.map.insert(key.clone(), (value, size));
        self.order.push_back(key);
        self.bytes += size;
        while self.bytes > self.budget {
            match self.order.pop_front() {
                Some(evict) => {
                    if let Some((_, sz)) = self.map.remove(&evict) {
                        self.bytes -= sz;
                    }
                }
                None => break,
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
}

fn file_diff_size(d: &FileDiff) -> usize {
    64 + d
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .map(|l| 24 + l.text.len())
        .sum::<usize>()
}

/// Managed-state handle for the per-file diff cache. Clone shares the underlying map.
#[derive(Clone)]
pub struct DiffCache {
    files: Arc<Mutex<Lru<FileKey, FileDiff>>>,
}

impl Default for DiffCache {
    fn default() -> Self {
        Self {
            files: Arc::new(Mutex::new(Lru::new(FILES_BUDGET))),
        }
    }
}

impl DiffCache {
    pub fn get_file(&self, key: &FileKey) -> Option<FileDiff> {
        self.files.lock().ok()?.get(key)
    }

    pub fn put_file(&self, key: FileKey, diff: FileDiff) {
        let size = file_diff_size(&diff);
        if let Ok(mut g) = self.files.lock() {
            g.put(key, diff, size);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_key_distinguishes_sig_and_ws() {
        let c = DiffCache::default();
        let base = FileKey {
            worktree: "a".into(),
            path: "f.rs".into(),
            file_sig: "10:100".into(),
            head_oid: "abc".into(),
            base_oid: "base1".into(),
            ignore_ws: true,
        };
        c.put_file(base.clone(), FileDiff::default());
        assert!(c.get_file(&base).is_some());
        // edited file (new sig) ⇒ miss
        assert!(c
            .get_file(&FileKey {
                file_sig: "12:200".into(),
                ..base.clone()
            })
            .is_none());
        // merge-base advanced (main moved) ⇒ miss
        assert!(c
            .get_file(&FileKey {
                base_oid: "base2".into(),
                ..base.clone()
            })
            .is_none());
        // whitespace toggle ⇒ miss
        assert!(c
            .get_file(&FileKey {
                ignore_ws: false,
                ..base
            })
            .is_none());
    }

    #[test]
    fn evicts_oldest_over_budget() {
        let mut lru: Lru<u32, Vec<u8>> = Lru::new(100);
        lru.put(1, vec![0u8; 40], 40);
        lru.put(2, vec![0u8; 40], 40);
        lru.get(&1); // promote 1 → 2 is now oldest
        lru.put(3, vec![0u8; 40], 40); // bytes 120 > 100 → evict oldest (2)
        assert!(lru.get(&2).is_none());
        assert!(lru.get(&1).is_some());
        assert!(lru.get(&3).is_some());
        assert_eq!(lru.len(), 2);
    }
}
