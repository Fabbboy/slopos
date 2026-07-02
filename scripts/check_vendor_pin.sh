#!/usr/bin/env bash
# Verify pinned third-party code admitted into the kernel TCB.
#
# vendor/unwinding is a named TCB annex: it may contain executable unsafe,
# but only while its identity and content match this pin. Updating it must be
# an explicit review event that refreshes the version, upstream VCS SHA, file
# count, and deterministic tree hash below.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

ANNEX_REL="vendor/unwinding"
ANNEX_DIR="$REPO_ROOT/$ANNEX_REL"
MANIFEST="$ANNEX_DIR/Cargo.toml"
VCS_INFO="$ANNEX_DIR/.cargo_vcs_info.json"

EXPECTED_NAME="unwinding"
EXPECTED_VERSION="0.2.9"
EXPECTED_REPOSITORY="https://github.com/nbdd0121/unwinding/"
EXPECTED_UPSTREAM_SHA="72162ed9c5c9111efb912b9b7dc3007d5ef19105"
EXPECTED_FILE_COUNT="38"
EXPECTED_TREE_SHA256="f6dd8243426f53b1e6f6156cb1b2ee38086e93bff93c05278c839adea53b3118"

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        echo "check_vendor_pin: need sha256sum or shasum" >&2
        exit 2
    fi
}

sha256_stream() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 | awk '{ print $1 }'
    else
        echo "check_vendor_pin: need sha256sum or shasum" >&2
        exit 2
    fi
}

toml_package_value() {
    local key="$1"
    awk -v key="$key" '
        /^[[:space:]]*\[package\][[:space:]]*$/ { in_package = 1; next }
        /^[[:space:]]*\[/ {
            if (in_package) exit
            next
        }
        in_package {
            line = $0
            sub(/[[:space:]]*#.*/, "", line)
            if (line ~ "^[[:space:]]*" key "[[:space:]]*=") {
                sub("^[[:space:]]*" key "[[:space:]]*=[[:space:]]*", "", line)
                sub(/[[:space:]]*$/, "", line)
                gsub(/^"|"$/, "", line)
                print line
                exit
            }
        }
    ' "$MANIFEST"
}

json_sha1_value() {
    sed -n 's/.*"sha1"[[:space:]]*:[[:space:]]*"\([0-9a-fA-F]\{40\}\)".*/\1/p' "$VCS_INFO" | head -n 1
}

tree_file_list() {
    cd "$REPO_ROOT"
    find "$ANNEX_REL" -type f \
        -not -path "$ANNEX_REL/.git/*" \
        -not -path "$ANNEX_REL/target/*" \
        -print \
      | LC_ALL=C sort
}

tree_hash() {
    cd "$REPO_ROOT"
    tree_file_list | while IFS= read -r path; do
        printf '%s  %s\n' "$(sha256_file "$path")" "$path"
    done | sha256_stream
}

bad=0

if [ ! -d "$ANNEX_DIR" ]; then
    echo "check_vendor_pin: missing $ANNEX_REL" >&2
    exit 1
fi
if [ ! -f "$MANIFEST" ]; then
    echo "check_vendor_pin: missing $ANNEX_REL/Cargo.toml" >&2
    exit 1
fi
if [ ! -f "$VCS_INFO" ]; then
    echo "check_vendor_pin: missing $ANNEX_REL/.cargo_vcs_info.json" >&2
    exit 1
fi

actual_name="$(toml_package_value name)"
actual_version="$(toml_package_value version)"
actual_repository="$(toml_package_value repository)"
actual_upstream_sha="$(json_sha1_value)"
actual_file_count="$(tree_file_list | wc -l | tr -d '[:space:]')"
actual_tree_sha256="$(tree_hash)"

check_eq() {
    local label="$1"
    local expected="$2"
    local actual="$3"

    if [ "$actual" != "$expected" ]; then
        printf 'check_vendor_pin: %s mismatch\n' "$label" >&2
        printf '  expected: %s\n' "$expected" >&2
        printf '  actual:   %s\n' "$actual" >&2
        bad=1
    fi
}

check_eq "crate name" "$EXPECTED_NAME" "$actual_name"
check_eq "crate version" "$EXPECTED_VERSION" "$actual_version"
check_eq "repository" "$EXPECTED_REPOSITORY" "$actual_repository"
check_eq "upstream sha1" "$EXPECTED_UPSTREAM_SHA" "$actual_upstream_sha"
check_eq "file count" "$EXPECTED_FILE_COUNT" "$actual_file_count"
check_eq "content tree sha256" "$EXPECTED_TREE_SHA256" "$actual_tree_sha256"

if [ "$bad" -ne 0 ]; then
    echo "check_vendor_pin: FAIL — $ANNEX_REL no longer matches the reviewed TCB annex pin" >&2
    exit 1
fi

echo "check_vendor_pin: OK — $ANNEX_REL is pinned to $EXPECTED_NAME $EXPECTED_VERSION ($EXPECTED_UPSTREAM_SHA)"
