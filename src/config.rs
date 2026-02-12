use anyhow::Context;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub listen: String,
    pub stores: HashMap<String, StoreConfig>,
    pub auth: AuthConfig,
    pub cache: CachePolicy,
    #[serde(default)]
    pub sentry: Option<SentryConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    pub public_key: String,
    pub private_key: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CachePolicy {
    pub ttl_seconds: u64,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SentryConfig {
    pub dsn: Option<String>,
    pub environment: Option<String>,
    pub release: Option<String>,
    pub traces_sample_rate: Option<f32>,
    pub debug: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum StoreConfig {
    #[serde(rename = "s3")]
    S3 {
        bucket: String,
        region: String,
        access_key: String,
        secret_key: String,
        endpoint: Option<String>,
        allow_http: Option<bool>,
    },
    #[serde(rename = "azure")]
    Azure {
        account: String,
        container: String,
        access_key: String,
    },
}

pub fn load_from_env() -> anyhow::Result<Config> {
    // if just CACHEGATE_CONFIG is set, parse that as a full config file, otherwise parse from individual env vars

    if let Ok(config_str) = std::env::var("CACHEGATE_CONFIG") {
        serde_yaml::from_str(&config_str).context("failed to parse config from CACHEGATE_CONFIG")
    } else {
        envious::Config::default()
            .with_prefix("CACHEGATE__")
            .build_from_env()
            .context("failed to parse config from environment variables")
    }
}
