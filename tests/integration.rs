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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MINIO_ENDPOINT: &str = "http://127.0.0.1:9000";
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
struct StatsResponse {
    cache_hit_total: u64,
    cache_miss_total: u64,
    cache: CacheStatsResponse,
}

#[derive(Deserialize)]
struct CacheStatsResponse {
    entries: u64,
    bytes: u64,
}

#[derive(Deserialize)]
struct PopulateResponse {
    cache_hit: bool,
    bytes: usize,
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
    let config_body = format!(
        r#"listen: "{listen}"

auth:
  public_key: "{public_b64}"
  private_key: "{private_b64}"
  bearer_token: "{TEST_BEARER_TOKEN}"

cache:
  ttl_seconds: 60
  max_bytes: 10485760

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
    let populate_sig = build_sig(&signing_key, store_id, &object_key, "POST");
    let populate_url = format!("{base_url}/populate/{store_id}/{object_key}?sig={populate_sig}");
    let populate = http.post(&populate_url).send().await.expect("populate");
    assert_eq!(populate.status(), StatusCode::OK);
    let populate_body = populate
        .json::<PopulateResponse>()
        .await
        .expect("populate json");
    assert!(!populate_body.cache_hit);
    assert_eq!(populate_body.bytes, payload.len());

    let mut bad_populate_sig = populate_sig.clone();
    bad_populate_sig.pop();
    bad_populate_sig.push('x');
    let bad_populate_url =
        format!("{base_url}/populate/{store_id}/{object_key}?sig={bad_populate_sig}");
    let bad_populate = http
        .post(&bad_populate_url)
        .send()
        .await
        .expect("bad populate sig");
    assert_eq!(bad_populate.status(), StatusCode::UNAUTHORIZED);

    let bearer_populate_url = format!("{base_url}/populate/{store_id}/{object_key}");
    let bearer_populate = http
        .post(&bearer_populate_url)
        .bearer_auth(TEST_BEARER_TOKEN)
        .send()
        .await
        .expect("bearer populate");
    assert_eq!(bearer_populate.status(), StatusCode::OK);
    let bearer_populate_body = bearer_populate
        .json::<PopulateResponse>()
        .await
        .expect("bearer populate json");
    assert!(bearer_populate_body.cache_hit);
    assert_eq!(bearer_populate_body.bytes, payload.len());

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
    assert!(stats.cache.bytes >= payload.len() as u64);

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
        match client.get(format!("{base_url}/stats")).send().await {
            Ok(resp) if resp.status().is_success() => return,
            Ok(resp) => last_error = Some(format!("status {}", resp.status())),
            Err(err) => last_error = Some(err.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    panic!("cachegate not ready: {:?}", last_error);
}
