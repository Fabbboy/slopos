#!/usr/bin/env bash
# Shared plumbing for the source-scanning discipline gates.
#
# Deliberately narrow: only what was already copied verbatim between them,
# plus the self-test harness. Each gate keeps its own exemption regexes and
# patterns — those are the thing under test and belong beside the test that
# pins them.
#
# Portability: bash 3.2 and stock BSD awk (macOS ships both). No `mapfile`,
# no `declare -A`, and no `\b` / `\s` in an ERE — awk reads `\b` as
# backspace and BSD grep reads `\s` as a literal `s`.

# ---------------------------------------------------------------------------
# Every `.rs` path under <root>, repo-relative, sorted, deduplicated. Tracked
# and untracked both: a gate that only saw committed files would pass on the
# change being made.
# ---------------------------------------------------------------------------
gate_collect_rs_files() {
    local root="$1"
    (
        cd "$root"
        {
            if command -v git >/dev/null 2>&1 && git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
                git ls-files '*.rs'
                git ls-files --others --exclude-standard '*.rs'
            else
                find . -type f -name '*.rs' \
                    -not -path './builddir/*' \
                    -not -path './third_party/*' \
                    -not -path './target/*'
            fi
            find vendor -type f -name '*.rs' 2>/dev/null || true
        } | sed 's|^\./||' | LC_ALL=C sort -u
    )
}

# ---------------------------------------------------------------------------
# The shared fail-open of every scan gate: an empty file list produces an
# empty offender list and a cheerful OK, with nothing to distinguish
# "scanned the tree and found nothing" from "scanned nothing".
# ---------------------------------------------------------------------------
gate_require_nonempty() {
    local gate="$1" root="$2" list="$3"
    if [ -z "$list" ]; then
        echo "$gate: no Rust sources found under $root — the scan would be a no-op," >&2
        echo "  so refusing to report OK. Check that this is the repository root and" >&2
        echo "  that git (or find) can see the tree." >&2
        exit 2
    fi
}

# ---------------------------------------------------------------------------
# Self-test harness. Both directions: exact hit counts on planted
# violations, and silence on the forms the gate deliberately accepts — a gate
# that cries wolf gets switched off just as surely as one that misses.
# ---------------------------------------------------------------------------

GATE_NAME=""
GATE_FIXTURE_ROOT=""
GATE_FINDINGS=""
GATE_SELF_TEST_FAIL=0

gate_selftest_begin() {
    GATE_NAME="$1"
    GATE_FIXTURE_ROOT="$(mktemp -d)"
    GATE_SELF_TEST_FAIL=0
    trap 'rm -rf "$GATE_FIXTURE_ROOT"' EXIT INT TERM
    echo "$GATE_NAME: self-test against built-in fixtures"
}

# Create the parent directory and echo the absolute path, for heredocs.
gate_fixture() {
    local rel="$1"
    mkdir -p "$GATE_FIXTURE_ROOT/$(dirname "$rel")"
    printf '%s\n' "$GATE_FIXTURE_ROOT/$rel"
}

# The scan must emit exactly <want> findings carrying <tag>.
gate_expect() {
    local tag="$1" want="$2" note="${3:-}" got
    got="$(printf '%s\n' "$GATE_FINDINGS" | grep -c "^$tag	" || true)"
    if [ "$got" -ne "$want" ]; then
        echo "$GATE_NAME --self-test: check $tag expected $want hit(s), got $got" >&2
        printf '%s\n' "$GATE_FINDINGS" | grep "^$tag	" | sed 's/^/      /' >&2
        GATE_SELF_TEST_FAIL=1
        return
    fi
    if [ -n "$note" ]; then
        echo "  check $tag: fires $got/$want as expected ($note)"
    else
        echo "  check $tag: fires $got/$want as expected"
    fi
}

# Nothing in the matching fixtures may fire, on any check.
gate_expect_silent() {
    local re="$1" description="$2" hits
    hits="$(printf '%s\n' "$GATE_FINDINGS" | grep -E "$re" || true)"
    if [ -n "$hits" ]; then
        echo "$GATE_NAME --self-test: false positive in $description:" >&2
        printf '%s\n' "$hits" | sed 's/^/      /' >&2
        GATE_SELF_TEST_FAIL=1
        return
    fi
    echo "  negatives: no false positives ($description)"
}

# The enumerator has two branches — `git ls-files` in a work tree, `find`
# outside one — and they can drift: find prunes builddir/third_party/target
# by hand while git relies on .gitignore. Nothing else checks they agree.
gate_expect_enumerator() {
    local root="$1" want="$2" got_find got_git
    got_find="$(gate_collect_rs_files "$root")"
    if [ "$got_find" != "$want" ]; then
        echo "$GATE_NAME --self-test: enumerator (find branch) returned the wrong set:" >&2
        diff <(printf '%s\n' "$want") <(printf '%s\n' "$got_find") | sed 's/^/      /' >&2
        GATE_SELF_TEST_FAIL=1
        return
    fi
    if ! command -v git >/dev/null 2>&1; then
        echo "  enumerator: find branch agrees ($(printf '%s\n' "$want" | grep -c . ) file(s); git not installed)"
        return
    fi
    (cd "$root" && git init -q . && git config user.email s@e && git config user.name s)
    got_git="$(gate_collect_rs_files "$root")"
    rm -rf "$root/.git"
    if [ "$got_git" != "$want" ]; then
        echo "$GATE_NAME --self-test: enumerator branches disagree:" >&2
        diff <(printf '%s\n' "$got_find") <(printf '%s\n' "$got_git") | sed 's/^/      /' >&2
        GATE_SELF_TEST_FAIL=1
        return
    fi
    echo "  enumerator: $(printf '%s\n' "$want" | grep -c .) file(s), git and find branches agree"
}

gate_selftest_end() {
    rm -rf "$GATE_FIXTURE_ROOT"
    trap - EXIT INT TERM
    if [ "$GATE_SELF_TEST_FAIL" -ne 0 ]; then
        echo "$GATE_NAME: SELF-TEST FAILED — the gate's patterns are wrong" >&2
        exit 1
    fi
    echo "$GATE_NAME: self-test OK"
    exit 0
}

# Rejects unknown arguments: a typo'd `--selftest` in the justfile would
# otherwise run the real scan and pass green forever.
GATE_SELF_TEST=0
gate_parse_args() {
    local gate="$1"
    shift
    case "${1:-}" in
        --self-test) GATE_SELF_TEST=1 ;;
        "") ;;
        *) echo "usage: $gate [--self-test]" >&2; exit 2 ;;
    esac
}
