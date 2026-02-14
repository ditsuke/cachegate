use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Extension, FromRequestParts, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Request, Response, StatusCode, header};
use axum::middleware::Next;
use axum::response::IntoResponse;
use bytes::Bytes;
use bytes::BytesMut;
use futures::StreamExt;
use object_store::ObjectStoreExt;
use object_store::WriteMultipart;
use serde::{Deserialize, Serialize};
use std::time::Instant;
use tracing::{info, info_span, warn};

use crate::auth::{AuthContext, AuthError, AuthMethod, AuthState};
use crate::cache::{CacheBackend, CacheEntry, CacheKey};
use crate::inflight::{Inflight, InflightPermit};
use crate::metrics::{Metrics, UpstreamErrorKind};
use crate::store::StoreMap;

pub struct AppState<C: CacheBackend> {
    pub stores: StoreMap,
    pub auth: AuthState,
    pub cache: Arc<C>,
    pub inflight: Arc<Inflight>,
    pub metrics: Arc<Metrics>,
    pub cache_max_object_bytes: u64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PathParams {
    bucket_id: String,
    path: String,
}

pub async fn auth_middleware<C: CacheBackend + 'static>(
    State(state): State<Arc<AppState<C>>>,
    request: Request<Body>,
    next: Next,
) -> Result<Response<Body>, AppError> {
    let (mut parts, body) = request.into_parts();
    let Path(PathParams { bucket_id, path }) =
        Path::<PathParams>::from_request_parts(&mut parts, &state)
            .await
            .map_err(|_| AppError::bad_request("invalid path"))?;

    let Query(params) = Query::<HashMap<String, String>>::from_request_parts(&mut parts, &state)
        .await
        .map_err(|_| AppError::bad_request("invalid query"))?;
    let method = parts.method.to_string();

    let span = info_span!(
        "auth_check",
        bucket_id = %bucket_id,
        path = %path,
        method = %method,
        auth = tracing::field::Empty,
        status = tracing::field::Empty,
        error = tracing::field::Empty
    );
    let _enter = span.enter();

    let bearer_token = parse_bearer_token(&parts.headers);
    let mut auth_method = None;
    let mut last_error: Option<AuthError> = None;

    if let Some(token) = bearer_token.as_deref() {
        match state.auth.verify_bearer(token) {
            Ok(_) => auth_method = Some(AuthMethod::Bearer),
            Err(err) => last_error = Some(err),
        }
    }

    if auth_method.is_none() {
        if let Some(sig) = params.get("sig") {
            match state.auth.verify(&method, &bucket_id, &path, sig) {
                Ok(_) => auth_method = Some(AuthMethod::Presign),
                Err(err) => last_error = Some(err),
            }
        } else if last_error.is_none() {
            last_error = Some(AuthError::MissingAuth);
        }
    }

    let auth_method = match auth_method {
        Some(method) => method,
        None => {
            state.metrics.inc_auth_fail(method.as_str());
            let error = last_error.unwrap_or(AuthError::MissingAuth);
            span.record("status", StatusCode::UNAUTHORIZED.to_string());
            span.record("error", error.to_string());
            warn!(bucket_id = %bucket_id, path = %path, error = %error, "auth failed");
            return Err(AppError::unauthorized("invalid auth"));
        }
    };

    span.record("auth", auth_method.as_str());
    span.record("status", StatusCode::OK.to_string());

    let mut request = Request::from_parts(parts, body);
    request.extensions_mut().insert(AuthContext {
        method: auth_method,
    });
    Ok(next.run(request).await)
}

