use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use object_store::ObjectStore;
use object_store::azure::MicrosoftAzureBuilder;
use tracing::info_span;

#[derive(Debug, Clone)]
struct AzureConnectionInfo {
    account: String,
    access_key: String,
    endpoint: Option<String>,
    allow_http: bool,
}

pub(crate) fn build_azure_store(
    store_id: &str,
    container: &str,
    connection_string: &str,
) -> anyhow::Result<Arc<dyn ObjectStore>> {
    let info = parse_connection_string(connection_string)
        .with_context(|| format!("failed to parse azure connection string for store {store_id}"))?;

    let span = info_span!(
        "azure_store_init",
        store_id = %store_id,
        container = %container,
        account = %info.account,
        endpoint = %info.endpoint.as_deref().unwrap_or("default"),
        allow_http = info.allow_http
    );
    let _enter = span.enter();

    let mut builder = MicrosoftAzureBuilder::new()
        .with_account(info.account)
        .with_access_key(info.access_key)
        .with_container_name(container);

    if let Some(endpoint) = info.endpoint {
        builder = builder.with_endpoint(endpoint);
    }
    if info.allow_http {
        builder = builder.with_allow_http(true);
    }

    Ok(Arc::new(builder.build()?))
}

fn parse_connection_string(connection_string: &str) -> anyhow::Result<AzureConnectionInfo> {
    let mut values = HashMap::<String, String>::new();

    for segment in connection_string.split(';') {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut parts = trimmed.splitn(2, '=');
        let key = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        if key.is_empty() {
            continue;
        }

        let value = value
            .trim_matches('"')
            .trim_matches('\'')
            .trim()
            .to_string();
        values.insert(key.to_ascii_lowercase(), value);
    }

    let account = values
        .remove("accountname")
        .filter(|value| !value.is_empty())
        .context("missing AccountName")?;
    let access_key = values
        .remove("accountkey")
        .filter(|value| !value.is_empty())
        .context("missing AccountKey")?;

    let endpoint = values
        .remove("blobendpoint")
        .filter(|value| !value.is_empty());

    let protocol = values
        .remove("defaultendpointsprotocol")
        .map(|value| value.to_ascii_lowercase());
    let endpoint_suffix = values.remove("endpointsuffix");

    let endpoint = match (endpoint, endpoint_suffix) {
        (Some(endpoint), _) => Some(endpoint),
        (None, Some(suffix)) => {
            let scheme = protocol.as_deref().unwrap_or("https");
            Some(format!("{scheme}://{account}.blob.{suffix}"))
        }
        (None, None) => None,
    };

    let allow_http = endpoint
        .as_deref()
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("http://"))
        || protocol.as_deref().is_some_and(|value| value == "http");

    Ok(AzureConnectionInfo {
        account,
        access_key,
        endpoint,
        allow_http,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_connection_string;

    #[test]
    fn parses_account_and_key() {
        let connection_string = "AccountName=cachegate;AccountKey=secret";
        let info = parse_connection_string(connection_string).unwrap();

        assert_eq!(info.account, "cachegate");
        assert_eq!(info.access_key, "secret");
        assert_eq!(info.endpoint, None);
        assert!(!info.allow_http);
    }

    #[test]
    fn preserves_equals_in_account_key() {
        let connection_string = "AccountName=cachegate;AccountKey=abc==";
        let info = parse_connection_string(connection_string).unwrap();

        assert_eq!(info.access_key, "abc==");
    }

    #[test]
    fn parses_blob_endpoint_and_http() {
        let connection_string = concat!(
            "AccountName=devstoreaccount1;",
            "AccountKey=abc;",
            "BlobEndpoint=http://localhost:10000/devstoreaccount1;"
        );
        let info = parse_connection_string(connection_string).unwrap();

        assert_eq!(
            info.endpoint.as_deref(),
            Some("http://localhost:10000/devstoreaccount1")
        );
        assert!(info.allow_http);
    }

    #[test]
    fn builds_endpoint_from_protocol_and_suffix() {
        let connection_string = concat!(
            "DefaultEndpointsProtocol=http;",
            "AccountName=cachegate;",
            "AccountKey=secret;",
            "EndpointSuffix=core.windows.net"
        );
        let info = parse_connection_string(connection_string).unwrap();

        assert_eq!(
            info.endpoint.as_deref(),
            Some("http://cachegate.blob.core.windows.net")
        );
        assert!(info.allow_http);
    }
}
