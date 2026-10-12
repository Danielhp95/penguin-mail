//! The last small messages an account fetched whole. A raw message holds
//! every file inside it, so saving a file, drawing a picture and checking
//! a signature right after a small message opened read these bytes instead
//! of fetching the whole message again. A message the store reports at
//! or over `RAW_LIMIT` enters only when its bytes come to less: its files
//! come one part at a time.

use std::collections::VecDeque;
use std::sync::Arc;

use mailrs_domain::AccountId;

/// How many bytes of raw mail all accounts together keep in memory.
pub const RAW_CACHE_BYTES: usize = 16 << 20;

/// One cached message: its account and the server's id, and its bytes.
type Entry = ((AccountId, String), Arc<Vec<u8>>);

/// Raw messages by account and the server's id, least recently used
/// first. Every account's loop shares one, so the whole app holds
/// `RAW_CACHE_BYTES` however many accounts it syncs, and the account being
/// read is the one that keeps the room.
#[derive(Debug)]
pub struct RawCache {
    cap: usize,
    used: usize,
    entries: VecDeque<Entry>,
}

impl RawCache {
    pub fn new(cap: usize) -> RawCache {
        RawCache {
            cap,
            used: 0,
            entries: VecDeque::new(),
        }
    }

    fn position(&self, account: AccountId, id: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|((a, key), _)| *a == account && key == id)
    }

    /// The message's bytes, which now count as the most recently used.
    pub fn get(&mut self, account: AccountId, id: &str) -> Option<Arc<Vec<u8>>> {
        let at = self.position(account, id)?;
        let entry = self.entries.remove(at)?;
        let bytes = Arc::clone(&entry.1);
        self.entries.push_back(entry);
        Some(bytes)
    }

    /// Keeps `bytes`, dropping the least recently used until they fit. A
    /// message bigger than the whole cache is not kept.
    pub fn put(&mut self, account: AccountId, id: String, bytes: Arc<Vec<u8>>) {
        if bytes.len() > self.cap {
            return;
        }
        if let Some(at) = self.position(account, &id)
            && let Some((_, old)) = self.entries.remove(at)
        {
            self.used -= old.len();
        }
        while self.used + bytes.len() > self.cap {
            let Some((_, old)) = self.entries.pop_front() else {
                break;
            };
            self.used -= old.len();
        }
        self.used += bytes.len();
        self.entries.push_back(((account, id), bytes));
    }

    /// Drops everything an account holds, as it stops.
    pub fn forget(&mut self, account: AccountId) {
        self.entries.retain(|((a, _), _)| *a != account);
        self.used = self.entries.iter().map(|(_, bytes)| bytes.len()).sum();
    }

    /// The bytes held now.
    pub fn used(&self) -> usize {
        self.used
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::RawCache;

    fn bytes(n: usize) -> Arc<Vec<u8>> {
        Arc::new(vec![0; n])
    }

    #[test]
    fn the_cache_keeps_to_its_cap_and_drops_the_oldest() {
        let mut cache = RawCache::new(10);
        cache.put(1, "a".into(), bytes(4));
        cache.put(1, "b".into(), bytes(4));
        assert!(cache.get(1, "a").is_some());
        cache.put(1, "c".into(), bytes(4));
        assert!(cache.get(1, "b").is_none(), "b was the least recent");
        assert!(cache.get(1, "a").is_some() && cache.get(1, "c").is_some());
    }

    #[test]
    fn a_message_bigger_than_the_cache_is_not_kept() {
        let mut cache = RawCache::new(10);
        cache.put(1, "a".into(), bytes(4));
        cache.put(1, "big".into(), bytes(11));
        assert!(cache.get(1, "big").is_none());
        assert!(cache.get(1, "a").is_some());
    }

    #[test]
    fn accounts_share_one_budget() {
        let mut cache = RawCache::new(10);
        cache.put(1, "m".into(), bytes(6));
        cache.put(2, "m".into(), bytes(6));
        assert!(cache.used() <= 10, "two accounts, one cap");
        assert!(cache.get(1, "m").is_none(), "the older account's message made room");
        assert!(cache.get(2, "m").is_some(), "the same id in another account is its own entry");
    }

    #[test]
    fn a_stopped_account_gives_its_bytes_back() {
        let mut cache = RawCache::new(20);
        cache.put(1, "a".into(), bytes(6));
        cache.put(2, "a".into(), bytes(5));
        cache.forget(1);
        assert_eq!(cache.used(), 5);
        assert!(cache.get(1, "a").is_none() && cache.get(2, "a").is_some());
    }
}
