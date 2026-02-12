use serde::Serialize;
use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug)]
struct Histogram {
    buckets_ms: Vec<u64>,
    counts: Vec<AtomicU64>,
    sum_ms: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    fn new(mut buckets_ms: Vec<u64>) -> Self {
        buckets_ms.sort_unstable();
        buckets_ms.dedup();

        let counts = buckets_ms.iter().map(|_| AtomicU64::new(0)).collect();
        Self {
            buckets_ms,
            counts,
            sum_ms: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    fn observe(&self, value_ms: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ms.fetch_add(value_ms, Ordering::Relaxed);

        for (idx, bucket) in self.buckets_ms.iter().enumerate() {
            if value_ms <= *bucket {
                self.counts[idx].fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
    }

    fn snapshot(&self) -> HistogramSnapshot {
        let mut counts = Vec::with_capacity(self.counts.len());
        for count in &self.counts {
            counts.push(count.load(Ordering::Relaxed));
        }
        HistogramSnapshot {
            buckets_ms: self.buckets_ms.clone(),
            counts,
            sum_ms: self.sum_ms.load(Ordering::Relaxed),
            count: self.count.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone)]
struct HistogramSnapshot {
    buckets_ms: Vec<u64>,
    counts: Vec<u64>,
    sum_ms: u64,
    count: u64,
}

#[derive(Debug)]
pub struct Metrics {
    requests_total: AtomicU64,
    auth_fail_total: AtomicU64,
    cache_hit_total: AtomicU64,
    cache_miss_total: AtomicU64,
    upstream_ok_total: AtomicU64,
    upstream_err_total: AtomicU64,
    cache_entries: AtomicU64,
    cache_bytes: AtomicU64,
    upstream_latency_ms: Histogram,
}

impl Metrics {
    pub fn new() -> Self {
        let buckets = vec![1, 5, 10, 25, 50, 100, 250, 500, 1000, 2000, 5000];
        Self {
            requests_total: AtomicU64::new(0),
            auth_fail_total: AtomicU64::new(0),
            cache_hit_total: AtomicU64::new(0),
            cache_miss_total: AtomicU64::new(0),
            upstream_ok_total: AtomicU64::new(0),
            upstream_err_total: AtomicU64::new(0),
            cache_entries: AtomicU64::new(0),
            cache_bytes: AtomicU64::new(0),
            upstream_latency_ms: Histogram::new(buckets),
        }
    }

    pub fn inc_requests(&self) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_auth_fail(&self) {
        self.auth_fail_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_cache_hit(&self) {
        self.cache_hit_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_cache_miss(&self) {
        self.cache_miss_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_upstream_ok(&self) {
        self.upstream_ok_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_upstream_err(&self) {
        self.upstream_err_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn observe_upstream_latency_ms(&self, value_ms: u64) {
        self.upstream_latency_ms.observe(value_ms);
    }

    pub fn set_cache_stats(&self, entries: usize, bytes: u64) {
        self.cache_entries.store(entries as u64, Ordering::Relaxed);
        self.cache_bytes.store(bytes, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            requests_total: self.requests_total.load(Ordering::Relaxed),
            auth_fail_total: self.auth_fail_total.load(Ordering::Relaxed),
            cache_hit_total: self.cache_hit_total.load(Ordering::Relaxed),
            cache_miss_total: self.cache_miss_total.load(Ordering::Relaxed),
            upstream_ok_total: self.upstream_ok_total.load(Ordering::Relaxed),
            upstream_err_total: self.upstream_err_total.load(Ordering::Relaxed),
            cache_entries: self.cache_entries.load(Ordering::Relaxed),
            cache_bytes: self.cache_bytes.load(Ordering::Relaxed),
        }
    }

    pub fn render_prometheus(&self) -> String {
        let snapshot = self.snapshot();
        let histogram = self.upstream_latency_ms.snapshot();
        let mut out = String::new();

        writeln!(out, "cachegate_requests_total {}", snapshot.requests_total).ok();
        writeln!(
            out,
            "cachegate_auth_fail_total {}",
            snapshot.auth_fail_total
        )
        .ok();
        writeln!(
            out,
            "cachegate_cache_hit_total {}",
            snapshot.cache_hit_total
        )
        .ok();
        writeln!(
            out,
            "cachegate_cache_miss_total {}",
            snapshot.cache_miss_total
        )
        .ok();
        writeln!(
            out,
            "cachegate_upstream_ok_total {}",
            snapshot.upstream_ok_total
        )
        .ok();
        writeln!(
            out,
            "cachegate_upstream_err_total {}",
            snapshot.upstream_err_total
        )
        .ok();
        writeln!(out, "cachegate_cache_entries {}", snapshot.cache_entries).ok();
        writeln!(out, "cachegate_cache_bytes {}", snapshot.cache_bytes).ok();

        let mut running = 0u64;
        for (idx, bucket) in histogram.buckets_ms.iter().enumerate() {
            running = running.saturating_add(histogram.counts[idx]);
            writeln!(
                out,
                "cachegate_upstream_latency_ms_bucket{{le=\"{}\"}} {}",
                bucket, running
            )
            .ok();
        }
        writeln!(
            out,
            "cachegate_upstream_latency_ms_bucket{{le=\"+Inf\"}} {}",
            histogram.count
        )
        .ok();
        writeln!(
            out,
            "cachegate_upstream_latency_ms_sum {}",
            histogram.sum_ms
        )
        .ok();
        writeln!(
            out,
            "cachegate_upstream_latency_ms_count {}",
            histogram.count
        )
        .ok();

        out
    }
}

#[derive(Debug, Serialize)]
pub struct MetricsSnapshot {
    pub requests_total: u64,
    pub auth_fail_total: u64,
    pub cache_hit_total: u64,
    pub cache_miss_total: u64,
    pub upstream_ok_total: u64,
    pub upstream_err_total: u64,
    pub cache_entries: u64,
    pub cache_bytes: u64,
}
