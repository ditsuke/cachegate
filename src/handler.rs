use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use bytes::Bytes;
use tracing::{info, warn};
use object_store::ObjectStoreExt;

use crate::auth::AuthState;
use crate::cache::{CacheBackend, CacheEntry, CacheKey};
use crate::inflight::{Inflight, InflightPermit};
use crate::store::StoreMap;

pub struct AppState {
    pub stores: StoreMap,
    pub auth: AuthState,
    pub cache: Arc<dyn CacheBackend>,
    pub inflight: Arc<Inflight>,
}

pub async fn get_object(
    State(state): State<Arc<AppState>>,
    Path((bucket_id, path)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response<Body>, AppError> {
    let sig = params
        .get("sig")
        .ok_or_else(|| AppError::unauthorized("missing signature"))?;

    if path.is_empty() {
        return Err(AppError::bad_request("missing object path"));
    }

    if let Err(err) = state.auth.verify("GET", &bucket_id, &path, sig) {
        warn!(bucket_id = %bucket_id, path = %path, error = %err, "signature verification failed");
        return Err(AppError::unauthorized("invalid signature"));
    }

    let key = CacheKey::new(bucket_id.clone(), path.clone());

    if let Some(entry) = state.cache.get(&key).await {
        return Ok(build_response(entry));
    }

    let permit = state.inflight.acquire(&key).await;
    match permit {
        InflightPermit::Leader(notify) => {
            let result = fetch_and_cache(&state, &key, &bucket_id, &path).await;
            state.inflight.release(&key, notify).await;
            result
        }
        InflightPermit::Follower(notify) => {
            notify.notified().await;
            if let Some(entry) = state.cache.get(&key).await {
                return Ok(build_response(entry));
            }
            fetch_and_cache(&state, &key, &bucket_id, &path).await
        }
    }
}

async fn fetch_and_cache(
    state: &AppState,
    key: &CacheKey,
    bucket_id: &str,
    path: &str,
) -> Result<Response<Body>, AppError> {
    let store = state
        .stores
        .get(bucket_id)
        .ok_or_else(|| AppError::not_found("unknown bucket"))?;

    let location: object_store::path::Path = path
        .try_into()
        .map_err(|_| AppError::bad_request("invalid object path"))?;

    let result = store.get(&location).await.map_err(AppError::from_store)?;
    let bytes = result.bytes().await.map_err(AppError::from_store)?;

    let content_type = Some(resolve_content_type(path, &bytes));
    state
        .cache
        .put(key.clone(), bytes.clone(), content_type.clone())
        .await;

    info!(bucket_id = %bucket_id, path = %path, size = bytes.len(), "cache miss fetch");
    Ok(build_response(CacheEntry::new(bytes, content_type)))
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

fn build_response(entry: CacheEntry) -> Response<Body> {
    let bytes = entry.bytes;
    let content_type = entry.content_type;
    let length = bytes.len();

    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;

    let headers = response.headers_mut();
    if let Some(content_type) = content_type {
        if let Ok(value) = HeaderValue::from_str(&content_type) {
            headers.insert(header::CONTENT_TYPE, value);
        }
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
            object_store::Error::NotFound { .. } => {
                Self::not_found("object not found")
            }
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