pub async fn get_object<C: CacheBackend + 'static>(
    State(state): State<Arc<AppState<C>>>,
    Path(PathParams { bucket_id, path }): Path<PathParams>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Response<Body>, AppError> {
    let start = Instant::now();
    let method = "GET";
    let span = info_span!(
        "get_object",
        bucket_id = %bucket_id,
        path = %path,
        auth = tracing::field::Empty,
        cache = tracing::field::Empty,
        inflight = tracing::field::Empty,
        status = tracing::field::Empty,
        bytes = tracing::field::Empty,
        elapsed_ms = tracing::field::Empty
    );
    let _enter = span.enter();

    span.record("auth", auth.method.as_str());
    let key = CacheKey::new(bucket_id.clone(), path.clone());
    let mut response_bytes: Option<usize> = None;

    let result = 'request: {
        if path.is_empty() || path.contains("..") || path.starts_with('/') {
            break 'request Err(AppError::bad_request("invalid object path"));
        }

        if let Some(entry) = state.cache.get(&key).await {
            state.metrics.inc_cache_hit(method);
            span.record("cache", "hit");
            response_bytes = Some(entry.bytes.len());
            info!(bucket_id = %bucket_id, path = %path, bytes = entry.bytes.len(), "served from cache");
            break 'request Ok(build_response(entry, true));
        }

        state.metrics.inc_cache_miss(method);
        span.record("cache", "miss");
        info!(bucket_id = %bucket_id, path = %path, "cache miss");

        let permit = state.inflight.acquire(&key).await;
        match permit {
            InflightPermit::Leader(notify) => {
                span.record("inflight", "leader");
                info!(bucket_id = %bucket_id, path = %path, "inflight leader fetch");
                let result = fetch_and_cache_entry(&state, &key, &bucket_id, &path, method)
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
                break 'request fetch_and_cache_entry(&state, &key, &bucket_id, &path, method)
                    .await
                    .map(|entry| {
                        response_bytes = Some(entry.bytes.len());
                        build_response(entry, false)
                    });
            }
        }
    };

    span.record("elapsed_ms", start.elapsed().as_millis().to_string());
    let status_label = match &result {
        Ok(response) => {
            span.record("status", response.status().to_string());
            if let Some(bytes) = response_bytes {
                span.record("bytes", bytes.to_string());
            }
            response.status().as_u16().to_string()
        }
        Err(err) => {
            span.record("status", err.status.to_string());
            span.record("bytes", "0");
            err.status.as_u16().to_string()
        }
    };
    state.metrics.inc_requests(method, status_label.as_str());

    result
}

pub async fn head_object<C: CacheBackend + 'static>(
    State(state): State<Arc<AppState<C>>>,
    Path(PathParams { bucket_id, path }): Path<PathParams>,
    Query(params): Query<HashMap<String, String>>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Response<Body>, AppError> {
    let start = Instant::now();
    let method = "HEAD";
    let span = info_span!(
        "head_object",
        bucket_id = %bucket_id,
        path = %path,
        auth = tracing::field::Empty,
        cache = tracing::field::Empty,
        inflight = tracing::field::Empty,
        status = tracing::field::Empty,
        bytes = tracing::field::Empty,
        elapsed_ms = tracing::field::Empty
    );
    let _enter = span.enter();

    span.record("auth", auth.method.as_str());
    let key = CacheKey::new(bucket_id.clone(), path.clone());
    let prefetch_enabled = parse_prefetch(&params);
    let mut response_bytes: Option<usize> = None;

    let result = 'request: {
        if path.is_empty() || path.contains("..") || path.starts_with('/') {
            break 'request Err(AppError::bad_request("invalid object path"));
        }

        if let Some(entry) = state.cache.get(&key).await {
            state.metrics.inc_cache_hit(method);
            span.record("cache", "hit");
            response_bytes = Some(entry.bytes.len());
            info!(bucket_id = %bucket_id, path = %path, bytes = entry.bytes.len(), "head served from cache");
            break 'request Ok(build_head_response(entry));
        }

        state.metrics.inc_cache_miss(method);
        span.record("cache", "miss");
        info!(bucket_id = %bucket_id, path = %path, "head cache miss");

        if prefetch_enabled {
            span.record("inflight", "prefetch");
        } else {
            span.record("inflight", "skipped");
        }

        let store = state.stores.get(&bucket_id).ok_or_else(|| {
            warn!(bucket_id = %bucket_id, path = %path, "unknown bucket");
            AppError::not_found("unknown bucket")
        })?;
        let location: object_store::path::Path = path.as_str().into();
        let head_start = Instant::now();
        let meta = match store.head(&location).await {
            Ok(meta) => {
                state
                    .metrics
                    .observe_upstream_latency_ms(method, head_start.elapsed().as_millis() as u64);
                state.metrics.inc_upstream_ok(method);
                meta
            }
            Err(err) => {
                let error_kind = UpstreamErrorKind::from_store_error(&err);
                state
                    .metrics
                    .observe_upstream_latency_ms(method, head_start.elapsed().as_millis() as u64);
                state.metrics.inc_upstream_err(method, error_kind);
                warn!(
                    bucket_id = %bucket_id,
                    path = %path,
                    elapsed_ms = head_start.elapsed().as_millis(),
                    error = %err,
                    "upstream head failed"
                );
                return Err(AppError::from_store(err));
            }
        };

        if prefetch_enabled {
            spawn_head_prefetch(state.clone(), key.clone(), bucket_id.clone(), path.clone());
        }

        if let Ok(size) = usize::try_from(meta.size) {
            response_bytes = Some(size);
        }

        let content_type = mime_guess::from_path(&path)
            .first()
            .map(|mime| mime.essence_str().to_string());
        break 'request Ok(build_head_response_with_meta(meta.size, content_type));
    };

    span.record("elapsed_ms", start.elapsed().as_millis().to_string());
    let status_label = match &result {
        Ok(response) => {
            span.record("status", response.status().to_string());
            if let Some(bytes) = response_bytes {
                span.record("bytes", bytes.to_string());
            }
            response.status().as_u16().to_string()
        }
        Err(err) => {
            span.record("status", err.status.to_string());
            span.record("bytes", "0");
            err.status.as_u16().to_string()
        }
    };
    state.metrics.inc_requests(method, status_label.as_str());

    result
}

