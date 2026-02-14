use mixtrics::metrics::{BoxedCounterVec, BoxedHistogramVec, BoxedRegistry};
use mixtrics::registry::prometheus_0_13::PrometheusMetricsRegistry;
use prometheus_0_13::proto::MetricFamily;
use prometheus_0_13::{Encoder, Registry, TextEncoder};
use serde::Serialize;
use std::borrow::Cow;

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
    mixtrics_registry: PrometheusMetricsRegistry,
    requests_total: BoxedCounterVec,
    auth_fail_total: BoxedCounterVec,
    cache_hit_total: BoxedCounterVec,
    cache_miss_total: BoxedCounterVec,
    upstream_ok_total: BoxedCounterVec,
    upstream_err_total: BoxedCounterVec,
    upstream_latency_ms: BoxedHistogramVec,
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();
        let mixtrics_registry = PrometheusMetricsRegistry::new(registry.clone());
        let registry_handle: BoxedRegistry = Box::new(mixtrics_registry.clone());

        let requests_total = registry_handle.register_counter_vec(
            "cachegate_requests_total".into(),
            "Total requests served by cachegate".into(),
            &["method", "status"],
        );
        let auth_fail_total = registry_handle.register_counter_vec(
            "cachegate_auth_fail_total".into(),
            "Total authentication failures".into(),
            &["method"],
        );
        let cache_hit_total = registry_handle.register_counter_vec(
            "cachegate_cache_hit_total".into(),
            "Total cache hits".into(),
            &["method"],
        );
        let cache_miss_total = registry_handle.register_counter_vec(
            "cachegate_cache_miss_total".into(),
            "Total cache misses".into(),
            &["method"],
        );
        let upstream_ok_total = registry_handle.register_counter_vec(
            "cachegate_upstream_ok_total".into(),
            "Total upstream successes".into(),
            &["method"],
        );
        let upstream_err_total = registry_handle.register_counter_vec(
            "cachegate_upstream_err_total".into(),
            "Total upstream errors".into(),
            &["method", "error_kind"],
        );

        let buckets = vec![
            1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2000.0, 5000.0,
        ];
        let upstream_latency_ms = registry_handle.register_histogram_vec_with_buckets(
            "cachegate_upstream_latency_ms".into(),
            "Upstream request latency in milliseconds".into(),
            &["method"],
            buckets,
        );

        Self {
            registry,
            mixtrics_registry,
            requests_total,
            auth_fail_total,
            cache_hit_total,
            cache_miss_total,
            upstream_ok_total,
            upstream_err_total,
            upstream_latency_ms,
        }
    }

    pub fn registry(&self) -> BoxedRegistry {
        Box::new(self.mixtrics_registry.clone())
    }

    pub fn inc_requests(&self, method: &str, status: &str) {
        self.requests_total
            .counter(&[owned_label(method), owned_label(status)])
            .increase(1);
    }

    pub fn inc_auth_fail(&self, method: &str) {
        self.auth_fail_total
            .counter(&[owned_label(method)])
            .increase(1);
    }

    pub fn inc_cache_hit(&self, method: &str) {
        self.cache_hit_total
            .counter(&[owned_label(method)])
            .increase(1);
    }

    pub fn inc_cache_miss(&self, method: &str) {
        self.cache_miss_total
            .counter(&[owned_label(method)])
            .increase(1);
    }

    pub fn inc_upstream_ok(&self, method: &str) {
        self.upstream_ok_total
            .counter(&[owned_label(method)])
            .increase(1);
    }

    pub fn inc_upstream_err(&self, method: &str, error_kind: UpstreamErrorKind) {
        self.upstream_err_total
            .counter(&[owned_label(method), owned_label(error_kind.as_str())])
            .increase(1);
    }

    pub fn observe_upstream_latency_ms(&self, method: &str, value_ms: u64) {
        self.upstream_latency_ms
            .histogram(&[owned_label(method)])
            .record(value_ms as f64);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let metric_families = self.registry.gather();
        MetricsSnapshot {
            requests_total: sum_counter(&metric_families, "cachegate_requests_total"),
            auth_fail_total: sum_counter(&metric_families, "cachegate_auth_fail_total"),
            cache_hit_total: sum_counter(&metric_families, "cachegate_cache_hit_total"),
            cache_miss_total: sum_counter(&metric_families, "cachegate_cache_miss_total"),
            upstream_ok_total: sum_counter(&metric_families, "cachegate_upstream_ok_total"),
            upstream_err_total: sum_counter(&metric_families, "cachegate_upstream_err_total"),
            cache_entries: sum_gauge(&metric_families, "cachegate_cache_entries"),
            cache_bytes: sum_gauge(&metric_families, "cachegate_cache_bytes"),
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

fn owned_label(value: &str) -> Cow<'static, str> {
    Cow::Owned(value.to_string())
}

fn sum_counter(metric_families: &[MetricFamily], name: &str) -> u64 {
    metric_families
        .iter()
        .find(|family| family.get_name() == name)
        .map(|family| {
            let mut total = 0f64;
            for metric in family.get_metric() {
                total += metric.get_counter().get_value();
            }
            total.round() as u64
        })
        .unwrap_or(0)
}

fn sum_gauge(metric_families: &[MetricFamily], name: &str) -> u64 {
    metric_families
        .iter()
        .find(|family| family.get_name() == name)
        .map(|family| {
            let mut total = 0f64;
            for metric in family.get_metric() {
                total += metric.get_gauge().get_value();
            }
            total.round() as u64
        })
        .unwrap_or(0)
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
