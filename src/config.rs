use anyhow::Context;
use serde::Deserialize;
use std::collections::HashMap;
use std::env;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub listen: String,
    pub stores: HashMap<String, StoreConfig>,
    pub auth: AuthConfig,
    pub cache: CachePolicy,
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
    let raw = env::var("CACHEGATE_CONFIG").with_context(|| "missing CACHEGATE_CONFIG")?;
    serde_yaml::from_str(&raw).with_context(|| "invalid CACHEGATE_CONFIG")
}
