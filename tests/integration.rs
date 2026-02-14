use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::primitives::ByteStream;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer, SigningKey};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::tempdir;

const MINIO_ENDPOINT: &str = "http://127.0.0.1:9305";
const MINIO_ACCESS_KEY: &str = "minioadmin";
const MINIO_SECRET_KEY: &str = "minioadmin";
const MINIO_REGION: &str = "us-east-1";
const TEST_BEARER_TOKEN: &str = "cachegate-test-token";

#[derive(Serialize)]
struct PresignPayload {
    v: u8,
    exp: i64,
    m: String,
    b: String,
    p: String,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct StatsResponse {
    requests_total: u64,
    auth_fail_total: u64,
    cache_hit_total: u64,
    cache_miss_total: u64,
    upstream_ok_total: u64,
    upstream_err_total: u64,
    cache: CacheStatsResponse,
}

#[derive(Deserialize)]
struct CacheStatsResponse {
    entries: u64,
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_minio_readthrough() {
    ensure_minio_ready().await;
    let client = minio_client().await;

    let bucket = format!("cachegate-test-{}", unix_timestamp());
    create_bucket(&client, &bucket).await;

    let object_key = format!("fixture-{}.txt", unix_timestamp());
    let payload = b"cachegate integration test".to_vec();
    put_object(&client, &bucket, &object_key, payload.clone()).await;

    let signing_key = SigningKey::from_bytes(&[7u8; 32]);
    let public_key = signing_key.verifying_key();
    let public_b64 = URL_SAFE_NO_PAD.encode(public_key.as_bytes());
    let private_b64 = URL_SAFE_NO_PAD.encode(signing_key.to_bytes());

    let port = free_port();
    let listen = format!("127.0.0.1:{port}");
    let mut config_file = tempfile::NamedTempFile::new().expect("temp config");
    let temp_disk = tempdir().expect("temp cache dir");

    let temp_disk_path = temp_disk.path().display();
    let config_body = format!(
        r#"listen: "{listen}"

auth:
  public_key: "{public_b64}"
  private_key: "{private_b64}"
  bearer_token: "{TEST_BEARER_TOKEN}"

cache:
  ttl_seconds: 60
  max_memory: 10MB
  max_object_size: 1MiB
  max_disk: 15MiB
  disk_path: {temp_disk_path}

stores:
  minio-test:
    type: s3
    bucket: "{bucket}"
    region: "{MINIO_REGION}"
    access_key: "{MINIO_ACCESS_KEY}"
    secret_key: "{MINIO_SECRET_KEY}"
    endpoint: "{MINIO_ENDPOINT}"
    allow_http: true
"#
    );
    config_file
        .write_all(config_body.as_bytes())
        .expect("write config");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cachegate"));
    cmd.arg("--config")
        .arg(config_file.path())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    let child = cmd.spawn().expect("start cachegate");
    let _guard = ChildGuard::new(child);

    let base_url = format!("http://{listen}");
    wait_for_ready(&base_url).await;

    let store_id = "minio-test";
    let http = reqwest::Client::new();

    let put_key = format!("upload-{}.txt", unix_timestamp());
    let put_payload = b"cachegate put test".to_vec();
    let put_url = format!("{base_url}/{store_id}/{put_key}");
    let put_response = http
        .put(&put_url)
        .bearer_auth(TEST_BEARER_TOKEN)
        .body(put_payload.clone())
        .send()
        .await
        .expect("put upload");
    assert_eq!(put_response.status(), StatusCode::OK);

    let get_put_response = http
        .get(&put_url)
        .bearer_auth(TEST_BEARER_TOKEN)
        .send()
        .await
        .expect("get after put");
    assert_eq!(get_put_response.status(), StatusCode::OK);
    let put_cache_status = get_put_response
        .headers()
        .get("X-CG-Status")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    assert_eq!(put_cache_status, "hit=1");
    let put_body = get_put_response.bytes().await.expect("read put body");
    assert_eq!(put_body.as_ref(), put_payload.as_slice());

    let large_key = format!("upload-large-{}.bin", unix_timestamp());
    let large_payload = vec![0u8; 2 * 1024 * 1024];
    let large_url = format!("{base_url}/{store_id}/{large_key}");
    let large_response = http
        .put(&large_url)
        .bearer_auth(TEST_BEARER_TOKEN)
        .body(large_payload.clone())
        .send()
        .await
        .expect("put large upload");
    assert_eq!(large_response.status(), StatusCode::OK);

    let get_large_response = http
        .get(&large_url)
        .bearer_auth(TEST_BEARER_TOKEN)
        .send()
        .await
        .expect("get large after put");
    assert_eq!(get_large_response.status(), StatusCode::OK);
    let large_cache_status = get_large_response
        .headers()
        .get("X-CG-Status")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    assert_eq!(large_cache_status, "hit=0");
    let large_body = get_large_response.bytes().await.expect("read large body");
    assert_eq!(large_body.as_ref(), large_payload.as_slice());

    let overwrite_payload = b"cachegate put overwrite".to_vec();
    let overwrite_response = http
        .put(&put_url)
        .bearer_auth(TEST_BEARER_TOKEN)
        .body(overwrite_payload.clone())
        .send()
        .await
        .expect("put overwrite");
    assert_eq!(overwrite_response.status(), StatusCode::OK);
    let overwrite_get = http
        .get(&put_url)
        .bearer_auth(TEST_BEARER_TOKEN)
        .send()
        .await
        .expect("get overwrite");
    assert_eq!(overwrite_get.status(), StatusCode::OK);
    let overwrite_body = overwrite_get.bytes().await.expect("read overwrite body");
    assert_eq!(overwrite_body.as_ref(), overwrite_payload.as_slice());
    let sig = build_sig(&signing_key, store_id, &object_key, "GET");
    let url = format!("{base_url}/{store_id}/{object_key}?sig={sig}");

    let bearer_url = format!("{base_url}/{store_id}/{object_key}");
    let response = http
        .get(&bearer_url)
        .bearer_auth(TEST_BEARER_TOKEN)
        .send()
        .await
        .expect("bearer get");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.bytes().await.expect("read bearer body");
    assert_eq!(body.as_ref(), payload.as_slice());

    let response = http.get(&url).send().await.expect("first get");
    assert_eq!(response.status(), StatusCode::OK);
    let status_header = response
        .headers()
        .get("X-CG-Status")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    assert_eq!(status_header, "hit=1");
    let body = response.bytes().await.expect("read body");
    assert_eq!(body.as_ref(), payload.as_slice());

    let response = http.get(&url).send().await.expect("second get");
    assert_eq!(response.status(), StatusCode::OK);
    let status_header = response
        .headers()
        .get("X-CG-Status")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    assert_eq!(status_header, "hit=1");
    let body = response.bytes().await.expect("read body 2");
    assert_eq!(body.as_ref(), payload.as_slice());

    // HEAD happy path: object exists, return headers only.
    let head_sig = build_sig(&signing_key, store_id, &object_key, "HEAD");
    let head_url = format!("{base_url}/{store_id}/{object_key}?sig={head_sig}");
    let head_response = http.head(&head_url).send().await.expect("head");
    assert_eq!(head_response.status(), StatusCode::OK);
    let head_content_length = head_response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    assert_eq!(head_content_length, payload.len().to_string());
    let head_content_type = head_response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    assert!(head_content_type.starts_with("text/plain"));
    assert!(head_response.headers().get("X-CG-Status").is_none());
    let head_body = head_response.bytes().await.expect("head body");
    assert!(head_body.is_empty());

    // HEAD not found: unknown object should return 404.
    let missing_key = format!("missing-{}.txt", unix_timestamp());
    let missing_head_sig = build_sig(&signing_key, store_id, &missing_key, "HEAD");
    let missing_head_url = format!("{base_url}/{store_id}/{missing_key}?sig={missing_head_sig}");
    let missing_head = http
        .head(&missing_head_url)
        .send()
        .await
        .expect("missing head");
    assert_eq!(missing_head.status(), StatusCode::NOT_FOUND);

    // HEAD auth failure: bad signature should be unauthorized.
    let mut bad_head_sig = head_sig.clone();
    bad_head_sig.pop();
    bad_head_sig.push('x');
    let bad_head_url = format!("{base_url}/{store_id}/{object_key}?sig={bad_head_sig}");
    let bad_head = http.head(&bad_head_url).send().await.expect("bad head sig");
    assert_eq!(bad_head.status(), StatusCode::UNAUTHORIZED);

    let stats_before = fetch_stats(&http, &base_url).await;
    let initial_entries = stats_before.cache.entries;

    let prefetch_off_key = format!("prefetch-off-{}.txt", unix_timestamp());
    let prefetch_off_payload = b"prefetch off".to_vec();
    put_object(
        &client,
        &bucket,
        &prefetch_off_key,
        prefetch_off_payload.clone(),
    )
    .await;
    let prefetch_off_sig = build_sig(&signing_key, store_id, &prefetch_off_key, "HEAD");
    let prefetch_off_url =
        format!("{base_url}/{store_id}/{prefetch_off_key}?sig={prefetch_off_sig}");
    let prefetch_off_response = http
        .head(&prefetch_off_url)
        .send()
        .await
        .expect("head prefetch off");
    assert_eq!(prefetch_off_response.status(), StatusCode::OK);
    let prefetch_off_length = prefetch_off_response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    assert_eq!(prefetch_off_length, prefetch_off_payload.len().to_string());
    assert_cache_entries_unchanged_for(
        &http,
        &base_url,
        initial_entries,
        Duration::from_millis(400),
    )
    .await;

    let prefetch_on_key = format!("prefetch-on-{}.txt", unix_timestamp());
    let prefetch_on_payload = b"prefetch on".to_vec();
    put_object(
        &client,
        &bucket,
        &prefetch_on_key,
        prefetch_on_payload.clone(),
    )
    .await;
    let prefetch_on_sig = build_sig(&signing_key, store_id, &prefetch_on_key, "HEAD");
    let prefetch_on_url =
        format!("{base_url}/{store_id}/{prefetch_on_key}?sig={prefetch_on_sig}&prefetch=1");
    let prefetch_on_response = http
        .head(&prefetch_on_url)
        .send()
        .await
        .expect("head prefetch on");
    assert_eq!(prefetch_on_response.status(), StatusCode::OK);
    let prefetch_on_length = prefetch_on_response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    assert_eq!(prefetch_on_length, prefetch_on_payload.len().to_string());
    wait_for_cache_entries_at_least(
        &http,
        &base_url,
        initial_entries + 1,
        Duration::from_secs(3),
    )
    .await;

    let stats = http
        .get(format!("{base_url}/stats"))
        .send()
        .await
        .expect("stats")
        .json::<StatsResponse>()
        .await
        .expect("stats json");
    assert!(stats.cache_hit_total >= 1);
    assert!(stats.cache_miss_total >= 1);
    assert!(stats.cache.entries >= 1);

    let mut bad_sig = sig.clone();
    bad_sig.pop();
    bad_sig.push('x');
    let bad_url = format!("{base_url}/{store_id}/{object_key}?sig={bad_sig}");
    let response = http.get(&bad_url).send().await.expect("bad sig");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let bad_bearer = http
        .get(&bearer_url)
        .bearer_auth("bad-token")
        .send()
        .await
        .expect("bad bearer");
    assert_eq!(bad_bearer.status(), StatusCode::UNAUTHORIZED);

    let unknown_sig = build_sig(&signing_key, "unknown", &object_key, "GET");
    let unknown_url = format!("{base_url}/unknown/{object_key}?sig={unknown_sig}");
    let response = http.get(&unknown_url).send().await.expect("unknown bucket");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let metrics = http
        .get(format!("{base_url}/metrics"))
        .send()
        .await
        .expect("metrics")
        .text()
        .await
        .expect("metrics body");
    assert!(metrics.contains("cachegate_upstream_latency_ms_bucket"));
}

fn build_sig(signing_key: &SigningKey, bucket: &str, path: &str, method: &str) -> String {
    let payload = PresignPayload {
        v: 1,
        exp: unix_timestamp() + 300,
        m: method.to_string(),
        b: bucket.to_string(),
        p: path.to_string(),
    };
    let payload_bytes = serde_json::to_vec(&payload).expect("payload json");
    let signature: Signature = signing_key.sign(&payload_bytes);
    let payload_b64 = URL_SAFE_NO_PAD.encode(payload_bytes);
    let signature_b64 = URL_SAFE_NO_PAD.encode(signature.to_bytes());
    format!("{payload_b64}.{signature_b64}")
}

async fn minio_client() -> S3Client {
    let creds = Credentials::new(MINIO_ACCESS_KEY, MINIO_SECRET_KEY, None, None, "static");
    let region = Region::new(MINIO_REGION);

    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(region)
        .credentials_provider(creds)
        .load()
        .await;

    let s3_config = aws_sdk_s3::config::Builder::from(&config)
        .endpoint_url(MINIO_ENDPOINT)
        .force_path_style(true)
        .build();

    S3Client::from_conf(s3_config)
}

async fn create_bucket(client: &S3Client, bucket: &str) {
    match client.create_bucket().bucket(bucket).send().await {
        Ok(_) => {}
        Err(err) => {
            let message = err.to_string();
            if !message.contains("BucketAlreadyOwnedByYou")
                && !message.contains("BucketAlreadyExists")
            {
                panic!("create bucket failed: {message}");
            }
        }
    }
}

async fn put_object(client: &S3Client, bucket: &str, key: &str, body: Vec<u8>) {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(body))
        .send()
        .await
        .expect("put object");
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_secs() as i64
}

async fn wait_for_ready(base_url: &str) {
    let client = reqwest::Client::new();
    let mut last_error = None;
    for _ in 0..50 {
        match client.get(format!("{base_url}/health")).send().await {
            Ok(resp) if resp.status().is_success() => return,
            Ok(resp) => last_error = Some(format!("status {}", resp.status())),
            Err(err) => last_error = Some(err.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    panic!("cachegate not ready: {:?}", last_error);
}

async fn fetch_stats(client: &reqwest::Client, base_url: &str) -> StatsResponse {
    client
        .get(format!("{base_url}/stats"))
        .send()
        .await
        .expect("stats")
        .json::<StatsResponse>()
        .await
        .expect("stats json")
}

async fn wait_for_cache_entries_at_least(
    client: &reqwest::Client,
    base_url: &str,
    expected: u64,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        let stats = fetch_stats(client, base_url).await;
        if stats.cache.entries >= expected {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "expected cache entries >= {}, got {}",
                expected, stats.cache.entries
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn assert_cache_entries_unchanged_for(
    client: &reqwest::Client,
    base_url: &str,
    expected: u64,
    duration: Duration,
) {
    let deadline = Instant::now() + duration;
    loop {
        let stats = fetch_stats(client, base_url).await;
        if stats.cache.entries != expected {
            panic!(
                "expected cache entries to stay at {}, got {}",
                expected, stats.cache.entries
            );
        }
        if Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn ensure_minio_ready() {
    let client = reqwest::Client::new();
    let url = format!("{MINIO_ENDPOINT}/minio/health/ready");
    let response = client
        .get(url)
        .send()
        .await
        .expect("minio readiness check failed to send request");
    if !response.status().is_success() {
        panic!(
            "minio readiness check failed: {} returned {}",
            MINIO_ENDPOINT,
            response.status()
        );
    }
}
