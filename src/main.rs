use std::sync::Arc;

use clap::Parser;

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
use config::{Config, load_from_env};
use handler::AppState;
use inflight::Inflight;
use metrics::Metrics;
use store::build_stores;

#[derive(Debug, Parser)]
#[command(name = "cachegate")]
struct Args {
    #[arg(long, value_name = "env|path")]
    config: Option<String>,
    #[arg(value_name = "config.yaml")]
    path: Option<String>,
}

#[derive(Debug)]
enum ConfigSource {
    Env,
    File(String),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    let source = match args.config.as_deref() {
        Some("env") => ConfigSource::Env,
        Some(value) => ConfigSource::File(value.to_string()),
        None => match args.path {
            Some(path) => ConfigSource::File(path),
            None => ConfigSource::File("config.yaml".to_string()),
        },
    };

    let config: Config = match source {
        ConfigSource::Env => load_from_env()?,
        ConfigSource::File(path) => {
            let raw = tokio::fs::read_to_string(&path).await?;
            serde_yaml::from_str(&raw)?
        }
    };

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
