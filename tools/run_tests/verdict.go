package main

import "fmt"

// Exit-code policy for a finished run, in one testable place.
//
// The codes are the wrapper's contract with CI:
//
//	0   every planned test ran and passed
//	1   the kernel ran tests and some failed, bailed, or timed out
//	2   the wrapper could not get a trustworthy answer — QEMU never
//	    launched, the kernel never reached the harness, or it reached the
//	    harness and planned nothing when the caller asked for everything
//	130 SIGINT
//
// QEMU's exit status is not evidence: `isa-debug-exit` encodes the kernel's
// verdict as `(value << 1) | 1`, so a passing run leaves QEMU with status 1 —
// also the status QEMU exits with when it fails to *start* — and
// `scripts/qemu_run.sh` maps status 1 to "Tests passed" and exits 0. Phase
// count is the evidence instead: the kernel harness emits `TAP version 14`
// per phase before anything else, so a run with no phase never reached it.
//
// A filter matching nothing is a different case and stays green: both phases
// still announce themselves and plan `1..0`, giving 0 tests across 2 phases
// where a launch failure gives 0 across 0. A zero plan is an error only when
// the caller passed no filter and no skip list.
type RunVerdict struct {
	// Code is the process exit status.
	Code int
	// Diagnostic is a human-facing explanation for stderr, empty when the
	// run is green.
	Diagnostic string
	// QemuStatusWarning is the "unexpected qemu status" note.
	QemuStatusWarning string
}

// ClassifyRun decides a finished run's exit status.
//
// `hasSelection` reports whether the caller narrowed the run with a filter
// or a skip list; a narrowed run is allowed to match nothing.
func ClassifyRun(s *RunSummary, d DriverResult, hasSelection bool) RunVerdict {
	failures := s.Failures()
	bailed := false
	for _, p := range s.Phases {
		if p.BailReason != nil {
			bailed = true
			break
		}
	}
	failedOverall := len(failures) > 0 || bailed || d.TimedOut || s.Truncated

	// Ctrl-C first: a run the user killed before the first phase is not
	// evidence of a broken harness.
	if d.UserAborted {
		return RunVerdict{Code: 130}
	}

	if failedOverall {
		v := RunVerdict{Code: 1}
		if d.QemuStatus != nil && *d.QemuStatus != 0 && *d.QemuStatus != 1 {
			v.QemuStatusWarning = fmt.Sprintf(
				"run_tests: warning: unexpected qemu_run.sh exit status %d "+
					"(kernel did not reach isa-debug-exit cleanly)", *d.QemuStatus)
			v.Code = 2
		}
		return v
	}

	if len(s.Phases) == 0 {
		return RunVerdict{
			Code: 2,
			Diagnostic: "run_tests: no test phases were seen — the kernel harness never started.\n" +
				"  Nothing was executed, so this run proves nothing and must not be read as a pass.\n" +
				"  Usual causes: QEMU failed to launch (a stale qemu-system-x86_64 still holding the\n" +
				"  write lock on the fs image is the common one — check `pgrep -a qemu-system`), the\n" +
				"  ISO did not boot, or the kernel panicked before reaching the test harness.",
		}
	}

	if s.PlannedTotal() == 0 && !hasSelection {
		return RunVerdict{
			Code: 2,
			Diagnostic: "run_tests: the kernel harness started but planned zero tests, and no filter\n" +
				"  or skip list was given. An unfiltered run is expected to plan thousands, so this\n" +
				"  is a broken build or a miswired cmdline rather than an empty selection.\n" +
				"  (A filter that matches nothing is legitimate and stays green — this is not that.)",
		}
	}

	v := RunVerdict{Code: 0}
	if d.QemuStatus != nil && *d.QemuStatus != 0 {
		v.QemuStatusWarning = fmt.Sprintf(
			"run_tests: warning: green run but qemu_run.sh exit status was %d; "+
				"treating as wrapper failure", *d.QemuStatus)
		v.Code = 2
	}
	return v
}
