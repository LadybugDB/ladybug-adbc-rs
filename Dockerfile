# syntax=docker/dockerfile:1
# LadybugDB Arrow Flight / ADBC server (Rust).
# Trixie (Debian 13) for both stages:
# - builder: lbug.hpp (from the ladybug prebuilt headers) #include <format>s,
#   and <format> only landed in libstdc++ in GCC 13. Trixie ships GCC 14,
#   bookworm is still on GCC 12. The test job hides this because it runs on
#   ubuntu-latest (GCC 13+).
# - runtime: keep the same Debian major as the builder so libstdc++ ABI
#   matches (otherwise the binary would need newer GLIBCXX symbols than
#   bookworm-slim provides).
# 1.90 satisfies the rustc >= 1.88 floor set by cxx 1.0.202 / time 0.3.55
# (pulled in transitively via ladybug-rust 19c48ac4) and tracks one stable
# behind current.
FROM rust:1.90-trixie AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential cmake pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY benches ./benches
COPY tests ./tests
# Build from bundled source — the ladybug prebuilt artifacts only ship
# lbug.h / lbug.hpp / liblbug.a, so lbug_arrow.cpp's
# #include "common/arrow/arrow_converter.h" can't be resolved against
# the prebuilt. The precompiled-bin workflow in LadybugDB/ladybug would
# need to start bundling common/arrow/*.h to flip this back off.
ENV LBUG_BUILD_FROM_SOURCE=1
# Warm dependency build before the final binary (Cargo.lock optional).
RUN cargo build --release --bin ladybug-flight-server \
    --bin ladybug-client --bin ladybug-healthcheck --bin ladybug-bench

FROM debian:trixie-slim AS runtime

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
