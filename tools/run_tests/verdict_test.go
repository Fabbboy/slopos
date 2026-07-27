package main

import (
	"strings"
	"testing"
)

// intp is a small helper for the *int QemuStatus field.
func intp(v int) *int { return &v }

// phase builds a PhaseRecord with a plan and a given number of passing
// tests, which is all ClassifyRun looks at.
func phase(idx int, name string, planN int, passing int) *PhaseRecord {
	p := &PhaseRecord{Idx: idx, Name: name}
	n := planN
	p.PlanN = &n
	for i := 0; i < passing; i++ {
		p.Tests = append(p.Tests, &TestRecord{
			PhaseIdx: idx, Idx: i + 1, Name: "t", Outcome: OutcomePass,
		})
	}
	p.Counters.Pass = passing
	return p
}

func summaryOf(phases ...*PhaseRecord) *RunSummary {
	s := &RunSummary{phaseByIdx: make(map[int]*PhaseRecord)}
	for _, p := range phases {
		s.Phases = append(s.Phases, p)
		s.phaseByIdx[p.Idx] = p
	}
	return s
}

// The regression this file exists for. A stale QEMU holding the write lock
// on the fs image makes QEMU exit 1 without producing a single byte of
// KTAP; qemu_run.sh reads status 1 as "tests passed" and exits 0. The
// wrapper used to agree and exit 0 having run nothing.
func TestQemuNeverLaunchedIsHardError(t *testing.T) {
	s := summaryOf() // no phases at all — nothing was ever announced
	v := ClassifyRun(s, DriverResult{QemuStatus: intp(0)}, false)
	if v.Code == 0 {
		t.Fatalf("a run that produced no phases must not exit 0")
	}
	if v.Code != 2 {
		t.Fatalf("want exit 2 (wrapper could not get an answer), got %d", v.Code)
	}
	if !strings.Contains(v.Diagnostic, "never started") {
		t.Fatalf("diagnostic should say the harness never started, got %q", v.Diagnostic)
	}
}

// The same thing with a filter set. A filter explains zero *tests*; it
// never explains zero *phases*, so this must still fail.
func TestQemuNeverLaunchedIsHardErrorEvenWithFilter(t *testing.T) {
	v := ClassifyRun(summaryOf(), DriverResult{QemuStatus: intp(0)}, true)
	if v.Code != 2 {
		t.Fatalf("no phases must fail even under a filter, got exit %d", v.Code)
	}
}

// The case that must stay green, and the reason the predicate keys on
// phase count rather than test count. Measured against the real harness:
// `just test 'zzz_no_such_test::*'` prints "0 tests across 2 phases",
// because both phases announce themselves and then plan 1..0.
func TestZeroMatchFilterStaysGreen(t *testing.T) {
	s := summaryOf(
		phase(0, "kernel", 0, 0),
		phase(1, "userland", 0, 0),
	)
	v := ClassifyRun(s, DriverResult{QemuStatus: intp(0)}, true)
	if v.Code != 0 {
		t.Fatalf("a filter matching nothing is legitimate; want exit 0, got %d (%s)",
			v.Code, v.Diagnostic)
	}
}

// Phases ran, planned nothing, and the caller asked for everything. That
// is a broken build rather than an empty request.
func TestZeroPlanWithoutFilterFails(t *testing.T) {
	s := summaryOf(
		phase(0, "kernel", 0, 0),
		phase(1, "userland", 0, 0),
	)
	v := ClassifyRun(s, DriverResult{QemuStatus: intp(0)}, false)
	if v.Code != 2 {
		t.Fatalf("zero planned tests with no filter must fail, got exit %d", v.Code)
	}
	if !strings.Contains(v.Diagnostic, "planned zero tests") {
		t.Fatalf("diagnostic should name the zero plan, got %q", v.Diagnostic)
	}
}

// Truncation: the kernel planned more than it delivered and did not bail.
// "0 failed" is true and misleading, so the exit code must not be 0.
func TestTruncatedRunFails(t *testing.T) {
	s := summaryOf(phase(0, "kernel", 2666, 2562))
	s.Truncated = true
	v := ClassifyRun(s, DriverResult{QemuStatus: intp(0)}, false)
	if v.Code != 1 {
		t.Fatalf("a truncated run must exit 1, got %d", v.Code)
	}
	if len(s.Failures()) != 0 {
		t.Fatalf("fixture sanity: this run has no failing tests, only truncation")
	}
}

// A truncated run is still a failure when the caller filtered, and the
// filter must not launder it into the legitimate-empty path.
func TestTruncatedRunFailsUnderFilter(t *testing.T) {
	s := summaryOf(phase(0, "kernel", 10, 4))
	s.Truncated = true
	if v := ClassifyRun(s, DriverResult{QemuStatus: intp(0)}, true); v.Code != 1 {
		t.Fatalf("truncation under a filter must exit 1, got %d", v.Code)
	}
}

