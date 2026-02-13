# Cachegate

Minimal read-only proxy for S3 and Azure Blob Storage.

Allows for presigned-URL style access to objects to allow for constrained public access.

Some design decisions are inspired by [Cachey](https://github.com/s2-streamstore/cachey).

## Features

- Designed exclusively for immutable blobs. Assumes objects are never modified or deleted after creation.
- `GET /:bucket_id/*path`
- `HEAD /:bucket_id/*path`
- `HEAD /:bucket_id/*path?prefetch=true|false|1|0`
  - Prefetch warms the cache without blocking HEAD
- Auth
  - Presigned URL auth via `?sig=<payload>.<signature>`
  - Bearer token auth via `Authorization: Bearer <token>`
- Modular store registry (`s3`, `azure`)
- In-memory LRU cache with TTL + max bytes
- Singleflight on cache misses to avoid thundering herd
- Content-Type prefill, from path with `magic` fallback.
- `/stats` and Prometheus-compatible `/metrics`.

## Config

Copy `config.example.yaml` to `config.yaml` and fill credentials, or use env-only mode with `--config env`.

Keys are base64url (no padding) Ed25519 keys.

Generate a new keypair and write it to a YAML file:

```bash
cargo run -- keygen --out auth.keys.yaml
```

```yaml
listen: "0.0.0.0:8080"

auth:
  public_key: "BASE64URL_PUBLIC_KEY"
  private_key: "BASE64URL_PRIVATE_KEY"
  bearer_token: "OPTIONAL_STATIC_TOKEN"

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
    container: "assets"
    connection_string: "DefaultEndpointsProtocol=https;AccountName=my-account;AccountKey=...;EndpointSuffix=core.windows.net"
```

## Presigned auth

`sig` is a base64url JSON payload and a base64url Ed25519 signature of the raw payload bytes.

Payload fields:

```json
{"v":1,"exp":1730000000,"m":"GET","b":"media-s3","p":"path/to/object.txt"}
```

The request is:

```
GET /media-s3/path/to/object.txt?sig=<payload_b64>.<signature_b64>
```

Notes:

- `p` is the decoded request path after `/:bucket_id/`.
- `exp` is a unix timestamp in seconds.
- `GET` is accepted for fetch.
- `HEAD` is accepted for metadata-only fetch.
- `prefetch` is optional for `HEAD`. Default is `false`.

## Bearer token format

If `auth.bearer_token` is set, you can authenticate requests with:

```
Authorization: Bearer <token>
```

Bearer and presigned auth are both accepted for `GET` and `HEAD`.
If `bearer_token` is unset, only presign auth is available.

## Environment-only config

When using `--config env`, the entire config is read from the environment. We don't
merge with a config file at the moment: missing fields fail startup. There are two
variants:

```bash
# All in one
CACHEGATE_CONFIG="$(cat config.yaml)" # Config-yaml as a single env var

# Or flat env vars
CACHEGATE__LISTEN=localhost:9010

CACHEGATE__STORES__minio__type=s3
CACHEGATE__STORES__minio__endpoint=localhost:9305
CACHEGATE__STORES__minio__access_key=minioadmin
CACHEGATE__STORES__minio__secret_key=minioadmin
CACHEGATE__STORES__minio__region=us-east-1
CACHEGATE__STORES__minio__bucket=cachegate

# Azure via connection string
CACHEGATE__STORES__assets__type=azure
CACHEGATE__STORES__assets__container=assets
CACHEGATE__STORES__assets__CONNECTION_STRING="DefaultEndpointsProtocol=https;AccountName=my-account;AccountKey=...;EndpointSuffix=core.windows.net"

CACHEGATE__AUTH__PUBLIC_KEY=PfIG9MO7yrSFq4DNs7GPFC4CticILjGtqpoh43p3ipE
CACHEGATE__AUTH__PRIVATE_KEY=NC7y4q2_rmnWBhlnEo34B9FddA0DkGlu7XGOs76bZn8
CACHEGATE__AUTH__BEARER_TOKEN=cachegate-secret

CACHEGATE__CACHE__TTL_SECONDS=3600
CACHEGATE__CACHE__MAX_BYTES=524288000

# Optional
#CACHEGATE__SENTRY__DSN=
#CACHEGATE__SENTRY__ENVIRONMENT=
#CACHEGATE__SENTRY__TRACES_SAMPLE_RATE=
#CACHEGATE__SENTRY__RELASE=
#CACHEGATE__SENTRY__DEBUG=
```

## Run

```bash
cargo run -- config.yaml
```

```bash
cargo run -- --config env
```

## Docker

Build and run with env-only config (recommended for containers):

```bash
docker build -t cachegate .
docker run --rm -p 8080:8080 \
  -e CACHEGATE_CONFIG="$(cat config.example.yaml)" \
  cachegate --config env
```

Or use the provided compose files (base compose + prod overlay):

```bash
docker compose -f docker-compose.prod.yml up --build
```

## Tests (MinIO)

```bash
docker compose up -d
cargo test
```

## Monitoring

`GET /stats` returns JSON counters and cache size.

`GET /metrics` returns Prometheus metrics.

Optional Sentry instrumentation is enabled by setting `sentry.dsn` in config. Tracing is controlled by `sentry.traces_sample_rate`.

## Cache behavior

- LRU eviction on insert when `max_bytes` is exceeded
- TTL is enforced on read
- Objects larger than `max_bytes` are served but not cached
