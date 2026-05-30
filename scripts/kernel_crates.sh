#!/usr/bin/env bash
# Resolve the set of kernel crates — the trusted core's denominator — from
# ground truth instead of a hand-maintained allowlist.
#
# Definition: a "kernel crate" is any workspace member that the `kernel`
# binary transitively links via *normal* dependencies. This is exactly the
# code that ends up in the kernel image, so it is the only honest
# denominator for the TCB ratio. Userland (`userland`, `slibc`, `appkit`,
# `slop-protocol`), test-only crates (`ktesting`, reached only via
# dev-deps), proc-macro/build tooling, and the `verification` proofs all
# fall out automatically because the kernel image does not normal-depend on
# them — there is no list to keep in sync.
#
# Output: one workspace-relative crate directory per line (e.g. `mm`,
# `sched`, `slopos-ostd`), sorted. The directory is derived from each
# package's manifest path, so it is robust to crate-name vs. dir-name skew
# (e.g. package `slopos-mm` lives in `mm/`).
#
# Requires `cargo metadata` + `jq`. Falls back to a clear error otherwise
# so a CI environment never silently miscounts.
#
# Usage:
#   scripts/kernel_crates.sh            # print kernel crate dirs
#   ROOT_PKG=kernel scripts/kernel_crates.sh   # override the root binary

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOT_PKG="${ROOT_PKG:-kernel}"

if ! command -v jq >/dev/null 2>&1; then
    echo "kernel_crates: jq is required" >&2
    exit 2
fi
if ! command -v cargo >/dev/null 2>&1; then
    echo "kernel_crates: cargo is required" >&2
    exit 2
fi

meta="$(cd "$REPO_ROOT" && cargo metadata --format-version 1 2>/dev/null)"

# Transitive closure of `kernel` over normal deps (kind == null), keeping
# only workspace members, then map each package id to its crate directory
# (the manifest's parent dir, relative to the workspace root).
printf '%s' "$meta" | jq -r --arg root "$ROOT_PKG" --arg wsroot "$REPO_ROOT" '
    # Workspace member package ids.
    (.workspace_members) as $members
    # id -> normal-dep ids, restricted to workspace members.
    | ( [ .resolve.nodes[]
          | { key: .id,
              value: [ .deps[]
                       | select( any(.dep_kinds[]; .kind == null) )
                       | .pkg ]
                     | map( select( . as $d | $members | index($d) ) ) }
        ] | from_entries ) as $adj
    # id -> crate directory (manifest parent dir, relative to workspace root).
    | ( [ .packages[]
          | select( .id as $id | $members | index($id) )
          | { key: .id,
              value: ( .manifest_path
                       | sub("/Cargo.toml$"; "")
                       | sub("^" + $wsroot + "/"; "") ) }
        ] | from_entries ) as $dir
    # Root package id.
    | ( .packages[] | select(.name == $root) | .id ) as $rootid
    # Iterative reachability fixpoint from the root over $adj.
    | { seen: {}, frontier: [ $rootid ] }
    | until( (.frontier | length) == 0;
        ( .frontier[0] ) as $cur
        | .frontier |= .[1:]
        | if (.seen[$cur] // false) then .
          else .seen[$cur] = true
               | .frontier += ( $adj[$cur] // [] )
          end )
    | .seen | keys[]
    | $dir[.]
' | sort -u
