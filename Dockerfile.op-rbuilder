#
# Base container (with sccache and cargo-chef)
#
# - https://github.com/mozilla/sccache
# - https://github.com/LukeMathWalker/cargo-chef
#
# Based on https://depot.dev/blog/rust-dockerfile-best-practices
#
ARG FEATURES
ARG RBUILDER_BIN="op-rbuilder"

FROM rust:1.86 AS base
ARG TARGETPLATFORM

RUN apt-get update \
    && apt-get install -y clang libclang-dev

RUN rustup component add clippy rustfmt


RUN set -eux; \
    case "$TARGETPLATFORM" in \
      "linux/amd64")  ARCH_TAG="x86_64-unknown-linux-musl" ;; \
      "linux/arm64")  ARCH_TAG="aarch64-unknown-linux-musl" ;; \
      *) \
        echo "Unsupported platform: $TARGETPLATFORM"; \
        exit 1 \
        ;; \
    esac; \
    wget -O /tmp/sccache.tar.gz \
      "https://github.com/mozilla/sccache/releases/download/v0.8.2/sccache-v0.8.2-${ARCH_TAG}.tar.gz"; \
    tar -xf /tmp/sccache.tar.gz -C /tmp; \
    mv /tmp/sccache-v0.8.2-${ARCH_TAG}/sccache /usr/local/bin/sccache; \
    chmod +x /usr/local/bin/sccache; \
    rm -rf /tmp/sccache.tar.gz /tmp/sccache-v0.8.2-${ARCH_TAG}

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

# Runtime container for rbuilder
FROM gcr.io/distroless/cc-debian12 AS rbuilder-runtime
ARG RBUILDER_BIN
WORKDIR /app
COPY --from=rbuilder /app/target/release/${RBUILDER_BIN} /app/rbuilder
ENTRYPOINT ["/app/rbuilder"]

