FROM rust:1.93-bookworm AS builder

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        pkg-config \
        libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && printf 'fn main() {}\n' > src/main.rs
RUN cargo build --release --locked

COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --gid 10001 cachegate \
    && useradd --uid 10001 --gid 10001 --home /app --shell /usr/sbin/nologin cachegate

COPY --from=builder /app/target/release/cachegate /usr/local/bin/cachegate

USER 10001
WORKDIR /app

EXPOSE 8080

ENTRYPOINT ["cachegate"]
CMD ["--config", "env"]
