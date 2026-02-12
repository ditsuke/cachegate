use anyhow::{Context, anyhow};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreType {
    S3,
    Azure,
}

#[derive(Debug, Clone, Default)]
struct StoreOverride {
    store_type: Option<StoreType>,
    bucket: Option<String>,
    region: Option<String>,
    access_key: Option<String>,
    secret_key: Option<String>,
    endpoint: Option<String>,
    allow_http: Option<bool>,
    account: Option<String>,
    container: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum StoreField {
    Type,
    Bucket,
    Region,
    AccessKey,
    SecretKey,
    Endpoint,
    AllowHttp,
    Account,
    Container,
}

pub fn apply_env_overrides(config: &mut Config) -> anyhow::Result<()> {
    let mut overrides: HashMap<String, StoreOverride> = HashMap::new();

    for (key, value) in env::vars() {
        match key.as_str() {
            "PROXY_LISTEN" => config.listen = value,
            "PROXY_AUTH_PUBLIC_KEY" => config.auth.public_key = value,
            "PROXY_AUTH_PRIVATE_KEY" => config.auth.private_key = value,
            "PROXY_CACHE_TTL_SECONDS" => {
                config.cache.ttl_seconds = value
                    .parse::<u64>()
                    .with_context(|| "invalid PROXY_CACHE_TTL_SECONDS")?
            }
            "PROXY_CACHE_MAX_BYTES" => {
                config.cache.max_bytes = value
                    .parse::<u64>()
                    .with_context(|| "invalid PROXY_CACHE_MAX_BYTES")?
            }
            _ => {
                if let Some((id, field)) = parse_store_env_key(&key) {
                    let entry = overrides.entry(id).or_default();
                    apply_store_override_field(entry, field, &value)?;
                }
            }
        }
    }

    for (id, override_config) in overrides {
        match config.stores.get_mut(&id) {
            Some(store) => apply_store_override(store, &id, &override_config)?,
            None => {
                let store = override_config.into_store_config(&id)?;
                config.stores.insert(id, store);
            }
        }
    }

    Ok(())
}

fn parse_store_env_key(key: &str) -> Option<(String, StoreField)> {
    let prefix = "PROXY_STORE_";
    if !key.starts_with(prefix) {
        return None;
    }

    let rest = &key[prefix.len()..];
    let fields = [
        ("TYPE", StoreField::Type),
        ("BUCKET", StoreField::Bucket),
        ("REGION", StoreField::Region),
        ("ACCESS_KEY", StoreField::AccessKey),
        ("SECRET_KEY", StoreField::SecretKey),
        ("ENDPOINT", StoreField::Endpoint),
        ("ALLOW_HTTP", StoreField::AllowHttp),
        ("ACCOUNT", StoreField::Account),
        ("CONTAINER", StoreField::Container),
    ];

    for (suffix, field) in fields {
        let marker = format!("_{suffix}");
        if rest.ends_with(&marker) {
            let raw_id = rest[..rest.len() - marker.len()].to_string();
            let id = normalize_store_id(&raw_id);
            if id.is_empty() {
                return None;
            }
            return Some((id, field));
        }
    }

    None
}

fn apply_store_override_field(
    entry: &mut StoreOverride,
    field: StoreField,
    value: &str,
) -> anyhow::Result<()> {
    match field {
        StoreField::Type => {
            entry.store_type = Some(parse_store_type(value)?);
        }
        StoreField::Bucket => entry.bucket = Some(value.to_string()),
        StoreField::Region => entry.region = Some(value.to_string()),
        StoreField::AccessKey => entry.access_key = Some(value.to_string()),
        StoreField::SecretKey => entry.secret_key = Some(value.to_string()),
        StoreField::Endpoint => {
            if value.is_empty() {
                entry.endpoint = None;
            } else {
                entry.endpoint = Some(value.to_string());
            }
        }
        StoreField::AllowHttp => {
            entry.allow_http = Some(parse_bool(value).with_context(|| "invalid ALLOW_HTTP")?);
        }
        StoreField::Account => entry.account = Some(value.to_string()),
        StoreField::Container => entry.container = Some(value.to_string()),
    }

    Ok(())
}

fn apply_store_override(
    store: &mut StoreConfig,
    id: &str,
    override_config: &StoreOverride,
) -> anyhow::Result<()> {
    match store {
        StoreConfig::S3 {
            bucket,
            region,
            access_key,
            secret_key,
            endpoint,
            allow_http,
        } => {
            if override_config.store_type == Some(StoreType::Azure) {
                return Err(anyhow!("store {id} type mismatch (s3 vs azure)"));
            }
            if let Some(value) = &override_config.bucket {
                *bucket = value.clone();
            }
            if let Some(value) = &override_config.region {
                *region = value.clone();
            }
            if let Some(value) = &override_config.access_key {
                *access_key = value.clone();
            }
            if let Some(value) = &override_config.secret_key {
                *secret_key = value.clone();
            }
            if let Some(value) = &override_config.endpoint {
                *endpoint = Some(value.clone());
            }
            if let Some(value) = override_config.allow_http {
                *allow_http = Some(value);
            }
        }
        StoreConfig::Azure {
            account,
            container,
            access_key,
        } => {
            if override_config.store_type == Some(StoreType::S3) {
                return Err(anyhow!("store {id} type mismatch (azure vs s3)"));
            }
            if let Some(value) = &override_config.account {
                *account = value.clone();
            }
            if let Some(value) = &override_config.container {
                *container = value.clone();
            }
            if let Some(value) = &override_config.access_key {
                *access_key = value.clone();
            }
        }
    }

    Ok(())
}

impl StoreOverride {
    fn into_store_config(self, id: &str) -> anyhow::Result<StoreConfig> {
        match self.store_type {
            Some(StoreType::S3) => Ok(StoreConfig::S3 {
                bucket: self
                    .bucket
                    .ok_or_else(|| anyhow!("store {id} missing bucket"))?,
                region: self
                    .region
                    .ok_or_else(|| anyhow!("store {id} missing region"))?,
                access_key: self
                    .access_key
                    .ok_or_else(|| anyhow!("store {id} missing access_key"))?,
                secret_key: self
                    .secret_key
                    .ok_or_else(|| anyhow!("store {id} missing secret_key"))?,
                endpoint: self.endpoint,
                allow_http: self.allow_http,
            }),
            Some(StoreType::Azure) => Ok(StoreConfig::Azure {
                account: self
                    .account
                    .ok_or_else(|| anyhow!("store {id} missing account"))?,
                container: self
                    .container
                    .ok_or_else(|| anyhow!("store {id} missing container"))?,
                access_key: self
                    .access_key
                    .ok_or_else(|| anyhow!("store {id} missing access_key"))?,
            }),
            None => Err(anyhow!("store {id} missing type")),
        }
    }
}

fn parse_store_type(value: &str) -> anyhow::Result<StoreType> {
    match value.to_lowercase().as_str() {
        "s3" => Ok(StoreType::S3),
        "azure" => Ok(StoreType::Azure),
        _ => Err(anyhow!("invalid store type: {value}")),
    }
}

fn parse_bool(value: &str) -> anyhow::Result<bool> {
    match value.to_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(anyhow!("invalid boolean: {value}")),
    }
}

fn normalize_store_id(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '_' {
            if matches!(chars.peek(), Some('_')) {
                chars.next();
                out.push('_');
            } else {
                out.push('-');
            }
        } else {
            out.push(ch.to_ascii_lowercase());
        }
    }

    out
}
