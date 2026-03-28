#!/usr/bin/env bash
set -euo pipefail

# Ensure the Limine bootloader is cloned and built.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

LIMINE_DIR="${LIMINE_DIR:-${REPO_ROOT}/third_party/limine}"
LIMINE_REPO="${LIMINE_REPO:-https://github.com/limine-bootloader/limine.git}"
LIMINE_BRANCH="${LIMINE_BRANCH:-v11.x-binary}"

if [ ! -f "$LIMINE_DIR/Makefile" ] && [ -f "$REPO_ROOT/.gitmodules" ]; then
    if command -v git >/dev/null 2>&1; then
        if git -C "$REPO_ROOT" config --file .gitmodules --get-regexp '^submodule\..*\.path$' \
            | grep -q 'third_party/limine'; then
            echo "Initializing Limine submodule..." >&2
            git -C "$REPO_ROOT" submodule update --init --depth=1 -- third_party/limine || true
        fi
    fi
fi

if [ ! -f "$LIMINE_DIR/Makefile" ]; then
    if [ -d "$LIMINE_DIR" ] && [ -n "$(ls -A "$LIMINE_DIR" 2>/dev/null)" ]; then
        echo "Limine directory exists but does not contain Limine sources: $LIMINE_DIR" >&2
        echo "Remove it or point LIMINE_DIR to a valid Limine checkout." >&2
        exit 1
    fi

    echo "Cloning Limine bootloader..." >&2
    mkdir -p "$(dirname "$LIMINE_DIR")"
    git clone --branch="$LIMINE_BRANCH" --depth=1 "$LIMINE_REPO" "$LIMINE_DIR"
fi

if [ ! -f "$LIMINE_DIR/limine-bios.sys" ] || [ ! -f "$LIMINE_DIR/BOOTX64.EFI" ]; then
    echo "Building Limine..." >&2
    make -C "$LIMINE_DIR" >/dev/null
fi
