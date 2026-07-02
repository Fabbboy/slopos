#!/usr/bin/env bash
# Verify pinned third-party code admitted into the kernel TCB.
#
# Each entry in ANNEX_PINS is a named TCB annex: it may contain executable
# unsafe, but only while its identity and content match its pin. Updating an
# annex must be an explicit review event that refreshes the version, upstream
# VCS SHA, file count, and deterministic tree hash in its table row.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# name|rel_dir|version|repository|upstream_sha1|file_count|tree_sha256
ANNEX_PINS=(
    "unwinding|vendor/unwinding|0.2.9|https://github.com/nbdd0121/unwinding/|72162ed9c5c9111efb912b9b7dc3007d5ef19105|38|f6dd8243426f53b1e6f6156cb1b2ee38086e93bff93c05278c839adea53b3118"
    "gimli|vendor/gimli|0.33.0|https://github.com/gimli-rs/gimli|033ef8dd5748236aa5bcecc868207a40e4e3f597|57|409a6a5ebf41fc015589e9b64576cb465161b78595eaa83ef5da9cf04d7c75cc"
)

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
    local manifest="$1"
    local key="$2"
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
    ' "$manifest"
}

json_sha1_value() {
    sed -n 's/.*"sha1"[[:space:]]*:[[:space:]]*"\([0-9a-fA-F]\{40\}\)".*/\1/p' "$1" | head -n 1
}

tree_file_list() {
    local annex_rel="$1"
    cd "$REPO_ROOT"
    find "$annex_rel" -type f \
        -not -path "$annex_rel/.git/*" \
        -not -path "$annex_rel/target/*" \
        -print \
      | LC_ALL=C sort
}

tree_hash() {
    local annex_rel="$1"
    cd "$REPO_ROOT"
    tree_file_list "$annex_rel" | while IFS= read -r path; do
        printf '%s  %s\n' "$(sha256_file "$path")" "$path"
    done | sha256_stream
}

bad=0

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

ok_names=""

for spec in "${ANNEX_PINS[@]}"; do
    IFS='|' read -r exp_name annex_rel exp_version exp_repository \
        exp_upstream_sha exp_file_count exp_tree_sha256 <<<"$spec"

    annex_dir="$REPO_ROOT/$annex_rel"
    manifest="$annex_dir/Cargo.toml"
    vcs_info="$annex_dir/.cargo_vcs_info.json"

    if [ ! -d "$annex_dir" ]; then
        echo "check_vendor_pin: missing $annex_rel" >&2
        exit 1
    fi
    if [ ! -f "$manifest" ]; then
        echo "check_vendor_pin: missing $annex_rel/Cargo.toml" >&2
        exit 1
    fi
    if [ ! -f "$vcs_info" ]; then
        echo "check_vendor_pin: missing $annex_rel/.cargo_vcs_info.json" >&2
        exit 1
    fi

    actual_name="$(toml_package_value "$manifest" name)"
    actual_version="$(toml_package_value "$manifest" version)"
    actual_repository="$(toml_package_value "$manifest" repository)"
    actual_upstream_sha="$(json_sha1_value "$vcs_info")"
    actual_file_count="$(tree_file_list "$annex_rel" | wc -l | tr -d '[:space:]')"
    actual_tree_sha256="$(tree_hash "$annex_rel")"

    check_eq "$annex_rel crate name" "$exp_name" "$actual_name"
    check_eq "$annex_rel crate version" "$exp_version" "$actual_version"
    check_eq "$annex_rel repository" "$exp_repository" "$actual_repository"
    check_eq "$annex_rel upstream sha1" "$exp_upstream_sha" "$actual_upstream_sha"
    check_eq "$annex_rel file count" "$exp_file_count" "$actual_file_count"
    check_eq "$annex_rel content tree sha256" "$exp_tree_sha256" "$actual_tree_sha256"

    ok_names="$ok_names $exp_name $exp_version ($exp_upstream_sha);"
done

if [ "$bad" -ne 0 ]; then
    echo "check_vendor_pin: FAIL — a vendored tree no longer matches its reviewed TCB annex pin" >&2
    exit 1
fi

echo "check_vendor_pin: OK —$ok_names"