pub async fn put_object<C: CacheBackend + 'static>(
    State(state): State<Arc<AppState<C>>>,
    Path(PathParams { bucket_id, path }): Path<PathParams>,
    Extension(auth): Extension<AuthContext>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response<Body>, AppError> {
    let start = Instant::now();
    let method = "PUT";
    let span = info_span!(
        "put_object",
        bucket_id = %bucket_id,
        path = %path,
        auth = tracing::field::Empty,
        cache = tracing::field::Empty,
        status = tracing::field::Empty,
        bytes = tracing::field::Empty,
        elapsed_ms = tracing::field::Empty
    );
    let _enter = span.enter();

    span.record("auth", auth.method.as_str());
    let key = CacheKey::new(bucket_id.clone(), path.clone());
    let mut response_bytes: Option<usize> = None;

    let result = 'request: {
        if path.is_empty() || path.contains("..") || path.starts_with('/') {
            break 'request Err(AppError::bad_request("invalid object path"));
        }

        let store = state.stores.get(&bucket_id).ok_or_else(|| {
            warn!(bucket_id = %bucket_id, path = %path, "unknown bucket");
            AppError::not_found("unknown bucket")
        })?;

        let location: object_store::path::Path = path.as_str().into();

        match store.head(&location).await {
            Ok(_) => {
                warn!(bucket_id = %bucket_id, path = %path, "overwriting existing object");
            }
            Err(err) => {
                if !matches!(err, object_store::Error::NotFound { .. }) {
                    warn!(bucket_id = %bucket_id, path = %path, error = %err, "head check before put failed");
                }
            }
        }

        let upload_start = Instant::now();
        let upload = match store.put_multipart(&location).await {
            Ok(upload) => upload,
            Err(err) => {
                let error_kind = UpstreamErrorKind::from_store_error(&err);
                state
                    .metrics
                    .observe_upstream_latency_ms(method, upload_start.elapsed().as_millis() as u64);
                state.metrics.inc_upstream_err(method, error_kind);
                warn!(
                    bucket_id = %bucket_id,
                    path = %path,
                    elapsed_ms = upload_start.elapsed().as_millis(),
                    error = %err,
                    "upstream put init failed"
                );
                break 'request Err(AppError::from_store(err));
            }
        };

        let mut write = WriteMultipart::new(upload);
        let mut stream = body.into_data_stream();
        let mut total_bytes: usize = 0;
        let cap_bytes = state.cache_max_object_bytes as usize;
        let mut buffer = BytesMut::new();
        let mut capped = cap_bytes == 0;

        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(err) => {
                    let _ = write.abort().await;
                    warn!(bucket_id = %bucket_id, path = %path, error = %err, "failed reading request body");
                    break 'request Err(AppError::bad_request("invalid request body"));
                }
            };

            total_bytes = total_bytes.saturating_add(chunk.len());

            if !capped {
                let remaining = cap_bytes.saturating_sub(buffer.len());
                if remaining == 0 {
                    capped = true;
                } else if chunk.len() <= remaining {
                    buffer.extend_from_slice(&chunk);
                } else {
                    buffer.extend_from_slice(&chunk[..remaining]);
                    capped = true;
                }
            }

            write.put(chunk);
        }

        match write.finish().await {
            Ok(_result) => {
                state
                    .metrics
                    .observe_upstream_latency_ms(method, upload_start.elapsed().as_millis() as u64);
                state.metrics.inc_upstream_ok(method);
            }
            Err(err) => {
                let error_kind = UpstreamErrorKind::from_store_error(&err);
                state
                    .metrics
                    .observe_upstream_latency_ms(method, upload_start.elapsed().as_millis() as u64);
                state.metrics.inc_upstream_err(method, error_kind);
                warn!(
                    bucket_id = %bucket_id,
                    path = %path,
                    elapsed_ms = upload_start.elapsed().as_millis(),
                    error = %err,
                    "upstream put failed"
                );
                break 'request Err(AppError::from_store(err));
            }
        }

        response_bytes = Some(total_bytes);

        if !capped {
            let content_type = content_type_from_headers(&headers, &path);
            span.record("cache", "insert");
            state
                .cache
                .put(key, buffer.freeze(), content_type.clone())
                .await;
        } else {
            span.record("cache", "skipped");
            info!(
                bucket_id = %bucket_id,
                path = %path,
                bytes = total_bytes,
                cap_bytes,
                "put cache skipped; payload exceeded cap"
            );
        }

        break 'request Ok(build_put_response());
    };

    span.record("elapsed_ms", start.elapsed().as_millis().to_string());
    let status_label = match &result {
        Ok(response) => {
            span.record("status", response.status().to_string());
            if let Some(bytes) = response_bytes {
                span.record("bytes", bytes.to_string());
            }
            response.status().as_u16().to_string()
        }
        Err(err) => {
            span.record("status", err.status.to_string());
            span.record("bytes", "0");
            err.status.as_u16().to_string()
        }
    };
    state.metrics.inc_requests(method, status_label.as_str());

    result
}

