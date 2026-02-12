# Agent Guidelines for CacheGate

- Format with `cargo +nightly fmt`
- Lint `cargo clippy --all-features --all-targets`
- Tests `cargo nextest run`
- Always do structured logging. We use tracing. No other means please.
- Where appropriate create spans, and add suitable attributes to them. This will
  make debugging much easier.
- Use `anyhow` for error handling. Don't forget to add context to errors with `.context()`.
- Use always rust idioms and best practises
- Immutable everything by default unless absolutely necessary. Please!
- **Do not** reinvent the wheel. If you need to do something, check if there's a
  crate for it first. If there is, use it. Prefer popular, well-maintained
  crates. If there are multiple good options, ask me for advice.
