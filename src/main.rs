use axum::extract::MatchedPath;
use axum::extract::Request;
use sentry::types::Dsn;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing::info_span;
use tracing_error::ErrorLayer;
use tracing_subscriber::fmt;

use anyhow::Context;
use axum::Router;
use axum::middleware;
use axum::routing::get;
use base64::Engine;
use clap::Parser;
use serde::Serialize;
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt};
use tracing_subscriber::{Layer, Registry};

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
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, value_name = "env|path")]
    config: Option<String>,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    Keygen(KeygenArgs),
}

#[derive(Debug, Parser)]
struct KeygenArgs {
    #[arg(long, default_value = "auth.keys.yaml")]
    out: String,
    #[arg(long)]
    force: bool,
}

#[derive(Debug)]
enum ConfigSource {
    Env,
    File(String),
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(command) = args.command {
        return match command {
            Command::Keygen(command_args) => run_keygen(command_args),
        };
    }
    let source = match args.config.as_deref() {
        Some("env") => ConfigSource::Env,
        Some(value) => ConfigSource::File(value.to_string()),
        None => anyhow::bail!(
            "config source must be specified with --config, either 'env' or a file path"
        ),
    };

    let config: Config = match source {
        ConfigSource::Env => load_from_env().context("failed to load config from env")?,
        ConfigSource::File(path) => {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read config file {path}"))?;
            serde_yaml::from_str(&raw).context("failed to parse config file")?
        }
    };

    let sentry_guard = init_sentry(&config);
    init_tracing(sentry_guard.is_some());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    if let Err(err) = runtime.block_on(async_main(config)) {
        error!(error = %err, "cachegate failed to start");
        return Err(err);
    }

    Ok(())
}

fn run_keygen(args: KeygenArgs) -> anyhow::Result<()> {
    let output_path = std::path::Path::new(&args.out);
    if output_path.exists() && !args.force {
        anyhow::bail!("output file already exists: {}", args.out);
    }

    let mut rng = rand::rngs::OsRng;
    let signing_key = ed25519_dalek::SigningKey::generate(&mut rng);
    let verifying_key = signing_key.verifying_key();

    let private_key =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signing_key.to_bytes());
    let public_key =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(verifying_key.to_bytes());

    let output = AuthKeyYaml {
        auth: AuthKeyPair {
            public_key,
            private_key,
        },
    };

    let yaml = serde_yaml::to_string(&output)?;
    std::fs::write(output_path, yaml)?;
    println!("wrote keypair to {}", args.out);

    Ok(())
}

#[derive(Debug, Serialize)]
struct AuthKeyYaml {
    auth: AuthKeyPair,
}

#[derive(Debug, Serialize)]
struct AuthKeyPair {
    public_key: String,
    private_key: String,
}

async fn async_main(config: Config) -> anyhow::Result<()> {
    let auth = AuthState::from_config(&config.auth).context("failed to initialize auth")?;
    let stores = build_stores(&config.stores).context("failed to build stores")?;
    let cache = Arc::new(MemoryCache::new(config.cache.clone()));
    let metrics = Arc::new(Metrics::new());

    let state = Arc::new(AppState {
        stores,
        auth,
        cache,
        inflight: Arc::new(Inflight::new()),
        metrics,
    });

    let protected = Router::new()
        .route(
            "/{bucket_id}/{*path}",
            get(handler::get_object).head(handler::head_object),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            handler::auth_middleware,
        ));

    let app = Router::new()
        .route("/stats", get(handler::stats))
        .route("/metrics", get(handler::metrics))
        .route("/health", get(handler::health))
        .merge(protected)
        .with_state(state)
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
                // Log the matched route's path (with placeholders not filled in).
                // Use request.uri() or OriginalUri if you want the real path.
                let matched_path = request
                    .extensions()
                    .get::<MatchedPath>()
                    .map(MatchedPath::as_str);

                let op = match matched_path {
                    Some("/metrics") => "http.r.metrics",
                    Some("/stats") => "http.r.stats",
                    Some("/health") => "http.r.health",
                    Some("/{bucket_id}/{*path}") => {
                        if request.method() == axum::http::Method::HEAD {
                            "http.r.head_object"
                        } else {
                            "http.r.get_object"
                        }
                    }
                    Some(_) | None => "http.r.unknown",
                };

                info_span!(
                    "http_request",
                    method = ?request.method(),
                    matched_path,
                    "sentry.op" = op,
                )
            }),
        );

    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("failed to bind to {}", config.listen))?;
    info!(listen = %config.listen, "listening");
    axum::serve(listener, app).await.context("server failed")?;

    Ok(())
}

fn init_sentry(config: &Config) -> Option<sentry::ClientInitGuard> {
    let sentry_config = config.sentry.as_ref()?;
    let dsn = sentry_config.dsn.parse::<Dsn>().expect("Bad sentry DSN");

    let options = sentry::ClientOptions {
        dsn: Some(dsn),
        environment: sentry_config
            .environment
            .clone()
            .map(std::borrow::Cow::from),
        release: sentry::release_name!(),
        traces_sample_rate: sentry_config.traces_sample_rate.unwrap_or(0.1),
        debug: sentry_config.debug.unwrap_or(false),
        ..Default::default()
    };

    Some(sentry::init(options))
}

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

fn init_tracing(sentry_enabled: bool) {
    let enable_pretty = std::env::var("CACHEGATE_LOG_PRETTY")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let mut layers = Vec::<BoxedLayer>::new();

    layers.push(
        make_fmt_layer(enable_pretty)
            .with_filter(filter.clone())
            .boxed(),
    );

    // ErrorLayer enables capturing SpanTrace so errors can include span context.
    layers.push(ErrorLayer::default().boxed());

    if sentry_enabled {
        layers.push(sentry_tracing::layer().with_filter(filter).boxed());
    }

    // 5. Compose all layers into one subscriber
    let subscriber = tracing_subscriber::registry().with(layers);

    // 6. Set as global default
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set global subscriber");
}

fn make_fmt_layer(enable_pretty: bool) -> BoxedLayer {
    if enable_pretty {
        fmt::layer()
            .pretty()
            .with_target(false)
            .with_thread_ids(true)
            .boxed()
    } else {
        fmt::layer()
            .with_target(false)
            .with_thread_ids(true)
            .boxed()
    }
}
