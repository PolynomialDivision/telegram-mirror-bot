# syntax=docker/dockerfile:1
#
# Build via build-bots.sh which injects the local matrix-rust-sdk as a named
# build context (--build-context matrix-sdk=...).  Cargo.toml patches resolve
# path = "../matrix-rust-sdk" against WORKDIR /build → /matrix-rust-sdk.
#
# ── Base: chef + build deps ───────────────────────────────────────────────────
FROM rust:1.98.1-slim-bookworm AS chef
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev libsqlite3-dev \
    && rm -rf /var/lib/apt/lists/*
# sccache (via openssl-sys) needs pkg-config/libssl-dev at its own build
# time, so this must come after the apt-get above.
RUN cargo install cargo-chef sccache --locked

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
    --mount=type=cache,id=shared-sccache,target=/sccache \
    --mount=type=cache,id=telegram-mirror-bot-target,target=/build/target \
    cargo chef cook --release --recipe-path recipe.json

# cargo-chef writes path-dependency skeletons while cooking the recipe. Restore
# the real SDK workspace before the final locked build.
COPY --from=matrix-sdk . /matrix-rust-sdk/
COPY . .
RUN --mount=type=cache,id=shared-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=shared-cargo-registry,target=/usr/local/cargo/registry \
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

# The bot touches store/.heartbeat every ~30s from a task independent of Matrix/Telegram
# traffic, so a stale file means the async runtime itself is wedged (deadlock), not just
# "no messages recently". A missing binary/crashed process is already caught by Docker's
# own container-state tracking; this check is for the "running but stuck" case that leaves.
HEALTHCHECK --interval=30s --timeout=5s --start-period=45s --retries=3 \
    CMD sh -c 'f=store/.heartbeat; [ -f "$f" ] && [ $(( $(date +%s) - $(stat -c %Y "$f") )) -lt 90 ]'

CMD ["telegram-mirror-bot", "/app/config/config.toml"]
