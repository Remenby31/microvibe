# Stage 1: Build
FROM rust:1.83-slim AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN cargo build --release

# Stage 2: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    git \
    grep \
    curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/microvibe /usr/local/bin/microvibe

WORKDIR /workspace

ENTRYPOINT ["microvibe"]