async fn fetch_and_cache_entry<C: CacheBackend>(
    state: &AppState<C>,
    key: &CacheKey,
    bucket_id: &str,
    path: &str,
    method: &str,
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
            let error_kind = UpstreamErrorKind::from_store_error(&err);
            state
                .metrics
                .observe_upstream_latency_ms(method, start.elapsed().as_millis() as u64);
            state.metrics.inc_upstream_err(method, error_kind);
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
            let error_kind = UpstreamErrorKind::from_store_error(&err);
            state
                .metrics
                .observe_upstream_latency_ms(method, start.elapsed().as_millis() as u64);
            state.metrics.inc_upstream_err(method, error_kind);
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
        .observe_upstream_latency_ms(method, start.elapsed().as_millis() as u64);
    state.metrics.inc_upstream_ok(method);

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

pub async fn stats<C: CacheBackend + 'static>(
    State(state): State<Arc<AppState<C>>>,
) -> Result<Json<StatsResponse>, AppError> {
    let snapshot = state.metrics.snapshot();
    let cache_stats = state.cache.stats().await;
    Ok(Json(StatsResponse {
        requests_total: snapshot.requests_total,
        auth_fail_total: snapshot.auth_fail_total,
        cache_hit_total: snapshot.cache_hit_total,
        cache_miss_total: snapshot.cache_miss_total,
        upstream_ok_total: snapshot.upstream_ok_total,
        upstream_err_total: snapshot.upstream_err_total,
        cache: CacheStatsResponse {
            entries: cache_stats.inserts,
            bytes: 0,
        },
    }))
}

