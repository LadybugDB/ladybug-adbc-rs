#!/usr/bin/env bash
# Download the prebuilt shared liblbug (LadybugDB core) into ./liblbug.
#
# The shared library (not the static archive) is required so that the
# dlopen'd algo extension can resolve ladybug symbols from the host
# process at LOAD EXTENSION time. With a static liblbug.a, dead-code
# stripping drops unreferenced symbols (e.g.
# lbug::function::TableFuncBindData) and the extension fails to load
# with "undefined symbol: _ZTIN4lbug8function17TableFuncBindDataE".
#
# Linux only — this crate does not ship macOS / Windows. Version
# defaults to the lbug crate in Cargo.toml (must match Cargo.lock).
#
# Mirrors ../bugscope/scripts/download-liblbug.sh with the macOS /
# Windows branches stripped.
set -euo pipefail

LIB_KIND="${LBUG_LIB_KIND:-shared}"
REPOSITORY="${LBUG_GITHUB_REPOSITORY:-LadybugDB/ladybug}"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${LBUG_TARGET_DIR:-$PROJECT_DIR/liblbug}"

if [ "$(uname -s)" != "Linux" ]; then
  echo "error: this crate only ships Linux liblbug prebuilts (got $(uname -s))" >&2
  exit 1
fi

if [ "$LIB_KIND" != "shared" ] && [ "$LIB_KIND" != "static" ]; then
  echo "Unsupported LBUG_LIB_KIND: $LIB_KIND (expected 'shared' or 'static')" >&2
  exit 1
fi

# Single source of truth is the lbug dependency in Cargo.toml.
LBUG_VERSION="${LBUG_VERSION:-$(grep -E '^lbug *=.*version *=' "$PROJECT_DIR/Cargo.toml" | head -n1 | grep -oE '"[^"]+"' | head -n1 | tr -d '"')}"
if [ -z "$LBUG_VERSION" ]; then
  echo "could not resolve lbug version from Cargo.toml" >&2
  exit 1
fi

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64) LINUX_ARCH=x86_64 ;;
  aarch64|arm64) LINUX_ARCH=aarch64 ;;
  *) echo "Unsupported Linux architecture: $ARCH" >&2; exit 1 ;;
esac

if [ "$LIB_KIND" = "static" ]; then
  ARCHIVE="liblbug-static-linux-${LINUX_ARCH}-compat.tar.gz"
  LIB_NAME="liblbug.a"
else
  ARCHIVE="liblbug-linux-${LINUX_ARCH}.tar.gz"
  LIB_NAME="liblbug.so"
fi

if [ -f "$TARGET_DIR/$LIB_NAME" ]; then
  echo "liblbug already exists in $TARGET_DIR"
  exit 0
fi

mkdir -p "$TARGET_DIR"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

DOWNLOAD_URL="https://github.com/${REPOSITORY}/releases/download/v${LBUG_VERSION}/${ARCHIVE}"
echo "Downloading $DOWNLOAD_URL ..."
curl -fSL "$DOWNLOAD_URL" -o "$TMPDIR/$ARCHIVE"

tar xzf "$TMPDIR/$ARCHIVE" -C "$TARGET_DIR"

if [ ! -f "$TARGET_DIR/$LIB_NAME" ]; then
  echo "Expected liblbug library not found at $TARGET_DIR/$LIB_NAME" >&2
  echo "Archive contents:" >&2
  ls "$TARGET_DIR" >&2
  exit 1
fi

echo "Installed $ARCHIVE (lbug $LBUG_VERSION) to $TARGET_DIR"