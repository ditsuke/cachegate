use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

use crate::cache::CacheKey;

pub enum InflightPermit<R: Send + 'static> {
    Leader(InflightGuard<R>),
    Follower(Arc<InflightEntry<R>>),
}

pub struct InflightGuard<R: Send + 'static> {
    inflight: Arc<Inflight<R>>,
    key: CacheKey,
    entry: Arc<InflightEntry<R>>,
    released: bool,
}

pub struct InflightEntry<R: Send + 'static> {
    notify: Notify,
    result: Mutex<Option<R>>,
}

#[derive(Default)]
pub struct Inflight<R: Send + 'static> {
    inner: Mutex<HashMap<CacheKey, Arc<InflightEntry<R>>>>,
}

impl<R: Send + 'static> Inflight<R> {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub async fn acquire(self: &Arc<Self>, key: &CacheKey) -> InflightPermit<R> {
        let mut guard = self.inner.lock().await;
        if let Some(existing) = guard.get(key) {
            return InflightPermit::Follower(existing.clone());
        }

        let entry = Arc::new(InflightEntry {
            notify: Notify::new(),
            result: Mutex::new(None),
        });
        guard.insert(key.clone(), entry.clone());
        InflightPermit::Leader(InflightGuard {
            inflight: Arc::clone(self),
            key: key.clone(),
            entry,
            released: false,
        })
    }

    async fn release(&self, key: &CacheKey, entry: &Arc<InflightEntry<R>>) {
        let mut guard = self.inner.lock().await;
        if let Some(current) = guard.get(key)
            && Arc::ptr_eq(current, entry)
        {
            guard.remove(key);
            entry.notify.notify_waiters();
        }
    }
}

impl<R: Send + 'static> InflightGuard<R> {
    pub async fn complete(mut self, result: R) {
        {
            let mut guard = self.entry.result.lock().await;
            *guard = Some(result);
        }
        self.inflight.release(&self.key, &self.entry).await;
        self.released = true;
    }
}

impl<R: Send + 'static> Drop for InflightGuard<R> {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let inflight = Arc::clone(&self.inflight);
        let key = self.key.clone();
        let entry = Arc::clone(&self.entry);
        tokio::spawn(async move {
            inflight.release(&key, &entry).await;
        });
    }
}

impl<R: Clone + Send + 'static> InflightEntry<R> {
    pub async fn wait(&self) -> Option<R> {
        self.notify.notified().await;
        let guard = self.result.lock().await;
        guard.clone()
    }
}
