use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};

pub mod foyer;
pub mod memory;

pub use memory::MemoryCache;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub bytes: Bytes,
    pub content_type: Option<String>,
}

impl CacheEntry {
    pub fn new(bytes: Bytes, content_type: Option<String>) -> Self {
        Self {
            bytes,
            content_type,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheKey {
    pub bucket_id: String,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct CacheStats {
    pub inserts: u64,
}

impl CacheKey {
    pub fn new(bucket_id: String, path: String) -> Self {
        Self { bucket_id, path }
    }
}

impl PartialEq for CacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.bucket_id == other.bucket_id && self.path == other.path
    }
}

impl Eq for CacheKey {}

impl Hash for CacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.bucket_id.hash(state);
        self.path.hash(state);
    }
}

#[async_trait]
pub trait CacheBackend: Send + Sync {
    async fn get(&self, key: &CacheKey) -> Option<CacheEntry>;
    async fn put(&self, key: CacheKey, bytes: Bytes, content_type: Option<String>);
    async fn stats(&self) -> CacheStats;
}
