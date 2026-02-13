# AGENTS.md

Project context and workflow guidance for AI-assisted development in Cachegate.

## Project Overview

**Cachegate** is a minimal read-only proxy for S3 and Azure Blob Storage.

**Core behaviors:**

- Presigned URL or bearer-token auth for `GET` and `HEAD`.
- Hybrid disk-memory LRU cache (Foyer) with TTL and max-bytes policy.
- Singleflight on cache misses to avoid thundering herd.
- `/stats` JSON and `/metrics` Prometheus endpoints.
- Optional Sentry tracing.

**Design assumption:** objects are immutable (no updates or deletes after creation).

## HTTP API (Quick Reference)

- `GET /{bucket_id}/{*path}`
- `HEAD /{bucket_id}/{*path}`
- `HEAD /{bucket_id}/{*path}?prefetch=true|false|1|0`
- `GET /stats`
- `GET /metrics`
- `GET /health`

Auth is required for `GET`/`HEAD` on object routes. Accepts presigned URL or bearer token.

## Configuration (Quick Reference)

- File-based config: copy `config.example.yaml` to `config.yaml`.
- Env-only config: use `--config env` with `CACHEGATE_CONFIG` or `CACHEGATE__...` vars.
- Keygen: `cargo run -- keygen --out auth.keys.yaml`.

## Tech Stack

- Rust (async, Tokio runtime)
- Axum for HTTP routing
- tower_http for tracing middleware
- tracing + tracing_subscriber for structured logging
- anyhow for error handling with context
- object_store for S3/Azure backends
- Hybrid cache via the foyer crate

## Project Structure

High-level layout:

```
src/main.rs           # CLI, config loading, tracing, HTTP router
src/handler.rs        # Request handlers and middleware
src/auth.rs           # Presigned and bearer auth
src/cache/mod.rs      # Cache interface/types
src/cache/memory.rs   # In-memory LRU cache
src/cache/foyer.rs    # Hybrid disk-memory cache (Foyer)
src/store/mod.rs      # Store registry (S3/Azure)
src/store/azure.rs    # Azure store builder
src/metrics.rs        # Prometheus metrics
src/inflight.rs       # Singleflight on cache misses
```

## Common Commands

```
cargo build
cargo +nightly fmt
cargo clippy --all-features --all-targets
cargo nextest run
cargo run -- --config env
cargo run -- keygen --out auth.keys.yaml
```

Integration tests require MinIO:

```
docker compose up -d
cargo test
```

## Testing Notes

- `cargo nextest run` runs unit/integration tests (integration requires MinIO).
- If MinIO is not reachable on `localhost:9305`, integration tests will fail.

## Coding Standards

### Logging and Tracing

- Always use structured logging with `tracing`.
- Prefer spans for request-level or operation-level context.
- Attach relevant fields (bucket_id, path, size_bytes, etc.).
- Avoid `println!` and ad-hoc logging.

### Error Handling

- Use `anyhow` and add context with `.context()`.
- Errors should be explicit and self-explanatory.

### Rust Style

- Idiomatic Rust, immutable by default.
- Prefer existing crates over custom implementations.
- If multiple crate options are viable, ask before choosing.

## Development Workflow

1. Search before implementing (avoid duplicate logic).
2. Read all impacted files fully.
3. Implement minimal, boring changes.
4. Update tests and docs as needed.
5. Run fmt, clippy, and nextest in the REPL cycle.

## Debugging Tips

- Enable logs with `RUST_LOG=debug` or module-specific filters.
- Check Sentry config if trace spans are missing.
- For integration tests, confirm MinIO is reachable at `localhost:9305`.

## Common Pitfalls

- Forgetting to start MinIO before running integration tests.
- Treating objects as mutable (design assumes immutability).
- Adding sync/blocking work in async handlers.

## Documentation Map

- Project overview and config examples: `README.md`.
- Sample config template: `config.example.yaml`.

## Mandatory Principles (AI Assistants)

**Critical:** do not be a yes-man. Challenge design choices that violate core rules.

Core rules:

- Single responsibility per function.
- Boring > clever.
- Search before you implement.
- DRY: avoid duplicated knowledge and logic.
- Validate preconditions early.
- Prefer explicit, contextual errors.

When to push back:

- Proposed change adds redundant state or logic.
- Implementation would block in async contexts.
- Design violates immutability or structured logging standards.
- Change adds complexity without clear benefit.

## Pre-Submission Checklist

- [ ] Searched for existing functionality before writing new code.
- [ ] Read all relevant files fully.
- [ ] Added structured logs/spans where appropriate.
- [ ] Added error context for fallible operations.
- [ ] Ran `cargo +nightly fmt`.
- [ ] Ran `cargo clippy --all-features --all-targets`.
- [ ] Ran `cargo nextest run` (or documented why it could not run).
