use anyhow::{Context, anyhow};
use async_trait::async_trait;
use bytes::Bytes;
use foyer::{
    BlockEngineConfig, DeviceBuilder, FsDeviceBuilder, HybridCache, HybridCacheBuilder,
    PsyncIoEngineConfig, S3FifoConfig,
};
use mixtrics::metrics::BoxedRegistry;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{info, warn};

use crate::cache::{CacheBackend, CacheEntry as CacheEntryInner, CacheKey, CacheStats};
use crate::config::CachePolicy;

type FoyerHybridCache = HybridCache<CacheKey, CacheEntryInner>;

pub struct FoyerCache {
    cache: FoyerHybridCache,
    inserts: AtomicU64,
}

impl FoyerCache {
    pub async fn new(
        policy: CachePolicy,
        registry: BoxedRegistry,
    ) -> Result<FoyerCache, anyhow::Error> {
        let max_bytes_memory = policy.max_memory.as_u64();
        if max_bytes_memory == 0 {
            return Err(anyhow!("Bad policy: 0 max_bytes_memory"));
        }

        let disk_capacity = policy.max_disk.as_u64();
        let disk_path = policy.disk_path.map(PathBuf::from);
        if disk_capacity == 0 && disk_path.is_some() {
            warn!("disk_path set but max_disk is 0; running in memory-only mode");
        }

        let builder = HybridCacheBuilder::new()
            .with_policy(foyer::HybridCachePolicy::WriteOnInsertion)
            .with_name("cachegate")
            .with_metrics_registry(registry)
            .memory(max_bytes_memory as usize)
            .with_shards(10) // TODO: have this in config
            .with_eviction_config(S3FifoConfig::default());

        let cache = if disk_capacity == 0 {
            let cache = builder
                .storage()
                .build()
                .await
                .context("Failed to initialise cache")?;
            info!(
                memory_capacity_bytes = max_bytes_memory,
                "Foyer cache initialized (memory-only)"
            );
            cache
        } else {
            let disk_path = disk_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("/tmp/cachegate_cache"));
            std::fs::create_dir_all(&disk_path).context("failed to create disk cache directory")?;

            let device = FsDeviceBuilder::new(&disk_path)
                .with_capacity(disk_capacity as usize)
                // TODO: Allow throttling config
                // TODO: Use direct unbuffered i/o on linux!
                .build()
                .context("failed to build disk cache device")?;

            let cache = builder
                .storage()
                .with_io_engine_config(PsyncIoEngineConfig::new())
                .with_engine_config(BlockEngineConfig::new(device))
                .build()
                .await
                .context("Failed to initialise cache")?;
            info!(
                memory_capacity_bytes = max_bytes_memory,
                disk_capacity_bytes = disk_capacity,
                disk_path = %disk_path.display(),
                "Foyer hybrid cache initialized"
            );
            cache
        };

        Ok(Self {
            cache,
            inserts: AtomicU64::new(0),
        })
    }
}

#[async_trait]
impl CacheBackend for FoyerCache {
    #[tracing::instrument(skip(self))]
    async fn get(&self, key: &CacheKey) -> Option<CacheEntryInner> {
        match self.cache.get(key).await {
            Ok(Some(entry)) => {
                let inner: &CacheEntryInner = entry.value();
                Some(inner.clone())
            }
            Ok(None) => None,
            Err(e) => {
                warn!(error = %e, "Foyer cache get failed");
                None
            }
        }
    }

    #[tracing::instrument(skip(self, bytes, content_type))]
    async fn put(&self, key: CacheKey, bytes: Bytes, content_type: Option<String>) {
        let entry = CacheEntryInner::new(bytes, content_type);
        self.cache.insert(key, entry);
        self.inserts.fetch_add(1, Ordering::Relaxed);
    }

    #[tracing::instrument(skip(self))]
    async fn stats(&self) -> CacheStats {
        CacheStats {
            inserts: self.inserts.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use bytesize::ByteSize;
    use tempfile::TempDir;

    use super::*;
    use crate::cache::{CacheBackend, CacheKey};

    fn noop_registry() -> BoxedRegistry {
        Box::new(mixtrics::registry::noop::NoopMetricsRegistry)
    }

    fn make_policy(
        max_memory_bytes: u64,
        max_disk_bytes: u64,
        disk_path: Option<String>,
    ) -> CachePolicy {
        CachePolicy {
            max_memory: ByteSize(max_memory_bytes),
            max_object_size: ByteSize(max_memory_bytes),
            max_disk: ByteSize(max_disk_bytes),
            disk_path,
        }
    }

    #[tokio::test]
    async fn new_rejects_zero_max_memory() {
        let policy = make_policy(0, 0, None);
        let result = FoyerCache::new(policy, noop_registry()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn new_allows_zero_max_disk() {
        let policy = make_policy(60, 0, None);
        let result = FoyerCache::new(policy, noop_registry()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn new_succeeds_with_valid_policy() {
        let disk_dir = TempDir::new().unwrap();
        let policy = make_policy(
            60,
            1024 * 1024,
            Some(disk_dir.path().to_string_lossy().to_string()),
        );
        let result = FoyerCache::new(policy, noop_registry()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn get_returns_none_for_missing_key() {
        let disk_dir = TempDir::new().unwrap();
        let policy = make_policy(
            60,
            1024 * 1024,
            Some(disk_dir.path().to_string_lossy().to_string()),
        );
        let cache = FoyerCache::new(policy, noop_registry()).await.unwrap();

        let key = CacheKey::new("bucket".to_string(), "nonexistent.txt".to_string());
        let result = cache.get(&key).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn put_and_get_roundtrip() {
        let disk_dir = TempDir::new().unwrap();
        let policy = make_policy(
            60,
            1024 * 1024,
            Some(disk_dir.path().to_string_lossy().to_string()),
        );
        let cache = FoyerCache::new(policy, noop_registry()).await.unwrap();

        let key = CacheKey::new("bucket".to_string(), "test.txt".to_string());
        let data = Bytes::from(b"hello world".to_vec());
        let content_type = Some("text/plain".to_string());

        cache
            .put(key.clone(), data.clone(), content_type.clone())
            .await;

        let result = cache.get(&key).await;
        assert!(result.is_some());
        let entry = result.unwrap();
        assert_eq!(entry.bytes, data);
        assert_eq!(entry.content_type, content_type);
    }
}
