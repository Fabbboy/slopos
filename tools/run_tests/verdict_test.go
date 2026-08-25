package main

import (
	"strings"
	"testing"
)

func intp(v int) *int { return &v }

// phase builds a PhaseRecord with the plan and pass count ClassifyRun reads.
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

func TestQemuNeverLaunchedIsHardError(t *testing.T) {
	s := summaryOf()
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

func TestQemuNeverLaunchedIsHardErrorEvenWithFilter(t *testing.T) {
	v := ClassifyRun(summaryOf(), DriverResult{QemuStatus: intp(0)}, true)
	if v.Code != 2 {
		t.Fatalf("no phases must fail even under a filter, got exit %d", v.Code)
	}
}

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

func TestUserAbortWinsOverNoPhases(t *testing.T) {
	v := ClassifyRun(summaryOf(), DriverResult{UserAborted: true}, false)
	if v.Code != 130 {
		t.Fatalf("want exit 130 for SIGINT, got %d", v.Code)
	}
	if v.Diagnostic != "" {
		t.Fatalf("SIGINT should not claim the harness never started, got %q", v.Diagnostic)
	}
}

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

func TestFailingRunWithQemuStatusOneStaysAtOne(t *testing.T) {
	p := phase(0, "kernel", 2, 1)
	p.Tests = append(p.Tests, &TestRecord{
		PhaseIdx: 0, Idx: 2, Name: "bad", Outcome: OutcomeFail,
	})
	if v := ClassifyRun(summaryOf(p), DriverResult{QemuStatus: intp(1)}, false); v.Code != 1 {
		t.Fatalf("want exit 1, got %d", v.Code)
	}
}

func TestNilQemuStatusIsSafe(t *testing.T) {
	s := summaryOf(phase(0, "kernel", 1, 1))
	if v := ClassifyRun(s, DriverResult{QemuStatus: nil}, false); v.Code != 0 {
		t.Fatalf("want exit 0 for a green run with unknown status, got %d", v.Code)
	}
}

func TestKernelAbortIsNotGreen(t *testing.T) {
	s := summaryOf(phase(1, "kernel", 3, 3), phase(2, "userland", 1, 1))
	s.KernelAbort = true
	s.KernelAbortReason = "NMI watchdog: CPU made no progress, sustained"
	v := ClassifyRun(s, DriverResult{QemuStatus: intp(0)}, false)
	if v.Code == 0 {
		t.Fatalf("an abort with zero failing tests must not exit 0")
	}
	if v.Code != 1 {
		t.Fatalf("want exit 1, got %d", v.Code)
	}
	if !strings.Contains(v.Diagnostic, "aborted on some CPU") {
		t.Fatalf("diagnostic must name the abort, got %q", v.Diagnostic)
	}
	if !strings.Contains(v.Diagnostic, "NMI watchdog") {
		t.Fatalf("diagnostic must carry the reason, got %q", v.Diagnostic)
	}
}

func TestKernelAbortWithNoReasonStillDiagnoses(t *testing.T) {
	s := summaryOf(phase(1, "kernel", 1, 1))
	s.KernelAbort = true
	v := ClassifyRun(s, DriverResult{QemuStatus: intp(0)}, false)
	if v.Code != 1 || v.Diagnostic == "" {
		t.Fatalf("want exit 1 with a diagnostic, got %d %q", v.Code, v.Diagnostic)
	}
	if !strings.Contains(v.Diagnostic, "no reason line") {
		t.Fatalf("diagnostic must say the reason never arrived, got %q", v.Diagnostic)
	}
}

// Ctrl-C still outranks the abort: a run the user killed is not evidence about
// the kernel, whatever the kernel printed on its way down.
func TestUserAbortStillOutranksKernelAbort(t *testing.T) {
	s := summaryOf(phase(1, "kernel", 1, 1))
	s.KernelAbort = true
	v := ClassifyRun(s, DriverResult{UserAborted: true}, false)
	if v.Code != 130 {
		t.Fatalf("want exit 130, got %d", v.Code)
	}
}

// The recorded case ended with QEMU killed by the wall guard, so the abort has
// to survive the unexpected-status path with its diagnostic intact.
func TestKernelAbortDiagnosticSurvivesUnexpectedQemuStatus(t *testing.T) {
	s := summaryOf(phase(1, "kernel", 1, 1))
	s.KernelAbort = true
	s.KernelAbortReason = "panic core abort"
	v := ClassifyRun(s, DriverResult{QemuStatus: intp(3)}, false)
	if v.Code != 2 {
		t.Fatalf("want exit 2 for an unexpected qemu status, got %d", v.Code)
	}
	if !strings.Contains(v.Diagnostic, "panic core abort") {
		t.Fatalf("diagnostic lost the reason, got %q", v.Diagnostic)
	}
}
