#!/usr/bin/env bash
# Download the Linux Icebug (NetworKit) prebuilt into ./icebug.
#
# Linux only — this crate does not ship macOS / Windows. The icebug Rust
# crate's build script (in dependency icebug-rust) links against
# ICEBUG_DIR (set in .cargo/config.toml): headers under include/networkit,
# the shared object at lib/libnetworkit.so. libnetworkit itself pulls in
# libarrow + libomp at runtime, which the Dockerfile installs system-wide.
#
# Mirrors ../bugscope/scripts/download_icebug.sh with the macOS / Windows
# branches stripped.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

REPOSITORY="${ICEBUG_GITHUB_REPOSITORY:-Ladybug-Memory/icebug}"
TARGET_DIR="${ICEBUG_TARGET_DIR:-$PROJECT_DIR/icebug}"
VERSION="${ICEBUG_VERSION:-13.2}"

if [ "$(uname -s)" != "Linux" ]; then
  echo "error: this crate only ships Linux icebug prebuilts (got $(uname -s))" >&2
  exit 1
fi

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64)
    ASSET_ARCH="x86_64"
    ;;
  aarch64|arm64)
    ASSET_ARCH="arm64"
    ;;
  *)
    echo "Unsupported Linux architecture: $ARCH" >&2
    exit 1
    ;;
esac

ARCHIVE="icebug-linux-${ASSET_ARCH}.tar.gz"
LIB_NAME="libnetworkit.so"

if [ -f "$TARGET_DIR/lib/$LIB_NAME" ] && [ -d "$TARGET_DIR/include/networkit" ]; then
  echo "icebug already exists in $TARGET_DIR"
  exit 0
fi

mkdir -p "$TARGET_DIR"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

DOWNLOAD_URL="https://github.com/${REPOSITORY}/releases/download/${VERSION}/${ARCHIVE}"
echo "Downloading $DOWNLOAD_URL ..."
curl -fSL "$DOWNLOAD_URL" -o "$TMPDIR/$ARCHIVE"

rm -rf "$TARGET_DIR/include" "$TARGET_DIR/lib" "$TARGET_DIR/extlibs"
tar xzf "$TMPDIR/$ARCHIVE" -C "$TARGET_DIR"

if [ ! -f "$TARGET_DIR/lib/$LIB_NAME" ]; then
  echo "Expected Icebug library not found at $TARGET_DIR/lib/$LIB_NAME" >&2
  echo "Archive contents:" >&2
  ls "$TARGET_DIR" >&2
  exit 1
fi

echo "Installed $ARCHIVE to $TARGET_DIR"