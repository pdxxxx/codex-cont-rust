use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

pub struct IdStore {
    maxsize: usize,
    ttl: Duration,
    // ponytail: O(n) LRU is fine for 10k IDs; use an indexed LRU only if this gets hot.
    entries: Mutex<Vec<(String, Instant)>>,
}

impl Default for IdStore {
    fn default() -> Self {
        Self::new(10_000, Duration::from_secs(3600))
    }
}

impl IdStore {
    pub fn new(maxsize: usize, ttl: Duration) -> Self {
        Self {
            maxsize,
            ttl,
            entries: Mutex::new(Vec::new()),
        }
    }

    pub fn add(&self, key: &str) {
        let mut entries = self.entries.lock().expect("id store poisoned");
        let now = Instant::now();
        entries.retain(|(k, exp)| k != key && *exp >= now);
        entries.push((key.to_string(), now + self.ttl));
        while entries.len() > self.maxsize {
            entries.remove(0);
        }
    }

    pub fn contains(&self, key: &str) -> bool {
        let mut entries = self.entries.lock().expect("id store poisoned");
        let now = Instant::now();
        entries.retain(|(_, exp)| *exp >= now);
        let Some(pos) = entries.iter().position(|(k, _)| k == key) else {
            return false;
        };
        let item = entries.remove(pos);
        entries.push(item);
        true
    }
}
