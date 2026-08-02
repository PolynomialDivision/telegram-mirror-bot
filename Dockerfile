# syntax=docker/dockerfile:1
#
# Build via build-bots.sh which injects the local matrix-rust-sdk as a named
# build context (--build-context matrix-sdk=...).  Cargo.toml patches resolve
# path = "../matrix-rust-sdk" against WORKDIR /build → /matrix-rust-sdk.
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
# Inject the SDK from the named build context before analysing deps.
COPY --from=matrix-sdk . /matrix-rust-sdk/
COPY . .
RUN --mount=type=cache,id=shared-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=shared-cargo-registry,target=/usr/local/cargo/registry \
    cargo chef prepare --recipe-path recipe.json

# ── Builder ───────────────────────────────────────────────────────────────────
FROM chef AS builder
COPY --from=matrix-sdk . /matrix-rust-sdk/
COPY --from=planner /build/recipe.json recipe.json

RUN --mount=type=cache,id=shared-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=shared-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=telegram-mirror-bot-target,target=/build/target \
    cargo chef cook --release --recipe-path recipe.json

# cargo-chef writes path-dependency skeletons while cooking the recipe. Restore
# the real SDK workspace before the final locked build.
COPY --from=matrix-sdk . /matrix-rust-sdk/
COPY . .
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
