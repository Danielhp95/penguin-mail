//! Pictures kept by how recently they were used, up to a number of bytes
//! rather than a number of pictures: one inline photo can be 5 MB and a
//! row's picture a few kilobytes. The conversation's attachment pictures
//! and the contact photos each keep one.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::rc::Rc;

/// Something [`Recent`] keeps, and how many bytes it holds.
pub(crate) trait Weighed: Clone {
    fn weight(&self) -> usize;
}

impl Weighed for String {
    fn weight(&self) -> usize {
        self.len()
    }
}

impl Weighed for Rc<str> {
    fn weight(&self) -> usize {
        self.len()
    }
}

/// Values kept by how recently they were used, up to `limit` bytes.
#[derive(Debug)]
pub(crate) struct Recent<K, V = String> {
    limit: usize,
    used: usize,
    clock: u64,
    held: HashMap<K, (V, u64)>,
    /// The keys by when they were last used, oldest first.
    order: BTreeMap<u64, K>,
}

impl<K: Hash + Eq + Clone, V: Weighed> Recent<K, V> {
    pub(crate) fn new(limit: usize) -> Recent<K, V> {
        Recent {
            limit,
            used: 0,
            clock: 0,
            held: HashMap::new(),
            order: BTreeMap::new(),
        }
    }

    /// The value under `key`, which now counts as the most recent.
    pub(crate) fn get(&mut self, key: &K) -> Option<V> {
        self.clock += 1;
        let (value, used_at) = self.held.get_mut(key)?;
        self.order.remove(used_at);
        *used_at = self.clock;
        self.order.insert(self.clock, key.clone());
        Some(value.clone())
    }

    /// Whether a value is kept under `key`, without counting as a use.
    pub(crate) fn holds(&self, key: &K) -> bool {
        self.held.contains_key(key)
    }

    /// Keeps `value` under `key`, letting the least recent values go
    /// until it fits. A value larger than the whole limit is not kept.
    pub(crate) fn put(&mut self, key: K, value: V) {
        if let Some((old, used_at)) = self.held.remove(&key) {
            self.used -= old.weight();
            self.order.remove(&used_at);
        }
        if !self.fits(&value) {
            return;
        }
        while self.used + value.weight() > self.limit {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            if let Some((gone, _)) = self.held.remove(&oldest) {
                self.used -= gone.weight();
            }
        }
        self.clock += 1;
        self.used += value.weight();
        self.order.insert(self.clock, key.clone());
        self.held.insert(key, (value, self.clock));
    }

    /// Whether `value` could be kept at all, being no larger than the limit.
    pub(crate) fn fits(&self, value: &V) -> bool {
        value.weight() <= self.limit
    }

    /// Lets every value go.
    pub(crate) fn clear(&mut self) {
        self.held.clear();
        self.order.clear();
        self.used = 0;
    }

    /// Bytes kept now.
    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        self.used
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Key = (i64, String, String);

    fn key(n: u8) -> Key {
        (1, format!("m{n}"), format!("a{n}"))
    }

    #[test]
    fn the_least_recent_picture_goes_first_once_the_bytes_run_out() {
        let mut recent: Recent<Key> = Recent::new(10);
        recent.put(key(1), "aaaa".into());
        recent.put(key(2), "bbbb".into());
        // Using the first makes the second the oldest.
        assert_eq!(recent.get(&key(1)).as_deref(), Some("aaaa"));
        recent.put(key(3), "cccc".into());
        assert_eq!(recent.get(&key(2)), None);
        assert!(recent.get(&key(1)).is_some() && recent.get(&key(3)).is_some());
        assert_eq!(recent.used(), 8);
    }

    #[test]
    fn a_picture_larger_than_the_whole_cache_is_not_kept() {
        let mut recent: Recent<Key> = Recent::new(4);
        recent.put(key(1), "ab".into());
        recent.put(key(2), "abcdefgh".into());
        assert_eq!(recent.get(&key(2)), None);
        assert_eq!(
            recent.get(&key(1)).as_deref(),
            Some("ab"),
            "nothing made room"
        );
    }

    #[test]
    fn keeping_a_picture_again_replaces_its_bytes() {
        let mut recent: Recent<Key> = Recent::new(100);
        recent.put(key(1), "abc".into());
        recent.put(key(1), "abcdef".into());
        assert_eq!(recent.used(), 6);
        assert_eq!(recent.get(&key(1)).as_deref(), Some("abcdef"));
    }
}
