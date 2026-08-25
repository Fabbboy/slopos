// KTAP wire-format regexes and constants. The kernel emits each line with a
// literal `KTAP\t` prefix; everything below operates on the post-prefix tail.

package main

import "regexp"

// KtapPrefix is the literal prefix the kernel prepends to every
// harness-emitted line: "KTAP" + ASCII tab (0x09).
const KtapPrefix = "KTAP\t"

// BailKey introduces a `Bail out!` line; matched after the prefix.
const BailKey = "Bail out!"

// MaxLogEmit mirrors the kernel-side per-failure captured-log cap; it is
// documented here, not enforced.
const MaxLogEmit = 4096

// KlogTailLines is the rolling buffer of non-KTAP klog lines attached to the
// next failure when a hard panic pre-empts the orderly `log: |` block.
const KlogTailLines = 64

// KernelAbortBanner is the line `boot/src/panic.rs` and `slopos-ostd/src/panic.rs`
// both write to the polling early console immediately before the abort reason.
// It is plain klog, never KTAP-prefixed.
const KernelAbortBanner = "=== KERNEL ABORT ==="

var (
	planRE = regexp.MustCompile(`^1\.\.(\d+)$`)

	topResultRE = regexp.MustCompile(`^(ok|not ok) (\d+) - (\S+)(?: # (.*))?$`)

	// Subtest names may contain spaces (userland-controlled), use lazy match.
	subtestRE = regexp.MustCompile(`^  (ok|not ok) (\d+) - (.+?)(?: # (.*))?$`)

	// Diagnostic field inside a `---`/`...` block.
	diagFieldRE = regexp.MustCompile(`^  (outcome|file|log): ?(.*)$`)

	footerRE = regexp.MustCompile(
		`^# elapsed_ms=(\d+) pass=(\d+) fail=(\d+) skip=(\d+) over_time=(\d+)$`,
	)

	timeMsRE = regexp.MustCompile(`\btime_ms=(\d+)\b`)

	skipRE = regexp.MustCompile(`^SKIP(?: (.*))?$`)

	// The kernel emits no colour today; stripped anyway so a future
	// kernel-side colour change doesn't break the parser.
	ansiRE = regexp.MustCompile(`\x1b\[[0-9;?]*[A-Za-z]`)
)
