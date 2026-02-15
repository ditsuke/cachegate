use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use lru::LruCache;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tracing::warn;

use crate::cache::{CacheBackend, CacheEntry, CacheKey, CacheStats};
use crate::config::CachePolicy;

struct MemoryEntry {
    bytes: Bytes,
    content_type: Option<String>,
    size_bytes: u64,
}

struct CacheState {
    lru: LruCache<CacheKey, MemoryEntry>,
    total_bytes: u64,
    max_bytes: u64,
}

pub struct MemoryCache {
    state: std::sync::Arc<Mutex<CacheState>>,

    inserts: AtomicU64,
}

impl MemoryCache {
    pub fn new(policy: CachePolicy) -> Self {
        let lru = LruCache::unbounded();
        let max_bytes = policy.max_memory.as_u64();
        let state = CacheState {
            lru,
            total_bytes: 0,
            max_bytes,
        };

        Self {
            state: std::sync::Arc::new(Mutex::new(state)),
            inserts: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl CacheBackend for MemoryCache {
    #[tracing::instrument(skip(self))]
    async fn get(&self, key: &CacheKey) -> Option<CacheEntry> {
        enum LookupResult {
            Hit(CacheEntry),
            Miss,
            Expired,
        }

        let mut state = self.state.lock().await;
        let now = Instant::now();
        let entry = state
            .lru
            .get(key)
            .map(|entry| {
                    LookupResult::Hit(CacheEntry::new(
                        entry.bytes.clone(),
                        entry.content_type.clone(),
                    ))
                }
            })
            .unwrap_or(LookupResult::Miss);

        match entry {
            LookupResult::Hit(hit) => return Some(hit),
            LookupResult::Expired => {
                if let Some(removed) = state.lru.pop(key) {
                    state.total_bytes = state.total_bytes.saturating_sub(removed.size_bytes);
                }
                None
            }
            LookupResult::Miss => None,
        }
    }

    #[tracing::instrument(skip(self, bytes, content_type))]
    async fn put(&self, key: CacheKey, bytes: Bytes, content_type: Option<String>) {
        let mut state = self.state.lock().await;
        let size_bytes = bytes.len() as u64;
        if size_bytes > state.max_bytes {
            warn!(
                bucket_id = %key.bucket_id,
                path = %key.path,
                size_bytes,
                max_bytes = state.max_bytes,
                "cache entry too large; skipping"
            );
            return;
        }

        if let Some(existing) = state.lru.pop(&key) {
            state.total_bytes = state.total_bytes.saturating_sub(existing.size_bytes);
        }

        let entry = MemoryEntry {
            bytes,
            content_type,
            size_bytes,
        };

        state.lru.put(key, entry);
        state.total_bytes = state.total_bytes.saturating_add(size_bytes);

        while state.total_bytes > state.max_bytes {
            if let Some((_key, removed)) = state.lru.pop_lru() {
                state.total_bytes = state.total_bytes.saturating_sub(removed.size_bytes);
            } else {
                break;
            }
        }
        self.inserts.fetch_add(1, Ordering::Relaxed);
    }

    #[tracing::instrument(skip(self))]
    async fn stats(&self) -> CacheStats {
        CacheStats {
            inserts: self.inserts.load(Ordering::Relaxed),
        }
    }
}
