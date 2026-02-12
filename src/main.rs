use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod auth;
mod cache;
mod config;
mod handler;
mod inflight;
mod metrics;
mod store;

use auth::AuthState;
use cache::memory::MemoryCache;
use config::{Config, apply_env_overrides};
use handler::AppState;
use inflight::Inflight;
use metrics::Metrics;
use store::build_stores;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.yaml".to_string());
    let raw = tokio::fs::read_to_string(&config_path).await?;
    let mut config: Config = serde_yaml::from_str(&raw)?;
    apply_env_overrides(&mut config)?;

    let auth = AuthState::from_config(&config.auth)?;
    let stores = build_stores(&config.stores)?;
    let cache = Arc::new(MemoryCache::new(config.cache.clone()));
    let metrics = Arc::new(Metrics::new());

    let state = Arc::new(AppState {
        stores,
        auth,
        cache,
        inflight: Arc::new(Inflight::new()),
        metrics,
    });

    let app = Router::new()
        .route("/stats", get(handler::stats))
        .route("/metrics", get(handler::metrics))
        .route("/:bucket_id/*path", get(handler::get_object))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    info!(listen = %config.listen, "listening");
    axum::serve(listener, app).await?;

    Ok(())
}
