use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, Response, StatusCode, header};
use axum::response::IntoResponse;
use bytes::Bytes;
use object_store::ObjectStoreExt;
use serde::Serialize;
use std::time::Instant;
use tracing::{info, info_span, warn};

use crate::auth::AuthState;
use crate::cache::{CacheBackend, CacheEntry, CacheKey};
use crate::inflight::{Inflight, InflightPermit};
use crate::metrics::Metrics;
use crate::store::StoreMap;

pub struct AppState {
    pub stores: StoreMap,
    pub auth: AuthState,
    pub cache: Arc<dyn CacheBackend>,
    pub inflight: Arc<Inflight>,
    pub metrics: Arc<Metrics>,
}

pub async fn get_object(
    State(state): State<Arc<AppState>>,
    Path((bucket_id, path)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response<Body>, AppError> {
    let start = Instant::now();
    let span = info_span!(
        "get_object",
        bucket_id = %bucket_id,
        path = %path,
        cache = tracing::field::Empty,
        inflight = tracing::field::Empty,
        status = tracing::field::Empty,
        bytes = tracing::field::Empty,
        elapsed_ms = tracing::field::Empty
    );
    let _enter = span.enter();

    state.metrics.inc_requests();
    let key = CacheKey::new(bucket_id.clone(), path.clone());
    let mut response_bytes: Option<usize> = None;

    let result = 'request: {
        let sig = match params.get("sig") {
            Some(sig) => sig,
            None => {
                state.metrics.inc_auth_fail();
                break 'request Err(AppError::unauthorized("missing signature"));
            }
        };

        if path.is_empty() {
            break 'request Err(AppError::bad_request("missing object path"));
        }

        if let Err(err) = state.auth.verify("GET", &bucket_id, &path, sig) {
            warn!(bucket_id = %bucket_id, path = %path, error = %err, "signature verification failed");
            state.metrics.inc_auth_fail();
            break 'request Err(AppError::unauthorized("invalid signature"));
        }

        if let Some(entry) = state.cache.get(&key).await {
            state.metrics.inc_cache_hit();
            span.record("cache", "hit");
            response_bytes = Some(entry.bytes.len());
            info!(bucket_id = %bucket_id, path = %path, bytes = entry.bytes.len(), "served from cache");
            break 'request Ok(build_response(entry, true));
        }

        state.metrics.inc_cache_miss();
        span.record("cache", "miss");
        info!(bucket_id = %bucket_id, path = %path, "cache miss");

        let permit = state.inflight.acquire(&key).await;
        match permit {
            InflightPermit::Leader(notify) => {
                span.record("inflight", "leader");
                info!(bucket_id = %bucket_id, path = %path, "inflight leader fetch");
                let result = fetch_and_cache_entry(&state, &key, &bucket_id, &path)
                    .await
                    .map(|entry| {
                        response_bytes = Some(entry.bytes.len());
                        build_response(entry, false)
                    });
                state.inflight.release(&key, notify).await;
                break 'request result;
            }
            InflightPermit::Follower(notify) => {
                span.record("inflight", "follower");
                info!(bucket_id = %bucket_id, path = %path, "awaiting inflight leader");
                notify.notified().await;
                if let Some(entry) = state.cache.get(&key).await {
                    response_bytes = Some(entry.bytes.len());
                    info!(bucket_id = %bucket_id, path = %path, bytes = entry.bytes.len(), "served from cache after inflight");
                    break 'request Ok(build_response(entry, true));
                }
                break 'request fetch_and_cache_entry(&state, &key, &bucket_id, &path)
                    .await
                    .map(|entry| {
                        response_bytes = Some(entry.bytes.len());
                        build_response(entry, false)
                    });
            }
        }
    };

    span.record("elapsed_ms", start.elapsed().as_millis().to_string());
    match &result {
        Ok(response) => {
            span.record("status", response.status().to_string());
            if let Some(bytes) = response_bytes {
                span.record("bytes", bytes.to_string());
            }
        }
        Err(err) => {
            span.record("status", err.status.to_string());
            span.record("bytes", "0");
        }
    }

    result
}

async fn fetch_and_cache_entry(
    state: &AppState,
    key: &CacheKey,
    bucket_id: &str,
    path: &str,
) -> Result<CacheEntry, AppError> {
    let store = state.stores.get(bucket_id).ok_or_else(|| {
        warn!(bucket_id = %bucket_id, path = %path, "unknown bucket");
        AppError::not_found("unknown bucket")
    })?;

    let location: object_store::path::Path = path.into();

    let start = Instant::now();
    let result = match store.get(&location).await {
        Ok(result) => result,
        Err(err) => {
            state
                .metrics
                .observe_upstream_latency_ms(start.elapsed().as_millis() as u64);
            state.metrics.inc_upstream_err();
            warn!(
                bucket_id = %bucket_id,
                path = %path,
                elapsed_ms = start.elapsed().as_millis(),
                error = %err,
                "upstream get failed"
            );
            return Err(AppError::from_store(err));
        }
    };

    let bytes = match result.bytes().await {
        Ok(bytes) => bytes,
        Err(err) => {
            state
                .metrics
                .observe_upstream_latency_ms(start.elapsed().as_millis() as u64);
            state.metrics.inc_upstream_err();
            warn!(
                bucket_id = %bucket_id,
                path = %path,
                elapsed_ms = start.elapsed().as_millis(),
                error = %err,
                "upstream read failed"
            );
            return Err(AppError::from_store(err));
        }
    };

    state
        .metrics
        .observe_upstream_latency_ms(start.elapsed().as_millis() as u64);
    state.metrics.inc_upstream_ok();

    let content_type = Some(resolve_content_type(path, &bytes));
    let elapsed_ms = start.elapsed().as_millis();
    state
        .cache
        .put(key.clone(), bytes.clone(), content_type.clone())
        .await;

    let span = tracing::Span::current();
    span.record("bytes", bytes.len().to_string());
    info!(
        bucket_id = %bucket_id,
        path = %path,
        size = bytes.len(),
        elapsed_ms,
        content_type = %content_type.as_deref().unwrap_or("application/octet-stream"),
        "cache miss fetch"
    );
    Ok(CacheEntry::new(bytes, content_type))
}

fn resolve_content_type(path: &str, bytes: &Bytes) -> String {
    if let Some(mime) = mime_guess::from_path(path).first() {
        return mime.essence_str().to_string();
    }

    if let Some(kind) = infer::get(bytes) {
        return kind.mime_type().to_string();
    }

    "application/octet-stream".to_string()
}

#[derive(Debug, Serialize)]
pub struct StatsResponse {
    requests_total: u64,
    auth_fail_total: u64,
    cache_hit_total: u64,
    cache_miss_total: u64,
    upstream_ok_total: u64,
    upstream_err_total: u64,
    cache: CacheStatsResponse,
}

#[derive(Debug, Serialize)]
pub struct CacheStatsResponse {
    entries: u64,
    bytes: u64,
}

pub async fn stats(State(state): State<Arc<AppState>>) -> Result<Json<StatsResponse>, AppError> {
    let cache_stats = state.cache.stats().await;
    state
        .metrics
        .set_cache_stats(cache_stats.entries, cache_stats.total_bytes);

    let snapshot = state.metrics.snapshot();
    Ok(Json(StatsResponse {
        requests_total: snapshot.requests_total,
        auth_fail_total: snapshot.auth_fail_total,
        cache_hit_total: snapshot.cache_hit_total,
        cache_miss_total: snapshot.cache_miss_total,
        upstream_ok_total: snapshot.upstream_ok_total,
        upstream_err_total: snapshot.upstream_err_total,
        cache: CacheStatsResponse {
            entries: snapshot.cache_entries,
            bytes: snapshot.cache_bytes,
        },
    }))
}

#[derive(Debug, Serialize)]
pub struct PopulateResponse {
    cache_hit: bool,
    bytes: usize,
}

pub async fn populate_object(
    State(state): State<Arc<AppState>>,
    Path((bucket_id, path)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<PopulateResponse>, AppError> {
    let start = Instant::now();
    let span = info_span!(
        "populate_object",
        bucket_id = %bucket_id,
        path = %path,
        cache = tracing::field::Empty,
        inflight = tracing::field::Empty,
        status = tracing::field::Empty,
        bytes = tracing::field::Empty,
        elapsed_ms = tracing::field::Empty
    );
    let _enter = span.enter();

    state.metrics.inc_requests();
    let key = CacheKey::new(bucket_id.clone(), path.clone());
    let mut response_bytes: Option<usize> = None;

    let result = 'request: {
        let sig = match params.get("sig") {
            Some(sig) => sig,
            None => {
                state.metrics.inc_auth_fail();
                break 'request Err(AppError::unauthorized("missing signature"));
            }
        };

        if path.is_empty() {
            break 'request Err(AppError::bad_request("missing object path"));
        }

        if let Err(err) = state.auth.verify("POST", &bucket_id, &path, sig) {
            warn!(bucket_id = %bucket_id, path = %path, error = %err, "signature verification failed");
            state.metrics.inc_auth_fail();
            break 'request Err(AppError::unauthorized("invalid signature"));
        }

        if let Some(entry) = state.cache.get(&key).await {
            state.metrics.inc_cache_hit();
            span.record("cache", "hit");
            response_bytes = Some(entry.bytes.len());
            info!(bucket_id = %bucket_id, path = %path, bytes = entry.bytes.len(), "populate cache hit");
            break 'request Ok(PopulateResponse {
                cache_hit: true,
                bytes: entry.bytes.len(),
            });
        }

        state.metrics.inc_cache_miss();
        span.record("cache", "miss");
        info!(bucket_id = %bucket_id, path = %path, "populate cache miss");

        let permit = state.inflight.acquire(&key).await;
        match permit {
            InflightPermit::Leader(notify) => {
                span.record("inflight", "leader");
                info!(bucket_id = %bucket_id, path = %path, "populate inflight leader fetch");
                let result = fetch_and_cache_entry(&state, &key, &bucket_id, &path)
                    .await
                    .map(|entry| {
                        response_bytes = Some(entry.bytes.len());
                        PopulateResponse {
                            cache_hit: false,
                            bytes: entry.bytes.len(),
                        }
                    });
                state.inflight.release(&key, notify).await;
                break 'request result;
            }
            InflightPermit::Follower(notify) => {
                span.record("inflight", "follower");
                info!(bucket_id = %bucket_id, path = %path, "awaiting inflight leader for populate");
                notify.notified().await;
                if let Some(entry) = state.cache.get(&key).await {
                    response_bytes = Some(entry.bytes.len());
                    info!(bucket_id = %bucket_id, path = %path, bytes = entry.bytes.len(), "populate served from cache after inflight");
                    break 'request Ok(PopulateResponse {
                        cache_hit: true,
                        bytes: entry.bytes.len(),
                    });
                }
                break 'request fetch_and_cache_entry(&state, &key, &bucket_id, &path)
                    .await
                    .map(|entry| {
                        response_bytes = Some(entry.bytes.len());
                        PopulateResponse {
                            cache_hit: false,
                            bytes: entry.bytes.len(),
                        }
                    });
            }
        }
    };

    span.record("elapsed_ms", start.elapsed().as_millis().to_string());
    match &result {
        Ok(_) => {
            span.record("status", StatusCode::OK.to_string());
            if let Some(bytes) = response_bytes {
                span.record("bytes", bytes.to_string());
            }
        }
        Err(err) => {
            span.record("status", err.status.to_string());
            span.record("bytes", "0");
        }
    }

    result.map(Json)
}

