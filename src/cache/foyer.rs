use anyhow::{Context, anyhow};
use async_trait::async_trait;
use bytes::Bytes;
use foyer::{DirectFsDeviceOptions, Engine, HybridCache, HybridCacheBuilder};
use std::path::PathBuf;
use tracing::{info, warn};

use crate::cache::{CacheBackend, CacheEntry as CacheEntryInner, CacheKey, CacheStats};
use crate::config::CachePolicy;

type FoyerHybridCache = HybridCache<CacheKey, CacheEntryInner>;

pub struct FoyerCache {
    cache: FoyerHybridCache,
    ttl_seconds: u64,
    max_bytes: u64,
}

impl FoyerCache {
    pub async fn new(policy: CachePolicy) -> Result<FoyerCache, anyhow::Error> {
        let max_bytes_memory = policy.max_memory.as_u64();
        if max_bytes_memory == 0 || policy.ttl_seconds == 0 {
            return Err(anyhow!("Bad policy: 0 max_bytes_memory/ttl_seconds"));
        }

        let disk_capacity = policy.max_disk.as_u64();
        let disk_path = policy
            .disk_path
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp/cachegate_cache"));

        if disk_capacity == 0 {
            return Err(anyhow!("max_disk must be > 0 when using Foyer cache"));
        }

        std::fs::create_dir_all(&disk_path).context("failed to create disk cache directory")?;

        let device_options =
            DirectFsDeviceOptions::new(&disk_path).with_capacity(disk_capacity as usize);

        let cache = HybridCacheBuilder::new()
            .with_name("cachegate")
            .memory(max_bytes_memory as usize)
            .storage(Engine::Large)
            .with_device_options(device_options)
            .build()
            .await
            .context("Failed to initialise cache")?;

        info!(
            memory_capacity_bytes = max_bytes_memory,
            disk_capacity_bytes = disk_capacity,
            disk_path = %disk_path.display(),
            ttl_seconds = policy.ttl_seconds,
            "Foyer hybrid cache initialized"
        );

        Ok(Self {
            cache,
            ttl_seconds: policy.ttl_seconds,
            max_bytes: max_bytes_memory,
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
        if self.ttl_seconds == 0 {
            return;
        }

        let entry = CacheEntryInner::new(bytes, content_type);
        self.cache.insert(key, entry);
    }

    #[tracing::instrument(skip(self))]
    async fn stats(&self) -> CacheStats {
        CacheStats {
            entries: 0,
            total_bytes: self.max_bytes,
        }
    }
}
