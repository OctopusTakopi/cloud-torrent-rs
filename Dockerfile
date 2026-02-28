# STEP 1 build executable binary
############################
FROM rust:1-bookworm AS builder
RUN apt-get update && apt-get install -y pkg-config libssl-dev git build-essential ca-certificates curl

# Install wasm32-unknown-unknown target for Yew frontend
RUN rustup target add wasm32-unknown-unknown

ARG TARGETARCH
# Install trunk (architecture-aware)
RUN case "${TARGETARCH}" in \
    "amd64") TRUNK_ARCH="x86_64" ;; \
    "arm64") TRUNK_ARCH="aarch64" ;; \
    *) TRUNK_ARCH="x86_64" ;; \
    esac && \
    curl -L "https://github.com/thedodd/trunk/releases/download/v0.21.14/trunk-${TRUNK_ARCH}-unknown-linux-gnu.tar.gz" | tar -xzf- -C /usr/local/bin

WORKDIR /root/cloud-torrent
# Copy local source code
COPY . .

# Build frontend
RUN cd frontend && trunk build --release

# Build backend
RUN cargo build --release

############################
# STEP 2 build a small image
############################
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /root/cloud-torrent/target/release/cloud-torrent /usr/local/bin/cloud-torrent

WORKDIR /app
ENTRYPOINT ["cloud-torrent"]
