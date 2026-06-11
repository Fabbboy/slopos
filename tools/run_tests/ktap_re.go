// Package main — KTAP wire-format regexes and constants.
//
// Wire format documented in the public KTAP docs. The kernel emits each
// line with a literal `KTAP\t` prefix; everything below operates on the
// post-prefix tail. Patterns mirror the Python wrapper (Phase 4) byte-for-
// byte so the differential JSONL diff in Phase 5N comes out empty.

package main

import "regexp"

// KtapPrefix is the literal seven bytes the kernel prepends to every
// harness-emitted line: "KTAP" + ASCII tab (0x09).
const KtapPrefix = "KTAP\t"

// BailKey introduces a `Bail out!` line; matched after the prefix.
const BailKey = "Bail out!"

// MaxLogEmit caps the per-failure captured log slice the parser is
// expected to receive (kernel-side limit at ktap.rs:16). We don't enforce
// it; we just preserve it as a documented invariant.
const MaxLogEmit = 4096

// KlogTailLines is the rolling buffer size for non-KTAP klog lines that
// get attached to the next failure if the kernel hard-panics before its
// orderly `log: |` block can flush.
const KlogTailLines = 64

// Wire patterns. Each compiled once at package init.
var (
	// Plan line: "1..N".
	planRE = regexp.MustCompile(`^1\.\.(\d+)$`)

	// Top-level result: `(ok|not ok) N - <name>` with an optional `# <suffix>`.
	// `name` is non-whitespace (test fully-qualified `<module>::<name>`).
	topResultRE = regexp.MustCompile(`^(ok|not ok) (\d+) - (\S+)(?: # (.*))?$`)

	// Subtest result: 2-space indent + `(ok|not ok) M - <name>` + opt suffix.
	// Subtest names may contain spaces (userland-controlled), use lazy match.
	subtestRE = regexp.MustCompile(`^  (ok|not ok) (\d+) - (.+?)(?: # (.*))?$`)

	// Diagnostic field inside a `---`/`...` block: `  outcome:` / `  file:` /
	// `  log:`. The colon may or may not have a trailing space.
	diagFieldRE = regexp.MustCompile(`^  (outcome|file|log): ?(.*)$`)

	// Phase footer: kernel emits a single line summarising counters.
	footerRE = regexp.MustCompile(
		`^# elapsed_ms=(\d+) pass=(\d+) fail=(\d+) skip=(\d+) over_time=(\d+)$`,
	)

	// `time_ms=NNN` substring, anywhere in a result-line suffix.
	timeMsRE = regexp.MustCompile(`\btime_ms=(\d+)\b`)

	// `SKIP <reason>` suffix; reason is whatever comes after.
	skipRE = regexp.MustCompile(`^SKIP(?: (.*))?$`)

	// Defensive ANSI-escape stripper. Kernel doesn't emit colour today but
	// we strip any `ESC [ ... letter` sequence before parsing so a future
	// kernel-side colour change doesn't break the parser.
	ansiRE = regexp.MustCompile(`\x1b\[[0-9;?]*[A-Za-z]`)
)
