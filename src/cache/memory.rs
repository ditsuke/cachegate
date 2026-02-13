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
    expires_at: Instant,
}

struct CacheState {
    lru: LruCache<CacheKey, MemoryEntry>,
    total_bytes: u64,
    max_bytes: u64,
    ttl_seconds: u64,
}

#[derive(Clone)]
pub struct MemoryCache {
    state: std::sync::Arc<Mutex<CacheState>>,
}

impl MemoryCache {
    pub fn new(policy: CachePolicy) -> Self {
        let lru = LruCache::unbounded();
        let state = CacheState {
            lru,
            total_bytes: 0,
            max_bytes: policy.max_bytes,
            ttl_seconds: policy.ttl_seconds,
        };

        Self {
            state: std::sync::Arc::new(Mutex::new(state)),
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
                if entry.expires_at <= now {
                    LookupResult::Expired
                } else {
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
        if state.max_bytes == 0 || state.ttl_seconds == 0 {
            return;
        }

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

        let expires_at = Instant::now() + Duration::from_secs(state.ttl_seconds);
        let entry = MemoryEntry {
            bytes,
            content_type,
            size_bytes,
            expires_at,
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
    }

    #[tracing::instrument(skip(self))]
    async fn stats(&self) -> CacheStats {
        let state = self.state.lock().await;
        CacheStats {
            entries: state.lru.len(),
            total_bytes: state.total_bytes,
        }
    }
}
