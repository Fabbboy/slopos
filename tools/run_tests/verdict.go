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
// The 1-versus-2 split is the load-bearing one. "Tests failed" and "no
// tests ran" are different facts and a human reading CI needs to tell them
// apart, but both must be non-zero, because the only thing automation
// reads is the exit code.
//
// # Why phase count is the evidence, not QEMU's exit status
//
// The wrapper cannot trust its child's status here. `isa-debug-exit`
// encodes the kernel's verdict as `(value << 1) | 1`, so a passing run
// leaves QEMU with status 1 — and 1 is also the status QEMU exits with
// when it fails to *start*, for instance when a stale instance still holds
// the write lock on the fs image. `scripts/qemu_run.sh` maps status 1 to
// "Tests passed" and exits 0, so a launch failure arrives here wearing a
// clean status and an empty stream. That is the defect this file exists to
// close, and no amount of exit-status inspection can close it.
//
// What the wrapper *can* trust is what it saw on the wire. The kernel
// harness emits `TAP version 14` per phase before it emits anything else,
// so a run that produced no phase at all did not reach the harness,
// whatever its child claimed.
//
// # Why a zero-length run is not the same as a zero-match filter
//
// Measured, not assumed: `just test 'zzz_no_such_test::*'` reports
//
//	0 tests across 2 phases
//
// because both phases still announce themselves and then plan `1..0`. The
// launch failure reported
//
//	0 tests across 0 phases
//
// So phase count separates the two cleanly. A filter that matches nothing
// stays green, which matters more than catching it would: a gate that
// fails a legitimate `just test 'nomatch::*'` teaches people to distrust
// it, and a distrusted gate is worse than an absent one.
//
// The zero-plan case is still caught when the caller passed no filter and
// no skip list, because then the kernel planning nothing means something
// is wrong with the build rather than with the request.
type RunVerdict struct {
	// Code is the process exit status.
	Code int
	// Diagnostic is a human-facing explanation for stderr, empty when the
	// run is green. It explains what the wrapper concluded and why, since
	// the exit code alone cannot.
	Diagnostic string
	// QemuStatusWarning is the pre-existing "unexpected qemu status"
	// note, kept separate so its wording does not change.
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

	// Every pre-existing non-zero path keeps its exact behaviour; the new
	// cases are checked only where the old code fell through to zero.
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

	// No phase header at all: the kernel's test harness never announced
	// itself, so nothing was executed no matter what the child reported.
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

	// Phases ran but planned nothing, and the caller asked for everything.
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
