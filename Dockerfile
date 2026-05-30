# syntax=docker/dockerfile:1
#
# Cargo.toml patches use path = "../matrix-rust-sdk", so build from the parent:
#   docker build -f telegram-mirror-bot/Dockerfile -t telegram-mirror-bot ..
#
# ── Base: chef + build deps ───────────────────────────────────────────────────
FROM rust:1.95-slim-bookworm AS chef
RUN cargo install cargo-chef --locked
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev libsqlite3-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build

# ── Planner ───────────────────────────────────────────────────────────────────
FROM chef AS planner
# The [patch.crates-io] entries point to ../matrix-rust-sdk which, with WORKDIR
# /build, resolves to /matrix-rust-sdk inside the container.
COPY matrix-rust-sdk/         /matrix-rust-sdk/
COPY telegram-mirror-bot/     .
RUN --mount=type=cache,id=shared-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=shared-cargo-registry,target=/usr/local/cargo/registry \
    cargo chef prepare --recipe-path recipe.json

# ── Builder ───────────────────────────────────────────────────────────────────
FROM chef AS builder
COPY matrix-rust-sdk/         /matrix-rust-sdk/
COPY --from=planner /build/recipe.json recipe.json

RUN --mount=type=cache,id=shared-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=shared-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=telegram-mirror-bot-target,target=/build/target \
    cargo chef cook --release --recipe-path recipe.json

COPY telegram-mirror-bot/     .
RUN --mount=type=cache,id=shared-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=shared-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=telegram-mirror-bot-target,target=/build/target \
    cargo build --release --locked && \
    cp target/release/telegram-mirror-bot /telegram-mirror-bot

# ── Runtime ───────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /telegram-mirror-bot /usr/local/bin/telegram-mirror-bot

VOLUME /app/store
VOLUME /app/config
WORKDIR /app
CMD ["telegram-mirror-bot", "/app/config/config.toml"]
