# Cachegate

Minimal read-only proxy for S3 and Azure Blob Storage.

Allows for presigned-URL style access to objects to allow for constrained public access.

Some design decisions are inspired by [Cachey](https://github.com/s2-streamstore/cachey).

## Features

- Designed exclusively for immutable blobs. Assumes objects are never modified or deleted after creation.
- `GET /:bucket_id/*path` only
  - Presigned URL auth via `?sig=<payload>.<signature>`
- Modular store registry (`s3`, `azure`)
- In-memory LRU cache with TTL + max bytes
- Singleflight on cache misses to avoid thundering herd
- Content-Type prefill, from path with `magic` fallback.
- `/stats` and Prometheus-compatible `/metrics`.

## Config

Copy `config.example.yaml` to `config.yaml` and fill credentials, or use env-only mode with `--config env`.

Keys are base64url (no padding) Ed25519 keys.

```yaml
listen: "0.0.0.0:8080"

auth:
  public_key: "BASE64URL_PUBLIC_KEY"
  private_key: "BASE64URL_PRIVATE_KEY"

cache:
  ttl_seconds: 3600
  max_bytes: 1073741824

sentry:
  dsn: null
  environment: null
  release: null
  traces_sample_rate: 0.1
  debug: false

stores:
  media-s3:
    type: s3
    bucket: "my-bucket"
    region: "us-east-1"
    access_key: "AKIA..."
    secret_key: "..."
    endpoint: null
    allow_http: false
  assets-azure:
    type: azure
    account: "my-account"
    container: "assets"
    access_key: "..."
```

## Presign format

`sig` is a base64url JSON payload and a base64url Ed25519 signature of the raw payload bytes.

Payload fields:

```json
{"v":1,"exp":1730000000,"m":"GET","b":"media-s3","p":"path/to/object.txt"}
```

### Environment-only config

When using `--config env`, the entire config is read from `CACHEGATE_CONFIG` as YAML or JSON. No merging happens, and missing fields fail startup.

```bash
export CACHEGATE_CONFIG="$(cat <<'EOF'
listen: "0.0.0.0:8080"

auth:
  public_key: "BASE64URL_PUBLIC_KEY"
  private_key: "BASE64URL_PRIVATE_KEY"

cache:
  ttl_seconds: 3600
  max_bytes: 1073741824

sentry:
  dsn: null
  environment: null
  release: null
  traces_sample_rate: 0.1
  debug: false

stores:
  media-s3:
    type: s3
    bucket: "my-bucket"
    region: "us-east-1"
    access_key: "AKIA..."
    secret_key: "..."
    endpoint: null
    allow_http: false
  assets-azure:
    type: azure
    account: "my-account"
    container: "assets"
    access_key: "..."
EOF
)"
```

The request is:

```
GET /media-s3/path/to/object.txt?sig=<payload_b64>.<signature_b64>
```

Notes:

- `p` is the decoded request path after `/:bucket_id/`.
- `exp` is a unix timestamp in seconds.
- Only `GET` is accepted.

## Run

```bash
cargo run -- config.yaml
```

```bash
cargo run -- --config env
```

## Tests (MinIO)

```bash
docker compose up -d
cargo test
```

## Monitoring

`GET /stats` returns JSON counters and cache size.

`GET /metrics` returns Prometheus text with counters and an upstream latency histogram.

Optional Sentry instrumentation is enabled by setting `sentry.dsn` in config. Tracing is controlled by `sentry.traces_sample_rate`.

## Cache behavior

- LRU eviction on insert when `max_bytes` is exceeded
- TTL is enforced on read
- Objects larger than `max_bytes` are served but not cached
