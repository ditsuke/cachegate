use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

use crate::cache::CacheKey;

pub enum InflightPermit {
    Leader(Arc<Notify>),
    Follower(Arc<Notify>),
}

#[derive(Default)]
pub struct Inflight {
    inner: Mutex<HashMap<CacheKey, Arc<Notify>>>,
}

impl Inflight {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub async fn acquire(&self, key: &CacheKey) -> InflightPermit {
        let mut guard = self.inner.lock().await;
        if let Some(existing) = guard.get(key) {
            return InflightPermit::Follower(existing.clone());
        }

        let notify = Arc::new(Notify::new());
        guard.insert(key.clone(), notify.clone());
        InflightPermit::Leader(notify)
    }

    pub async fn release(&self, key: &CacheKey, notify: Arc<Notify>) {
        let mut guard = self.inner.lock().await;
        if let Some(current) = guard.get(key) {
            if Arc::ptr_eq(current, &notify) {
                guard.remove(key);
            }
        }
        notify.notify_waiters();
    }
}
