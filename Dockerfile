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
#
# icebug (the NetworKit-backed Rust crate, behind the `icebug-analytics`
# feature flag) requires three Linux system libs at build/run time:
#   * libarrow-dev  — Apache Arrow C++ headers (icebug's cxx bridge
#                     `#include <arrow/api.h>`). Trixie does not ship
#                     Apache Arrow, hence the Apache apt source below.
#   * GCC           — `<omp.h>` for the cxx bridge ships with libgcc-14-dev
#                     (pulled in by build-essential). The prebuilt
#                     libnetworkit.so is linked against GCC's libgomp
#                     (`NEEDED libgomp.so.1`); LLVM's libomp is
#                     ABI-incompatible, so we must match.
#   * libssl-dev    — libnetworkit.so pulls in libcurl/libssl transitively.
#   * pkg-config    — icebug-rust's build.rs probes Arrow via pkg-config.
# 1.90 satisfies the rustc >= 1.88 floor set by cxx 1.0.202 / time 0.3.55
# (pulled in transitively via lbug 0.21.1) and tracks one stable
# behind current.
FROM rust:1.90-trixie AS builder

# Apache Arrow C++ dev headers. Trixie is not in stock Debian; the
# Apache repo publishes a .deb that registers the source list. Mirrors
# ../bugscope/.github/workflows/build.yml's "Apache Arrow repo" block.
ARG ARROW_APT_DEB=https://apache.jfrog.io/artifactory/arrow/debian/apache-arrow-apt-source-latest-trixie.deb
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
    && curl -fsSL -o /tmp/arrow-apt.deb "$ARROW_APT_DEB" \
    && apt-get install -y /tmp/arrow-apt.deb \
    && rm /tmp/arrow-apt.deb \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential cmake pkg-config \
        libssl-dev \
        libarrow-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY benches ./benches
COPY tests ./tests
COPY scripts ./scripts
COPY .cargo ./.cargo

# Bundled shared liblbug (downloaded, not committed): required so the
# dlopen'd algo extension can resolve ladybug symbols from the host
# process at LOAD EXTENSION time. Without a shared library, dead-code
# stripping drops unreferenced symbols (e.g.
# lbug::function::TableFuncBindData) and the extension fails to load
# with "undefined symbol: _ZTIN4lbug8function17TableFuncBindDataE".
RUN bash scripts/download-liblbug.sh

# Bundled NetworKit prebuilt (downloaded, not committed). The icebug Rust
# crate's build script links against ICEBUG_DIR (./icebug — see
# .cargo/config.toml); the extension build also reads the headers under
# include/networkit. idempotent: bails out if the archive is already
# extracted.
RUN bash scripts/download-icebug.sh

# Warm dependency build before the final binary (Cargo.lock optional).
RUN cargo build --release --bin ladybug-flight-server \
    --bin ladybug-client --bin ladybug-healthcheck --bin ladybug-bench

# Build the gds_page_rank integration test so the runtime image can run
# it as a smoke check (`docker run --rm --entrypoint ...`). This pulls
# in the same compiled deps as the bins above, so it's near-free.
RUN cargo test --release --no-run --test gds_page_rank \
    # cargo's test depfiles (`gds_page_rank-XXXXXXXX.d`) sit next to the
    # binary and share the same hash prefix, so COPY globs match both
    # and only keep one. Stage the executable under a fixed name so the
    # runtime COPY is unambiguous.
    && find /build/target/release/deps -maxdepth 1 -name 'gds_page_rank-*' -type f -executable -exec cp {} /build/gds_page_rank_smoke_check \; \
    && ls -la /build/gds_page_rank_smoke_check

FROM debian:trixie-slim AS runtime

LABEL org.opencontainers.image.title="ladybug-adbc-rs"
LABEL org.opencontainers.image.description="Arrow Flight / ADBC columnar server for LadybugDB (Rust)"
LABEL org.opencontainers.image.source="https://github.com/LadybugDB/ladybug-adbc-rs"

# Switch to root for setup, then drop to appuser at the end.
USER root

# libgomp / libarrow / libssl / ca-certificates must survive in the slim
# image: libnetworkit.so (and the dlopen'd algo extension) need them at
# runtime. libstdc++6 / libgcc-s1 keep the lbug C++ bridge happy.
# Apache Arrow isn't in stock Debian either; pull from the Apache repo
# so the runtime libs match the builder's headers (same Arrow version).
ARG ARROW_APT_DEB=https://apache.jfrog.io/artifactory/arrow/debian/apache-arrow-apt-source-latest-trixie.deb
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
    && curl -fsSL -o /tmp/arrow-apt.deb "$ARROW_APT_DEB" \
    && apt-get install -y /tmp/arrow-apt.deb \
    && rm /tmp/arrow-apt.deb \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates libstdc++6 libgcc-s1 \
        libgomp1 libarrow2500 libssl3t64 \
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

# Stage the integration test binary as a smoke check (`docker run --rm
# --entrypoint /usr/local/bin/gds_page_rank_smoke_check`). Builder
# already staged it under a fixed name to dodge the cargo .d-vs-binary
# glob ambiguity.
COPY --from=builder /build/gds_page_rank_smoke_check /usr/local/bin/gds_page_rank_smoke_check

# Stage the bundled shared liblbug next to the binary so the binary's
# embedded rpath ($ORIGIN/liblbug) resolves liblbug.so at runtime. The
# dlopen'd algo extension uses the SAME liblbug (it looks up ladybug
# symbols from the host process), so it must be on the loader path.
# Mirror bugscope's dist layout (binary + ./icebug + ./liblbug side by
# side): drop the assets into /usr/local/bin, not /usr/local/, so the
# rpath is unambiguous regardless of how the binary is invoked.
COPY --from=builder /build/liblbug /usr/local/bin/liblbug

# Stage the bundled NetworKit prebuilt next to the binary so the binary's
# embedded rpath ($ORIGIN/icebug/lib) resolves libnetworkit.so at
# runtime. The system libarrow / libomp above are the *load* dependencies
# of libnetworkit.so; libnetworkit itself is the only artifact that has
# to ship in the image.
COPY --from=builder /build/icebug /usr/local/bin/icebug

USER appuser

# The dlopen'd algo extension (`LOAD algo`) depends on libnetworkit.so,
# but the server binary does not link it directly (only the test binary
# does, via the icebug crate). The loader therefore cannot find
# libnetworkit.so through the binary's RPATH at dlopen time — point it
# at the bundled copy explicitly. (liblbug.so resolves via RPATH
# already, but listing it here too is harmless.)
ENV LD_LIBRARY_PATH=/usr/local/bin/icebug/lib:/usr/local/bin/liblbug

ENV FLIGHT_HOST=0.0.0.0 \
    FLIGHT_PORT=50051 \
    LADYBUG_DB=:memory:

EXPOSE 50051
VOLUME /data

HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
    CMD ["ladybug-healthcheck"]

CMD ["ladybug-flight-server"]