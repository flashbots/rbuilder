#
# Base container (with sccache and cargo-chef)
#
# - https://github.com/mozilla/sccache
# - https://github.com/LukeMathWalker/cargo-chef
#
# Based on https://depot.dev/blog/rust-dockerfile-best-practices
#
ARG FEATURES
ARG RBUILDER_BIN="rbuilder"

FROM rust:1.82 AS base

RUN apt-get update \
    && apt-get install -y clang libclang-dev

RUN rustup component add clippy rustfmt


RUN cargo install sccache --version ^0.8
RUN cargo install cargo-chef --version ^0.1


ENV CARGO_HOME=/usr/local/cargo
ENV RUSTC_WRAPPER=sccache
ENV SCCACHE_DIR=/sccache

#
# Planner container (running "cargo chef prepare")
#
FROM base AS planner
WORKDIR /app
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=$SCCACHE_DIR,sharing=locked \
    cargo chef prepare --recipe-path recipe.json

#
# Builder container (running "cargo chef cook" and "cargo build --release")
#
FROM base AS builder
WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=$SCCACHE_DIR,sharing=locked \
    cargo chef cook --release --recipe-path recipe.json
COPY . .


FROM builder AS rbuilder
ARG RBUILDER_BIN
ARG FEATURES
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=$SCCACHE_DIR,sharing=locked \
    cargo build --release --features="$FEATURES" --package=${RBUILDER_BIN}

FROM builder AS test-relay
ARG FEATURES
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=$SCCACHE_DIR,sharing=locked \
    cargo build --release --features="$FEATURES" --package=test-relay


# Runtime container for test-relay
FROM gcr.io/distroless/cc-debian12 AS test-relay-runtime
WORKDIR /app
COPY --from=test-relay /app/target/release/test-relay /app/test-relay
ENTRYPOINT ["/app/test-relay"]

# Runtime container for rbuilder
FROM gcr.io/distroless/cc-debian12 AS rbuilder-runtime
ARG RBUILDER_BIN
WORKDIR /app
COPY --from=rbuilder /app/target/release/${RBUILDER_BIN} /app/rbuilder
ENTRYPOINT ["/app/rbuilder"]

