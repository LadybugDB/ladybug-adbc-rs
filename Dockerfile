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
# Fast ladybug linkage (assembled by CI via scripts/fetch-lbug-deps.sh before
docker build): link the prebuilt static archive, compile the small cxx shims
against the real headers from the matching core commit. This avoids both
failure modes — pure prebuilt is missing common/arrow/*.h, and
LBUG_BUILD_FROM_SOURCE=1 compiles all of ladybug core (hours under QEMU).
lbug-deps/ layout: include/ (shared) + lib-<TARGETARCH>/liblbug.a.
(Local builds: run the fetch script first to create lbug-deps/.)
ARG TARGETARCH=amd64
COPY lbug-deps /opt/lbug-deps
# CXXFLAGS=-DLBUG_BUNDLED: ladybug-rust's headers switch on LBUG_BUNDLED
# (defined by its own bundled builds). Without it they take the amalgamated
# <lbug.hpp> branch, which redefines classes from common/vector/value_vector.h
# (both trees are on the include path). cc-rs picks CXXFLAGS up from env.
ENV LBUG_LIBRARY_DIR=/opt/lbug-deps/lib-${TARGETARCH} \
    LBUG_INCLUDE_DIR=/opt/lbug-deps/include \
    CXXFLAGS=-DLBUG_BUNDLED
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY benches ./benches
COPY tests ./tests
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
