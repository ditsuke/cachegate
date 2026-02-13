use std::collections::HashMap;
use std::sync::Arc;

use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;

mod azure;

use crate::config::StoreConfig;

pub type StoreMap = HashMap<String, Arc<dyn ObjectStore>>;

pub fn build_stores(configs: &HashMap<String, StoreConfig>) -> anyhow::Result<StoreMap> {
    let mut stores: StoreMap = HashMap::new();

    for (id, config) in configs {
        let store: Arc<dyn ObjectStore> = match config {
            StoreConfig::S3 {
                bucket,
                region,
                access_key,
                secret_key,
                endpoint,
                allow_http,
            } => {
                let mut builder = AmazonS3Builder::new()
                    .with_bucket_name(bucket)
                    .with_region(region)
                    .with_access_key_id(access_key)
                    .with_secret_access_key(secret_key);

                if let Some(endpoint) = endpoint.as_deref() {
                    builder = builder.with_endpoint(endpoint);
                }
                if allow_http.unwrap_or(false) {
                    builder = builder.with_allow_http(true);
                }

                Arc::new(builder.build()?)
            }
            StoreConfig::Azure {
                container,
                connection_string,
            } => azure::build_azure_store(id, container, connection_string)?,
        };

        stores.insert(id.clone(), store);
    }

    Ok(stores)
}
