# syntax=docker/dockerfile:1
# ── Base: chef + build deps ───────────────────────────────────────────────────
FROM rust:1.99.1-slim-bookworm AS chef
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev libsqlite3-dev xz-utils \
    && rm -rf /var/lib/apt/lists/*
# cargo-chef and sccache as their released static binaries, pinned by checksum, instead
# of compiling both in every bot's build (about 3.5 minutes on a cold BuildKit cache,
# 2026-10-21). To upgrade: the new release's URLs and their sha256 (the release lists them).
ADD --checksum=sha256:aca691abfbfbbe00d482e0ed2249eec3091b65e96a0ce92947fb5b254d48b16d \
    https://github.com/LukeMathWalker/cargo-chef/releases/download/v0.1.78/cargo-chef-x86_64-unknown-linux-musl.tar.xz /tmp/cargo-chef.tar.xz
ADD --checksum=sha256:45f1447fbe231e3037bde351ef70677dd212216c8d62ae7ca409fecc4d6acc89 \
    https://github.com/mozilla/sccache/releases/download/v0.18.0/sccache-v0.18.0-x86_64-unknown-linux-musl.tar.gz /tmp/sccache.tar.gz
RUN tar -xJf /tmp/cargo-chef.tar.xz --strip-components=1 -C /usr/local/cargo/bin \
        cargo-chef-x86_64-unknown-linux-musl/cargo-chef \
    && tar -xzf /tmp/sccache.tar.gz --strip-components=1 -C /usr/local/cargo/bin \
        sccache-v0.18.0-x86_64-unknown-linux-musl/sccache \
    && rm /tmp/cargo-chef.tar.xz /tmp/sccache.tar.gz

# sccache wraps rustc so identical compilation work (e.g. the matrix-sdk /
# ruma / mxbot-common dependency graph shared by these bots) is reused from
# one BuildKit cache mount instead of recompiled per project. Incremental
# compilation writes its own per-crate cache that fights sccache's
# object-level cache, so it's disabled here per sccache's own guidance.
ENV RUSTC_WRAPPER=sccache \
    SCCACHE_DIR=/sccache \
    SCCACHE_CACHE_SIZE=20G \
    CARGO_INCREMENTAL=0
WORKDIR /build

# ── Planner: capture the full dependency graph ────────────────────────────────
FROM chef AS planner
COPY . .
RUN --mount=type=cache,id=shared-cargo-git,target=/usr/local/cargo/git,sharing=private \
    --mount=type=cache,id=shared-cargo-registry,target=/usr/local/cargo/registry,sharing=private \
    cargo chef prepare --recipe-path recipe.json

# ── Builder ───────────────────────────────────────────────────────────────────
FROM chef AS builder
# Cargo only locks its registry/git caches within one container, so builds
# running in parallel (build-bots.sh -j) get private copies of those caches
# instead of racing while unpacking crates. sccache stays shared.
COPY --from=planner /build/recipe.json recipe.json

RUN --mount=type=cache,id=shared-cargo-git,target=/usr/local/cargo/git,sharing=private \
    --mount=type=cache,id=shared-cargo-registry,target=/usr/local/cargo/registry,sharing=private \
    --mount=type=cache,id=shared-sccache,target=/sccache \
    --mount=type=cache,id=telegram-mirror-bot-target,target=/build/target \
    cargo chef cook --release --recipe-path recipe.json

COPY . .
RUN --mount=type=cache,id=shared-cargo-git,target=/usr/local/cargo/git,sharing=private \
    --mount=type=cache,id=shared-cargo-registry,target=/usr/local/cargo/registry,sharing=private \
    --mount=type=cache,id=shared-sccache,target=/sccache \
    --mount=type=cache,id=telegram-mirror-bot-target,target=/build/target \
    cargo build --release && \
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

# mxbot-common's heartbeat: the bot writes the Unix time into this file after every
# successful Matrix sync (long polling returns at least every 30 s), so a stale file means the
# sync is dead or the runtime is wedged. Set here so the image is healthy on its own; house-infra
# with `heartbeat: true` sets the same variable and the same check in compose.
ENV MXBOT_HEARTBEAT_FILE=/app/store/.heartbeat
HEALTHCHECK --interval=30s --timeout=5s --start-period=120s --retries=3 \
    CMD sh -c 'f="$MXBOT_HEARTBEAT_FILE"; [ -f "$f" ] && [ $(( $(date +%s) - $(cat "$f") )) -lt 120 ]'

CMD ["telegram-mirror-bot", "/app/config/config.toml"]
