#!/usr/bin/env bash
set -euo pipefail

# Resolve repository root relative to this script
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OVMF_DIR="${REPO_ROOT}/third_party/ovmf"
OVMF_CODE="${OVMF_DIR}/OVMF_CODE.fd"
OVMF_VARS="${OVMF_DIR}/OVMF_VARS.fd"
SYSTEM_OVMF_DIR="/usr/share/OVMF"

# Pinned to a commit rather than `master`, which rebuilds most days, so CI and a
# laptop boot identical firmware.
OVMF_COMMIT="${OVMF_COMMIT:-4dfb14f6bcb9}"
OVMF_BASE_URL="${OVMF_BASE_URL:-https://raw.githubusercontent.com/retrage/edk2-nightly/${OVMF_COMMIT}/bin}"
OVMF_CODE_SHA256="9ac67a5e7c8404754042c435d829a13c938b6093622e74bbdfa43f5bfd4677c9"
OVMF_VARS_SHA256="5d2ac383371b408398accee7ec27c8c09ea5b74a0de0ceea6513388b15be5d1e"

mkdir -p "${OVMF_DIR}"

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    echo "No SHA-256 tool found (need sha256sum or shasum)" >&2
    exit 1
  fi
}

copy_system_firmware() {
  local candidate="$1"
  local dest="$2"

  if [ -f "${SYSTEM_OVMF_DIR}/${candidate}" ]; then
    echo "Copying ${candidate} from system OVMF install" >&2
    cp "${SYSTEM_OVMF_DIR}/${candidate}" "${dest}"
    return 0
  fi

  return 1
}

download_firmware() {
  local url="$1"
  local dest="$2"
  local want="$3"

  if [ -f "${dest}" ]; then
    local have
    have="$(sha256_of "${dest}")"
    if [ "${have}" = "${want}" ]; then
      echo "OVMF artifact already present: ${dest}" >&2
      return
    fi
    # ci.yml caches third_party/ovmf, so without this a stale copy is served
    # forever.
    echo "Refetching $(basename "${dest}"): checksum does not match the pin" >&2
    rm -f "${dest}"
  fi

  echo "Downloading $(basename "${dest}") from ${url}" >&2
  curl -L --fail --progress-bar "${url}" -o "${dest}.tmp"

  local got
  got="$(sha256_of "${dest}.tmp")"
  if [ "${got}" != "${want}" ]; then
    rm -f "${dest}.tmp"
    echo "OVMF checksum mismatch for $(basename "${dest}")" >&2
    echo "  expected: ${want}" >&2
    echo "  actual:   ${got}" >&2
    exit 1
  fi
  mv "${dest}.tmp" "${dest}"
}

# Distro firmware is a starting point, not an override: it is consulted only
# when nothing is cached, and still has to match the pin below.
if [ ! -f "${OVMF_CODE}" ]; then
  copy_system_firmware "OVMF_CODE.fd" "${OVMF_CODE}" ||
    copy_system_firmware "OVMF_CODE_4M.fd" "${OVMF_CODE}" || true
fi
if [ ! -f "${OVMF_VARS}" ]; then
  copy_system_firmware "OVMF_VARS.fd" "${OVMF_VARS}" ||
    copy_system_firmware "OVMF_VARS_4M.fd" "${OVMF_VARS}" || true
fi

download_firmware "${OVMF_BASE_URL}/RELEASEX64_OVMF_CODE.fd" "${OVMF_CODE}" "${OVMF_CODE_SHA256}"
download_firmware "${OVMF_BASE_URL}/RELEASEX64_OVMF_VARS.fd" "${OVMF_VARS}" "${OVMF_VARS_SHA256}"

echo "OVMF firmware ready in ${OVMF_DIR}" >&2
