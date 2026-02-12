# Cachegate

Minimal read-only proxy for S3 and Azure Blob Storage with presigned URL auth and in-memory LRU cache.

## Features

- `GET /:bucket_id/*path` only
- Presigned URL auth via `?sig=<payload>.<signature>`
- Modular store registry (`s3`, `azure`)
- In-memory LRU cache with TTL + max bytes
- Singleflight on cache misses to avoid duplicate upstream fetches
- Content-Type guessed from path, with magic fallback
- `/stats` JSON and `/metrics` Prometheus endpoints

## Config

Copy `config.example.yaml` to `config.yaml` and fill credentials.

Keys are base64url (no padding) Ed25519 keys.

```yaml
listen: "0.0.0.0:8080"

auth:
  public_key: "BASE64URL_PUBLIC_KEY"
  private_key: "BASE64URL_PRIVATE_KEY"

cache:
  ttl_seconds: 3600
  max_bytes: 1073741824

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

### Environment overrides

Environment variables override `config.yaml` when set.

Store IDs are normalized from env keys: single `_` becomes `-`, double `__` becomes `_`, and IDs are lowercased.

```
PROXY_LISTEN=0.0.0.0:8080
PROXY_AUTH_PUBLIC_KEY=BASE64URL_PUBLIC_KEY
PROXY_AUTH_PRIVATE_KEY=BASE64URL_PRIVATE_KEY
PROXY_CACHE_TTL_SECONDS=3600
PROXY_CACHE_MAX_BYTES=1073741824

PROXY_STORE_MEDIA_S3_TYPE=s3
PROXY_STORE_MEDIA_S3_BUCKET=my-bucket
PROXY_STORE_MEDIA_S3_REGION=us-east-1
PROXY_STORE_MEDIA_S3_ACCESS_KEY=AKIA...
PROXY_STORE_MEDIA_S3_SECRET_KEY=...
PROXY_STORE_MEDIA_S3_ENDPOINT=
PROXY_STORE_MEDIA_S3_ALLOW_HTTP=false

PROXY_STORE_ASSETS_AZURE_TYPE=azure
PROXY_STORE_ASSETS_AZURE_ACCOUNT=my-account
PROXY_STORE_ASSETS_AZURE_CONTAINER=assets
PROXY_STORE_ASSETS_AZURE_ACCESS_KEY=...
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

## Monitoring

`GET /stats` returns JSON counters and cache size.

`GET /metrics` returns Prometheus text with counters and an upstream latency histogram.

## Cache behavior

- LRU eviction on insert when `max_bytes` is exceeded
- TTL is enforced on read
- Objects larger than `max_bytes` are served but not cached