pub async fn health() -> Result<Response<Body>, AppError> {
    let mut response = Response::new(Body::from("OK"));
    *response.status_mut() = StatusCode::OK;
    Ok(response)
}

pub async fn metrics<C: CacheBackend + 'static>(
    State(state): State<Arc<AppState<C>>>,
) -> Result<Response<Body>, AppError> {
    let _cache_stats = state.cache.stats().await;
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

fn build_head_response(entry: CacheEntry) -> Response<Body> {
    let length = entry.bytes.len();
    let content_type = entry.content_type;

    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::OK;

    let headers = response.headers_mut();
    if let Some(content_type) = content_type
        && let Ok(value) = HeaderValue::from_str(&content_type)
    {
        headers.insert(header::CONTENT_TYPE, value);
    }
    let len_value = HeaderValue::from_str(&length.to_string())
        .unwrap_or_else(|_| HeaderValue::from_static("0"));
    headers.insert(header::CONTENT_LENGTH, len_value);

    response
}

fn build_head_response_with_meta(length: u64, content_type: Option<String>) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::OK;

    let headers = response.headers_mut();
    if let Some(content_type) = content_type
        && let Ok(value) = HeaderValue::from_str(&content_type)
    {
        headers.insert(header::CONTENT_TYPE, value);
    }
    let len_value = HeaderValue::from_str(&length.to_string())
        .unwrap_or_else(|_| HeaderValue::from_static("0"));
    headers.insert(header::CONTENT_LENGTH, len_value);

    response
}

fn build_put_response() -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::OK;
    response
}

fn content_type_from_headers(headers: &HeaderMap, path: &str) -> Option<String> {
    if let Some(value) = headers.get(header::CONTENT_TYPE) {
        if let Ok(value) = value.to_str() {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }

    mime_guess::from_path(path)
        .first()
        .map(|mime| mime.essence_str().to_string())
}

fn parse_prefetch(params: &HashMap<String, String>) -> bool {
    params
        .get("prefetch")
        .and_then(|value| parse_prefetch_value(value))
        .unwrap_or(false)
}

fn parse_prefetch_value(value: &str) -> Option<bool> {
    match value {
        "1" => Some(true),
        "0" => Some(false),
        _ if value.eq_ignore_ascii_case("true") => Some(true),
        _ if value.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

fn spawn_head_prefetch<C: CacheBackend + 'static>(
    state: Arc<AppState<C>>,
    key: CacheKey,
    bucket_id: String,
    path: String,
) {
    // May be dropped, but that's okay.
    tokio::spawn(async move {
        let span = info_span!(
            "head_prefetch",
            bucket_id = %bucket_id,
            path = %path,
            inflight = tracing::field::Empty,
            status = tracing::field::Empty,
            error = tracing::field::Empty
        );
        let _enter = span.enter();

        let permit = state.inflight.acquire(&key).await;
        match permit {
            InflightPermit::Leader(notify) => {
                span.record("inflight", "leader");
                info!(bucket_id = %bucket_id, path = %path, "head prefetch leader fetch");
                let result = fetch_and_cache_entry(&state, &key, &bucket_id, &path, "HEAD").await;
                match &result {
                    Ok(entry) => {
                        span.record("status", "ok");
                        info!(
                            bucket_id = %bucket_id,
                            path = %path,
                            bytes = entry.bytes.len(),
                            "head prefetch completed"
                        );
                    }
                    Err(err) => {
                        span.record("status", err.status.to_string());
                        span.record("error", err.message.as_str());
                        warn!(
                            bucket_id = %bucket_id,
                            path = %path,
                            status = %err.status,
                            "head prefetch failed"
                        );
                    }
                }
                state.inflight.release(&key, notify).await;
            }
            InflightPermit::Follower(_notify) => {
                span.record("inflight", "follower");
                info!(
                    bucket_id = %bucket_id,
                    path = %path,
                    "head prefetch skipped; inflight exists"
                );
            }
        }
    });
}

fn parse_bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let mut parts = value.split_whitespace();
    let scheme = parts.next()?;
    let token = parts.next()?;
    if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() {
        Some(token.to_string())
    } else {
        None
    }
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
