#!/usr/bin/env bash
set -euo pipefail

# Ensure a Go toolchain (>= 1.22) is available for building
# `tools/run_tests/`. The wrapper depends on stdlib + `golang.org/x/term`
# only; any reasonably recent Go install works.
#
# We do NOT auto-install Go — host-side toolchains are user-managed.
# This script just gives an actionable error pointing at the standard
# install paths when Go is missing or too old.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

MIN_MAJOR=1
MIN_MINOR=22

if ! command -v go >/dev/null 2>&1; then
    cat >&2 <<'EOF'
ensure_go: `go` not found in PATH.

The SlopOS host-side test wrapper (`tools/run_tests/`) is written in Go
and requires a Go toolchain >= 1.22.

Install one of:

  • Linux (apt):   sudo apt-get install -y golang-go
  • Linux (other): https://go.dev/dl/  (single-file install to /usr/local/go)
  • macOS (brew):  brew install go
  • macOS (other): https://go.dev/dl/

Then re-run `just setup` (or `just _build-run-tests` directly).
EOF
    exit 1
fi

GO_VERSION="$(go version | awk '{print $3}' | sed 's/^go//')"
GO_MAJOR="$(echo "$GO_VERSION" | cut -d. -f1)"
GO_MINOR="$(echo "$GO_VERSION" | cut -d. -f2)"

if [ "$GO_MAJOR" -lt "$MIN_MAJOR" ] || \
   { [ "$GO_MAJOR" -eq "$MIN_MAJOR" ] && [ "$GO_MINOR" -lt "$MIN_MINOR" ]; }; then
    cat >&2 <<EOF
ensure_go: found go ${GO_VERSION}; need >= ${MIN_MAJOR}.${MIN_MINOR}.

Update Go to a recent release (https://go.dev/dl/ or your distro's
package manager) and re-run.
EOF
    exit 1
fi

# Pre-download module dependencies so the first `go build` is offline-friendly.
# Cheap when cache is warm.
( cd "${REPO_ROOT}/tools/run_tests" && go mod download ) || {
    echo "ensure_go: \`go mod download\` failed in tools/run_tests" >&2
    exit 1
}
