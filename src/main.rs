use std::sync::Arc;

use clap::Parser;

use axum::Router;
use axum::routing::get;
use tracing::info;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

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

fn main() -> anyhow::Result<()> {
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
            let raw = std::fs::read_to_string(&path)?;
            serde_yaml::from_str(&raw)?
        }
    };

    let sentry_guard = init_sentry(&config);
    init_tracing(sentry_guard.is_some());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async_main(config))?;

    Ok(())
}

async fn async_main(config: Config) -> anyhow::Result<()> {
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

fn init_tracing(enable_sentry: bool) {
    if enable_sentry {
        tracing_subscriber::registry()
            .with(EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer())
            .with(sentry_tracing::layer())
            .init();
    } else {
        tracing_subscriber::registry()
            .with(EnvFilter::from_default_env())
            .with(tracing_subscriber::fmt::layer())
            .init();
    }
}

fn init_sentry(config: &Config) -> Option<sentry::ClientInitGuard> {
    let sentry_config = config.sentry.as_ref()?;
    let dsn = sentry_config.dsn.as_deref()?;
    let dsn = dsn.parse().ok()?;

    let options = sentry::ClientOptions {
        dsn: Some(dsn),
        environment: sentry_config
            .environment
            .clone()
            .map(std::borrow::Cow::from),
        release: sentry_config.release.clone().map(std::borrow::Cow::from),
        traces_sample_rate: sentry_config.traces_sample_rate.unwrap_or(0.1),
        debug: sentry_config.debug.unwrap_or(false),
        ..Default::default()
    };

    Some(sentry::init(options))
}
