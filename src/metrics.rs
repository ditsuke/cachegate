use prometheus::core::Collector;
use prometheus::{
    CounterVec, Encoder, HistogramOpts, HistogramVec, IntGauge, Opts, Registry, TextEncoder,
};
use serde::Serialize;

#[derive(Debug, Clone, Copy)]
pub enum UpstreamErrorKind {
    NotFound,
    PermissionDenied,
    Unauthenticated,
    NotSupported,
    Precondition,
    Other,
}

impl UpstreamErrorKind {
    pub fn from_store_error(error: &object_store::Error) -> Self {
        match error {
            object_store::Error::NotFound { .. } => Self::NotFound,
            object_store::Error::PermissionDenied { .. } => Self::PermissionDenied,
            object_store::Error::Unauthenticated { .. } => Self::Unauthenticated,
            object_store::Error::NotSupported { .. } => Self::NotSupported,
            object_store::Error::Precondition { .. } => Self::Precondition,
            _ => Self::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::PermissionDenied => "permission_denied",
            Self::Unauthenticated => "unauthenticated",
            Self::NotSupported => "not_supported",
            Self::Precondition => "precondition",
            Self::Other => "other",
        }
    }
}

#[derive(Debug)]
pub struct Metrics {
    registry: Registry,
    requests_total: CounterVec,
    auth_fail_total: CounterVec,
    cache_hit_total: CounterVec,
    cache_miss_total: CounterVec,
    upstream_ok_total: CounterVec,
    upstream_err_total: CounterVec,
    cache_entries: IntGauge,
    cache_bytes: IntGauge,
    upstream_latency_ms: HistogramVec,
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();

        let requests_total = CounterVec::new(
            Opts::new(
                "cachegate_requests_total",
                "Total requests served by cachegate",
            ),
            &["method", "status"],
        )
        .expect("requests_total metrics");
        let auth_fail_total = CounterVec::new(
            Opts::new("cachegate_auth_fail_total", "Total authentication failures"),
            &["method"],
        )
        .expect("auth_fail_total metrics");
        let cache_hit_total = CounterVec::new(
            Opts::new("cachegate_cache_hit_total", "Total cache hits"),
            &["method"],
        )
        .expect("cache_hit_total metrics");
        let cache_miss_total = CounterVec::new(
            Opts::new("cachegate_cache_miss_total", "Total cache misses"),
            &["method"],
        )
        .expect("cache_miss_total metrics");
        let upstream_ok_total = CounterVec::new(
            Opts::new("cachegate_upstream_ok_total", "Total upstream successes"),
            &["method"],
        )
        .expect("upstream_ok_total metrics");
        let upstream_err_total = CounterVec::new(
            Opts::new("cachegate_upstream_err_total", "Total upstream errors"),
            &["method", "error_kind"],
        )
        .expect("upstream_err_total metrics");
        let cache_entries = IntGauge::new("cachegate_cache_entries", "Current cache entry count")
            .expect("cache_entries metrics");
        let cache_bytes = IntGauge::new("cachegate_cache_bytes", "Current cache bytes")
            .expect("cache_bytes metrics");

        let buckets = vec![
            1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2000.0, 5000.0,
        ];
        let upstream_latency_ms = HistogramVec::new(
            HistogramOpts::new(
                "cachegate_upstream_latency_ms",
                "Upstream request latency in milliseconds",
            )
            .buckets(buckets),
            &["method"],
        )
        .expect("upstream_latency_ms metrics");

        registry
            .register(Box::new(requests_total.clone()))
            .expect("register requests_total");
        registry
            .register(Box::new(auth_fail_total.clone()))
            .expect("register auth_fail_total");
        registry
            .register(Box::new(cache_hit_total.clone()))
            .expect("register cache_hit_total");
        registry
            .register(Box::new(cache_miss_total.clone()))
            .expect("register cache_miss_total");
        registry
            .register(Box::new(upstream_ok_total.clone()))
            .expect("register upstream_ok_total");
        registry
            .register(Box::new(upstream_err_total.clone()))
            .expect("register upstream_err_total");
        registry
            .register(Box::new(cache_entries.clone()))
            .expect("register cache_entries");
        registry
            .register(Box::new(cache_bytes.clone()))
            .expect("register cache_bytes");
        registry
            .register(Box::new(upstream_latency_ms.clone()))
            .expect("register upstream_latency_ms");

        Self {
            registry,
            requests_total,
            auth_fail_total,
            cache_hit_total,
            cache_miss_total,
            upstream_ok_total,
            upstream_err_total,
            cache_entries,
            cache_bytes,
            upstream_latency_ms,
        }
    }

    pub fn inc_requests(&self, method: &str, status: &str) {
        self.requests_total
            .with_label_values(&[method, status])
            .inc();
    }

    pub fn inc_auth_fail(&self, method: &str) {
        self.auth_fail_total.with_label_values(&[method]).inc();
    }

    pub fn inc_cache_hit(&self, method: &str) {
        self.cache_hit_total.with_label_values(&[method]).inc();
    }

    pub fn inc_cache_miss(&self, method: &str) {
        self.cache_miss_total.with_label_values(&[method]).inc();
    }

    pub fn inc_upstream_ok(&self, method: &str) {
        self.upstream_ok_total.with_label_values(&[method]).inc();
    }

    pub fn inc_upstream_err(&self, method: &str, error_kind: UpstreamErrorKind) {
        self.upstream_err_total
            .with_label_values(&[method, error_kind.as_str()])
            .inc();
    }

    pub fn observe_upstream_latency_ms(&self, method: &str, value_ms: u64) {
        self.upstream_latency_ms
            .with_label_values(&[method])
            .observe(value_ms as f64);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            requests_total: sum_counter(&self.requests_total),
            auth_fail_total: sum_counter(&self.auth_fail_total),
            cache_hit_total: sum_counter(&self.cache_hit_total),
            cache_miss_total: sum_counter(&self.cache_miss_total),
            upstream_ok_total: sum_counter(&self.upstream_ok_total),
            upstream_err_total: sum_counter(&self.upstream_err_total),
            cache_entries: self.cache_entries.get() as u64,
            cache_bytes: self.cache_bytes.get() as u64,
        }
    }

    pub fn render_prometheus(&self) -> String {
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        let encoder = TextEncoder::new();
        if encoder.encode(&metric_families, &mut buffer).is_err() {
            return String::new();
        }
        String::from_utf8(buffer).unwrap_or_default()
    }
}

fn sum_counter(counter: &CounterVec) -> u64 {
    let mut total = 0f64;
    for family in counter.collect() {
        for metric in family.get_metric() {
            total += metric.get_counter().get_value();
        }
    }
    total.round() as u64
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
