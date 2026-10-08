#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

TARGET="${1:-}"
OUTPUT_DIR="${2:-${PLUGIN_ROOT}/dist}"

PLUGIN_ID=$(grep '^id = ' "${PLUGIN_ROOT}/plugin.toml" | head -n 1 | cut -d '"' -f 2)
PLUGIN_VERSION=$(grep '^version = ' "${PLUGIN_ROOT}/plugin.toml" | head -n 1 | cut -d '"' -f 2)

if [ -z "${PLUGIN_ID}" ] || [ -z "${PLUGIN_VERSION}" ]; then
  echo "Error: Failed to parse plugin ID or version from plugin.toml" >&2
  exit 1
fi

echo "Packaging plugin: ${PLUGIN_ID} v${PLUGIN_VERSION} (target: ${TARGET:-native})"

# Locate binary
if [ -n "${TARGET}" ] && [ -f "${PLUGIN_ROOT}/target/${TARGET}/release/voidb-plugin-${PLUGIN_ID}" ]; then
  BIN_PATH="${PLUGIN_ROOT}/target/${TARGET}/release/voidb-plugin-${PLUGIN_ID}"
  ARCH_SUFFIX="-${TARGET}"
elif [ -f "${PLUGIN_ROOT}/target/release/voidb-plugin-${PLUGIN_ID}" ]; then
  BIN_PATH="${PLUGIN_ROOT}/target/release/voidb-plugin-${PLUGIN_ID}"
  ARCH_SUFFIX="${TARGET:+-${TARGET}}"
elif [ -f "${PLUGIN_ROOT}/bin/voidb-plugin-${PLUGIN_ID}" ]; then
  BIN_PATH="${PLUGIN_ROOT}/bin/voidb-plugin-${PLUGIN_ID}"
  ARCH_SUFFIX="${TARGET:+-${TARGET}}"
elif [ -f "${PLUGIN_ROOT}/target/debug/voidb-plugin-${PLUGIN_ID}" ]; then
  BIN_PATH="${PLUGIN_ROOT}/target/debug/voidb-plugin-${PLUGIN_ID}"
  ARCH_SUFFIX="${TARGET:+-${TARGET}}"
else
  echo "Error: Plugin binary not found in target/${TARGET}/release, target/release, bin/, or target/debug/" >&2
  exit 1
fi

STAGE_DIR=$(mktemp -d)
trap 'rm -rf "${STAGE_DIR}"' EXIT

mkdir -p "${STAGE_DIR}/bin" "${OUTPUT_DIR}"
cp "${BIN_PATH}" "${STAGE_DIR}/bin/voidb-plugin-${PLUGIN_ID}"
chmod +x "${STAGE_DIR}/bin/voidb-plugin-${PLUGIN_ID}"
cp "${PLUGIN_ROOT}/plugin.toml" "${STAGE_DIR}/"

if [ -d "${PLUGIN_ROOT}/schemas" ]; then
  cp -r "${PLUGIN_ROOT}/schemas" "${STAGE_DIR}/"
fi

PKG_NAME="${PLUGIN_ID}-${PLUGIN_VERSION}${ARCH_SUFFIX}"

# 1. Package .tar (uncompressed, fully compatible with voidb-cli offline install)
tar -cf "${OUTPUT_DIR}/${PKG_NAME}.tar" -C "${STAGE_DIR}" bin plugin.toml $([ -d "${PLUGIN_ROOT}/schemas" ] && echo "schemas")

# 2. Package .tar.gz (compressed standard distribution package)
tar -czf "${OUTPUT_DIR}/${PKG_NAME}.tar.gz" -C "${STAGE_DIR}" bin plugin.toml $([ -d "${PLUGIN_ROOT}/schemas" ] && echo "schemas")

# 3. Package .tar.zst (zstd compressed format supported natively by voidb-cli)
tar --zstd -cf "${OUTPUT_DIR}/${PKG_NAME}.tar.zst" -C "${STAGE_DIR}" bin plugin.toml $([ -d "${PLUGIN_ROOT}/schemas" ] && echo "schemas") 2>/dev/null || true

echo "Packaged artifacts generated in ${OUTPUT_DIR}:"
ls -lh "${OUTPUT_DIR}/${PKG_NAME}"*
