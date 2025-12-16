# ============================================================================
# Platform Chain - Optimized Multi-stage Docker Build
# ============================================================================

# Stage 1: Builder
FROM rust:slim-trixie AS builder

WORKDIR /app

# Install build dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    protobuf-compiler \
    clang \
    libclang-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy source code
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY bins ./bins
COPY tests ./tests

# Build release binaries
RUN cargo build --release --bin validator-node --bin csudo

# Strip binaries for smaller size
RUN strip /app/target/release/validator-node /app/target/release/csudo

# Stage 2: Runtime - Minimal production image
FROM debian:trixie-slim AS runtime

# Install runtime dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3t64 \
    curl \
    tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 1000 -m platform

WORKDIR /app

# Copy binaries from builder
COPY --from=builder /app/target/release/validator-node /usr/local/bin/
COPY --from=builder /app/target/release/csudo /usr/local/bin/

# Create data directory
RUN mkdir -p /data

# Environment
ENV RUST_LOG=info,platform_chain=debug
ENV DATA_DIR=/data

# Expose ports (RPC: 8080, P2P: 9000)
EXPOSE 8080 9000

# Use tini as init system
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["validator-node", "--data-dir", "/data"]

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -sf http://localhost:8080/health || exit 1

# Labels
LABEL org.opencontainers.image.source="https://github.com/PlatformNetwork/platform"
LABEL org.opencontainers.image.description="Platform Chain Validator Node"
LABEL org.opencontainers.image.licenses="MIT"