func TestGreenRunExitsZero(t *testing.T) {
	s := summaryOf(phase(0, "kernel", 3, 3), phase(1, "userland", 1, 1))
	if v := ClassifyRun(s, DriverResult{QemuStatus: intp(0)}, false); v.Code != 0 {
		t.Fatalf("want exit 0 for a full green run, got %d (%s)", v.Code, v.Diagnostic)
	}
}

func TestFailingTestExitsOne(t *testing.T) {
	p := phase(0, "kernel", 2, 1)
	p.Tests = append(p.Tests, &TestRecord{
		PhaseIdx: 0, Idx: 2, Name: "bad", Outcome: OutcomeFail,
	})
	p.Counters.Fail = 1
	if v := ClassifyRun(summaryOf(p), DriverResult{QemuStatus: intp(0)}, false); v.Code != 1 {
		t.Fatalf("want exit 1 for a failing test, got %d", v.Code)
	}
}

func TestBailExitsOne(t *testing.T) {
	p := phase(0, "kernel", 5, 2)
	reason := "kernel panic"
	p.BailReason = &reason
	if v := ClassifyRun(summaryOf(p), DriverResult{QemuStatus: intp(0)}, false); v.Code != 1 {
		t.Fatalf("want exit 1 for a bail, got %d", v.Code)
	}
}

func TestTimeoutExitsOne(t *testing.T) {
	s := summaryOf(phase(0, "kernel", 5, 2))
	if v := ClassifyRun(s, DriverResult{TimedOut: true, QemuStatus: intp(0)}, false); v.Code != 1 {
		t.Fatalf("want exit 1 for a timeout, got %d", v.Code)
	}
}

// Ctrl-C outranks the no-phases check: a run killed before the first phase
// is the user's doing, not a broken harness.
func TestUserAbortWinsOverNoPhases(t *testing.T) {
	v := ClassifyRun(summaryOf(), DriverResult{UserAborted: true}, false)
	if v.Code != 130 {
		t.Fatalf("want exit 130 for SIGINT, got %d", v.Code)
	}
	if v.Diagnostic != "" {
		t.Fatalf("SIGINT should not claim the harness never started, got %q", v.Diagnostic)
	}
}

// Pre-existing behaviour that must not drift: a green run whose child
// reported a non-zero status is a wrapper failure, and a failing run whose
// child reported something other than 0 or 1 escalates from 1 to 2.
func TestGreenRunWithDirtyQemuStatusEscalates(t *testing.T) {
	s := summaryOf(phase(0, "kernel", 1, 1))
	v := ClassifyRun(s, DriverResult{QemuStatus: intp(3)}, false)
	if v.Code != 2 {
		t.Fatalf("want exit 2, got %d", v.Code)
	}
	if !strings.Contains(v.QemuStatusWarning, "green run but qemu_run.sh exit status was 3") {
		t.Fatalf("warning wording drifted: %q", v.QemuStatusWarning)
	}
}

func TestFailingRunWithUnexpectedQemuStatusEscalates(t *testing.T) {
	p := phase(0, "kernel", 2, 1)
	p.Tests = append(p.Tests, &TestRecord{
		PhaseIdx: 0, Idx: 2, Name: "bad", Outcome: OutcomeFail,
	})
	v := ClassifyRun(summaryOf(p), DriverResult{QemuStatus: intp(9)}, false)
	if v.Code != 2 {
		t.Fatalf("want exit 2, got %d", v.Code)
	}
	if !strings.Contains(v.QemuStatusWarning, "did not reach isa-debug-exit cleanly") {
		t.Fatalf("warning wording drifted: %q", v.QemuStatusWarning)
	}
}

// A failing run whose child exited 1 is the normal shape (isa-debug-exit
// encodes the kernel verdict), so it must stay at 1 rather than escalating.
func TestFailingRunWithQemuStatusOneStaysAtOne(t *testing.T) {
	p := phase(0, "kernel", 2, 1)
	p.Tests = append(p.Tests, &TestRecord{
		PhaseIdx: 0, Idx: 2, Name: "bad", Outcome: OutcomeFail,
	})
	if v := ClassifyRun(summaryOf(p), DriverResult{QemuStatus: intp(1)}, false); v.Code != 1 {
		t.Fatalf("want exit 1, got %d", v.Code)
	}
}

// A nil status (child never finished cleanly) must not be dereferenced.
func TestNilQemuStatusIsSafe(t *testing.T) {
	s := summaryOf(phase(0, "kernel", 1, 1))
	if v := ClassifyRun(s, DriverResult{QemuStatus: nil}, false); v.Code != 0 {
		t.Fatalf("want exit 0 for a green run with unknown status, got %d", v.Code)
	}
}
