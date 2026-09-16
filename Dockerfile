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
# docker build): link the prebuilt static archive, compile the small cxx shims
# against the real headers from the matching core commit. This avoids both
# failure modes — pure prebuilt is missing common/arrow/*.h, and
# LBUG_BUILD_FROM_SOURCE=1 compiles all of ladybug core (hours under QEMU).
# lbug-deps/ layout: include/ (shared) + lib-<TARGETARCH>/liblbug.a.
# (Local builds: run the fetch script first to create lbug-deps/.)
COPY lbug-deps /opt/lbug-deps
# Pick the static archive by the actual machine arch (uname -m reports the
# emulated arch under QEMU) and verify its ELF machine with objdump BEFORE
# the ~45 min release compile: linking an x86_64 liblbug.a into the arm64
# link fails at the very end with "Relocations in generic ELF (EM: 62)".
# (Deliberately not via buildx's TARGETARCH: the arm64 leg once resolved it
# to the amd64 default and burned a full build.)
RUN set -eu; \
    echo "uname -m: $(uname -m)"; \
    ls /opt/lbug-deps; \
    echo '--- lib-amd64:'; objdump -a /opt/lbug-deps/lib-amd64/liblbug.a | grep -m1 'file format'; \
    echo '--- lib-arm64:'; objdump -a /opt/lbug-deps/lib-arm64/liblbug.a | grep -m1 'file format'; \
    case "$(uname -m)" in \
      aarch64|arm64) SEL=arm64; WANT=littleaarch64;; \
      x86_64|amd64) SEL=amd64; WANT=x86-64;; \
      *) echo "unknown arch $(uname -m)"; exit 1;; \
    esac; \
    echo "selecting lib-$SEL, expecting $WANT"; \
    objdump -a "/opt/lbug-deps/lib-$SEL/liblbug.a" | grep -m1 'file format' | grep -q "$WANT" \
      || { echo 'ARCH MISMATCH between selected liblbug.a and builder'; exit 1; }; \
    ln -sfn "/opt/lbug-deps/lib-$SEL" /opt/lbug-active; \
    ls /opt/lbug-active
# CXXFLAGS=-DLBUG_BUNDLED: ladybug-rust's headers switch on LBUG_BUNDLED
# (defined by its own bundled builds). Without it they take the amalgamated
# <lbug.hpp> branch, which redefines classes from common/vector/value_vector.h
# (both trees are on the include path). cc-rs picks CXXFLAGS up from env.
ENV LBUG_LIBRARY_DIR=/opt/lbug-active \
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