pub async fn metrics(State(state): State<Arc<AppState>>) -> Result<Response<Body>, AppError> {
    let cache_stats = state.cache.stats().await;
    state
        .metrics
        .set_cache_stats(cache_stats.entries, cache_stats.total_bytes);
    let body = state.metrics.render_prometheus();
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    Ok(response)
}

fn build_response(entry: CacheEntry, cache_hit: bool) -> Response<Body> {
    let bytes = entry.bytes;
    let content_type = entry.content_type;
    let length = bytes.len();

    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;

    let headers = response.headers_mut();
    if let Some(content_type) = content_type
        && let Ok(value) = HeaderValue::from_str(&content_type)
    {
        headers.insert(header::CONTENT_TYPE, value);
    }
    let cache_status = if cache_hit { "hit=1" } else { "hit=0" };
    if let Ok(value) = HeaderValue::from_str(cache_status) {
        headers.insert("X-CG-Status", value);
    }
    let len_value = HeaderValue::from_str(&length.to_string())
        .unwrap_or_else(|_| HeaderValue::from_static("0"));
    headers.insert(header::CONTENT_LENGTH, len_value);

    response
}

#[derive(Debug)]
pub struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn bad_request(message: &str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.to_string(),
        }
    }

    fn unauthorized(message: &str) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.to_string(),
        }
    }

    fn not_found(message: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.to_string(),
        }
    }

    fn from_store(error: object_store::Error) -> Self {
        match error {
            object_store::Error::NotFound { .. } => Self::not_found("object not found"),
            _ => Self {
                status: StatusCode::BAD_GATEWAY,
                message: "upstream error".to_string(),
            },
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response<Body> {
        let mut response = Response::new(Body::from(self.message));
        *response.status_mut() = self.status;
        response
    }
}
