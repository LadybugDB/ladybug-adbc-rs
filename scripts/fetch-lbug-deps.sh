#!/usr/bin/env bash
# Assemble ladybug link+header deps for ladybug-rust's "external" build path
# (LBUG_LIBRARY_DIR + LBUG_INCLUDE_DIR), so downstream builds neither fail
# on missing headers nor compile all of ladybug core from source.
#
# Background: ladybug-rust >= 19c48ac4 compiles src/lbug_arrow.cpp, which
# #includes "common/arrow/arrow_converter.h". The prebuilt artifacts
# (liblbug-static-*-compat.tar.gz) ship only lbug.h / lbug.hpp / liblbug.a,
# so a pure-prebuilt build fails at that #include; LBUG_BUILD_FROM_SOURCE=1
# instead compiles the whole core (30-60 min natively, hours under QEMU).
# This script takes the middle path: link the prebuilt static archive and
# compile the small cxx shims against the real headers from the matching
# core commit.
#
# Usage: fetch-lbug-deps.sh <core-run-id> <core-sha> <out-dir> <arch...>
#   arch is docker-style: amd64, arm64
# Layout:
#   <out-dir>/include/            merged headers (lbug.h/lbug.hpp + src/include + third_party bits)
#   <out-dir>/lib-<arch>/liblbug.a (+ lbug.h/lbug.hpp copies)
#
# Needs: curl, tar, and `gh` authenticated (GH_TOKEN) for run artifacts.
set -euo pipefail

RUN_ID="${1:?usage: $0 <core-run-id> <core-sha> <out-dir> <arch...>}"
CORE_SHA="${2:?usage: $0 <core-run-id> <core-sha> <out-dir> <arch...>}"
OUT="${3:?usage: $0 <core-run-id> <core-sha> <out-dir> <arch...>}"
shift 3
[ "$#" -ge 1 ] || { echo "need at least one arch (amd64, arm64)" >&2; exit 1; }
REPO="${LBUG_GITHUB_REPOSITORY:-LadybugDB/ladybug}"

artifact_for() {
  case "$1" in
    amd64) echo "liblbug-static-linux-x86_64-compat" ;;
    arm64) echo "liblbug-static-linux-aarch64-compat" ;;
    *) echo "unknown arch: $1 (want amd64|arm64)" >&2; exit 1 ;;
  esac
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$OUT/include"

# 1. Source headers from the exact core commit the prebuilt was built from
#    (public tarball, no auth needed).
echo "Fetching ladybug source headers at $CORE_SHA ..." >&2
curl -fsSL "https://github.com/${REPO}/archive/${CORE_SHA}.tar.gz" -o "$TMP/lbug-src.tar.gz"
mkdir -p "$TMP/src"
tar -xzf "$TMP/lbug-src.tar.gz" -C "$TMP/src" --strip-components=1
cp -r "$TMP/src/src/include/." "$OUT/include/"
# Mirror the extra -I dirs of ladybug-rust's bundled cmake build, in case any
# header in the closure reaches into third_party.
for d in third_party/nlohmann_json third_party/fastpfor third_party/alp/include; do
  if [ -d "$TMP/src/$d" ]; then
    cp -r "$TMP/src/$d/." "$OUT/include/"
  fi
done

# 2. Prebuilt static archive per arch (run artifacts need gh + GH_TOKEN).
for arch in "$@"; do
  name="$(artifact_for "$arch")"
  libdir="$OUT/lib-$arch"
  mkdir -p "$libdir" "$TMP/dl-$arch"
  echo "Downloading $name from run $RUN_ID ..." >&2
  gh run download "$RUN_ID" --repo "$REPO" --name "$name" --dir "$TMP/dl-$arch" >/dev/null
  tarball="$(find "$TMP/dl-$arch" -name '*.tar.gz' | head -n1)"
  [ -n "$tarball" ] || { echo "artifact $name contains no tarball" >&2; exit 1; }
  tar -xzf "$tarball" -C "$libdir"
  [ -f "$libdir/liblbug.a" ] || { echo "liblbug.a missing in $name" >&2; exit 1; }
  # The external build mode only adds LBUG_INCLUDE_DIR (not the lib dir) to
  # the cxx include path, so the public headers must live there too.
  cp "$libdir/lbug.h" "$libdir/lbug.hpp" "$OUT/include/"
done

echo "Assembled ladybug deps in $OUT:" >&2
find "$OUT" -maxdepth 2 | sort >&2
