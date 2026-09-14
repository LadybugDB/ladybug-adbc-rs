# syntax=docker/dockerfile:1
# LadybugDB Arrow Flight / ADBC server (Rust).
FROM rust:1.85-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential cmake pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY benches ./benches
COPY tests ./tests
# Warm dependency build before the final binary (Cargo.lock optional).
RUN cargo build --release --bin ladybug-flight-server \
    --bin ladybug-client --bin ladybug-healthcheck --bin ladybug-bench

FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="ladybug-adbc-rs"
LABEL org.opencontainers.image.description="Arrow Flight / ADBC columnar server for LadybugDB (Rust)"
LABEL org.opencontainers.image.source="https://github.com/LadybugDB/ladybug-adbc-rs"

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libstdc++6 libgcc-s1 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 10001 appuser \
    && mkdir -p /data \
    && chown -R appuser:appuser /data

COPY --from=builder \
    /build/target/release/ladybug-flight-server \
    /build/target/release/ladybug-client \
    /build/target/release/ladybug-healthcheck \
    /build/target/release/ladybug-bench \
    /usr/local/bin/
USER appuser

ENV FLIGHT_HOST=0.0.0.0 \
    FLIGHT_PORT=50051 \
    LADYBUG_DB=:memory:

EXPOSE 50051
VOLUME /data

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD ["ladybug-healthcheck"]

CMD ["ladybug-flight-server"]
