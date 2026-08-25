// Tests for KtapParser, the leading-garbage rescue, and RunRecorder
// aggregation.

package main

import (
	"strings"
	"testing"
)

func feedAll(p *KtapParser, lines []string) []Event {
	var out []Event
	for _, ln := range lines {
		out = append(out, p.Feed(ln)...)
	}
	return out
}

func filterTests(events []Event) []*EvTest {
	var out []*EvTest
	for _, e := range events {
		if t, ok := e.(*EvTest); ok {
			out = append(out, t)
		}
	}
	return out
}

func filterPhaseStarts(events []Event) []*EvPhaseStart {
	var out []*EvPhaseStart
	for _, e := range events {
		if p, ok := e.(*EvPhaseStart); ok {
			out = append(out, p)
		}
	}
	return out
}

func filterPlans(events []Event) []*EvPlan {
	var out []*EvPlan
	for _, e := range events {
		if p, ok := e.(*EvPlan); ok {
			out = append(out, p)
		}
	}
	return out
}

func filterPhaseEnds(events []Event) []*EvPhaseEnd {
	var out []*EvPhaseEnd
	for _, e := range events {
		if p, ok := e.(*EvPhaseEnd); ok {
			out = append(out, p)
		}
	}
	return out
}

func filterBails(events []Event) []*EvBail {
	var out []*EvBail
	for _, e := range events {
		if b, ok := e.(*EvBail); ok {
			out = append(out, b)
		}
	}
	return out
}

func TestKtapMinimalGreenRun(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..3",
		"KTAP\tok 1 - mod::a # time_ms=1",
		"KTAP\tok 2 - mod::b # time_ms=2",
		"KTAP\tok 3 - mod::c # time_ms=3",
		"KTAP\t# elapsed_ms=10 pass=3 fail=0 skip=0 over_time=0",
	})
	starts := filterPhaseStarts(events)
	plans := filterPlans(events)
	tests := filterTests(events)
	ends := filterPhaseEnds(events)

	if len(starts) != 1 || starts[0].Name != "kernel" {
		t.Fatalf("phase starts: got %v, want 1 kernel", starts)
	}
	if len(plans) != 1 || plans[0].N != 3 {
		t.Fatalf("plans: got %v, want 1 with N=3", plans)
	}
	if len(tests) != 3 {
		t.Fatalf("tests: got %d, want 3", len(tests))
	}
	wantNames := []string{"mod::a", "mod::b", "mod::c"}
	for i, tt := range tests {
		if tt.Record.Name != wantNames[i] {
			t.Errorf("test %d: name = %q, want %q", i, tt.Record.Name, wantNames[i])
		}
		if tt.Record.Outcome != OutcomePass {
			t.Errorf("test %d: outcome = %s, want pass", i, tt.Record.Outcome)
		}
	}
	if len(ends) != 1 || ends[0].Pass != 3 || ends[0].Fail != 0 {
		t.Fatalf("phase end: got %+v, want pass=3 fail=0", ends[0])
	}
}

func TestKtapSingleFailWithDiagAndLog(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..1",
		"KTAP\tnot ok 1 - mod::bad # time_ms=11",
		"KTAP\t  ---",
		"KTAP\t  outcome: Fail",
		"KTAP\t  file: core/src/sched_tests.rs:1832",
		"KTAP\t  log: |",
		"KTAP\t   SCHED: priority bump observed",
		"KTAP\t   ASSERT_EQ: expected 5, got 9",
		"KTAP\t  ...",
		"KTAP\t# elapsed_ms=11 pass=0 fail=1 skip=0 over_time=0",
	})
	tests := filterTests(events)
	if len(tests) != 1 {
		t.Fatalf("tests: got %d, want 1", len(tests))
	}
	rec := tests[0].Record
	if rec.Outcome != OutcomeFail {
		t.Errorf("outcome = %s, want fail", rec.Outcome)
	}
	if rec.TimeMs == nil || *rec.TimeMs != 11 {
		t.Errorf("time_ms: got %v, want 11", rec.TimeMs)
	}
	if rec.FailOutcomeKind == nil || *rec.FailOutcomeKind != "Fail" {
		t.Errorf("fail_outcome_kind: got %v, want Fail", rec.FailOutcomeKind)
	}
	if rec.FailFile == nil || *rec.FailFile != "core/src/sched_tests.rs:1832" {
		t.Errorf("fail_file: got %v", rec.FailFile)
	}
	if rec.Log == nil || !strings.Contains(*rec.Log, "priority bump") {
		t.Errorf("log missing priority bump line: %v", rec.Log)
	}
	if rec.Log == nil || !strings.Contains(*rec.Log, "ASSERT_EQ: expected 5, got 9") {
		t.Errorf("log missing assert: %v", rec.Log)
	}
}

func TestKtapFailWithTruncationMarkers(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..1",
		"KTAP\tnot ok 1 - mod::big # time_ms=5",
		"KTAP\t  ---",
		"KTAP\t  outcome: Fail",
		"KTAP\t  file: foo.rs:1",
		"KTAP\t  log: |",
		"KTAP\t   [head trimmed: 4096 bytes]",
		"KTAP\t   tail content here",
		"KTAP\t   [tail trimmed: 200 bytes lost to ring overflow]",
		"KTAP\t  ...",
		"KTAP\t# elapsed_ms=5 pass=0 fail=1 skip=0 over_time=0",
	})
	tests := filterTests(events)
	if len(tests) != 1 || tests[0].Record.Log == nil {
		t.Fatalf("expected 1 fail with log, got %v", tests)
	}
	log := *tests[0].Record.Log
	if !strings.Contains(log, "[head trimmed: 4096 bytes]") {
		t.Errorf("log missing head trim: %q", log)
	}
	if !strings.Contains(log, "[tail trimmed: 200 bytes lost to ring overflow]") {
		t.Errorf("log missing tail trim: %q", log)
	}
}

func TestKtapSubtestsPrecedeParent(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..1",
		"KTAP\t  ok 1 - alloc_basic",
		"KTAP\t  ok 2 - alloc_huge",
		"KTAP\t  not ok 3 - alloc_zero # zero-size returned null",
		"KTAP\tnot ok 1 - utest_heap # time_ms=234",
		"KTAP\t  ---",
		"KTAP\t  outcome: Fail",
		"KTAP\t  file: core/src/exec/utest.rs:123",
		"KTAP\t  log: |",
		"KTAP\t   utest reported 1 of 3 subtests failed",
		"KTAP\t  ...",
		"KTAP\t# elapsed_ms=234 pass=0 fail=1 skip=0 over_time=0",
	})
	tests := filterTests(events)
	if len(tests) != 1 {
		t.Fatalf("expected 1 test, got %d", len(tests))
	}
	rec := tests[0].Record
	if rec.Name != "utest_heap" {
		t.Errorf("name: got %q want utest_heap", rec.Name)
	}
	if len(rec.Subtests) != 3 {
		t.Fatalf("subtests: got %d, want 3", len(rec.Subtests))
	}
	if rec.Subtests[0].Name != "alloc_basic" || rec.Subtests[0].Outcome != OutcomePass {
		t.Errorf("subtest 0: %+v", rec.Subtests[0])
	}
	if rec.Subtests[2].Name != "alloc_zero" || rec.Subtests[2].Outcome != OutcomeFail {
		t.Errorf("subtest 2: %+v", rec.Subtests[2])
	}
	if rec.Subtests[2].Msg != "zero-size returned null" {
		t.Errorf("subtest 2 msg: got %q", rec.Subtests[2].Msg)
	}
}

func TestKtapPassWithVerboseLogBlockNoOutcomeField(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..1",
		"KTAP\tok 1 - mod::ok # time_ms=2",
		"KTAP\t  ---",
		"KTAP\t  log: |",
		"KTAP\t   trace line 1",
		"KTAP\t   trace line 2",
		"KTAP\t  ...",
		"KTAP\t# elapsed_ms=2 pass=1 fail=0 skip=0 over_time=0",
	})
	tests := filterTests(events)
	if len(tests) != 1 {
		t.Fatalf("expected 1 test, got %d", len(tests))
	}
	rec := tests[0].Record
	if rec.Outcome != OutcomePass {
		t.Errorf("outcome = %s, want pass", rec.Outcome)
	}
	if rec.Log == nil || *rec.Log != "trace line 1\ntrace line 2" {
		t.Errorf("log: got %v", rec.Log)
	}
	if rec.FailOutcomeKind != nil {
		t.Errorf("fail_outcome_kind should be nil, got %v", rec.FailOutcomeKind)
	}
	if rec.FailFile != nil {
		t.Errorf("fail_file should be nil, got %v", rec.FailFile)
	}
}

func TestKtapOverTimeSuffix(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..1",
		"KTAP\tok 1 - mod::slow # time_ms=7321 OVER_TIME",
		"KTAP\t# elapsed_ms=7321 pass=1 fail=0 skip=0 over_time=1",
	})
	tests := filterTests(events)
	if len(tests) != 1 {
		t.Fatalf("expected 1 test, got %d", len(tests))
	}
	rec := tests[0].Record
	if rec.Outcome != OutcomePass || !rec.OverTime || rec.TimeMs == nil || *rec.TimeMs != 7321 {
		t.Errorf("got %+v, want pass + over_time + time_ms=7321", rec)
	}
}

func TestKtapExpectedPanicSuffix(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..1",
		"KTAP\tok 1 - mod::bootstrap_panic_isolation # time_ms=3 EXPECTED_PANIC",
		"KTAP\t# elapsed_ms=3 pass=1 fail=0 skip=0 over_time=0",
	})
	tests := filterTests(events)
	if len(tests) != 1 || !tests[0].Record.ExpectedPanic {
		t.Fatalf("expected_panic missing")
	}
}

func TestKtapSkipWithReason(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..1",
		"KTAP\tok 1 - mod::skipme # SKIP test returned Skipped",
		"KTAP\t# elapsed_ms=1 pass=1 fail=0 skip=0 over_time=0",
	})
	tests := filterTests(events)
	if len(tests) != 1 {
		t.Fatalf("expected 1 test, got %d", len(tests))
	}
	rec := tests[0].Record
	if rec.Outcome != OutcomeSkip || rec.SkipReason == nil || *rec.SkipReason != "test returned Skipped" {
		t.Errorf("got %+v", rec)
	}
}

func TestKtapKernelThenUserland(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..1",
		"KTAP\tok 1 - mod::a # time_ms=1",
		"KTAP\t# elapsed_ms=1 pass=1 fail=0 skip=0 over_time=0",
		"KTAP\tTAP version 14",
		"KTAP\t1..1",
		"KTAP\t  ok 1 - inner",
		"KTAP\tok 1 - utest_fork # time_ms=120",
		"KTAP\t# elapsed_ms=120 pass=1 fail=0 skip=0 over_time=0",
	})
	starts := filterPhaseStarts(events)
	if len(starts) != 2 || starts[0].Name != "kernel" || starts[1].Name != "userland" {
		t.Fatalf("phase order wrong: %+v", starts)
	}
	ends := filterPhaseEnds(events)
	if len(ends) != 2 {
		t.Fatalf("expected 2 phase ends, got %d", len(ends))
	}
	tests := filterTests(events)
	if len(tests) != 2 {
		t.Fatalf("expected 2 tests, got %d", len(tests))
	}
	if tests[1].Record.PhaseName != "userland" {
		t.Errorf("phase 2 test phase name = %q", tests[1].Record.PhaseName)
	}
	if len(tests[1].Record.Subtests) != 1 {
		t.Errorf("expected 1 subtest, got %d", len(tests[1].Record.Subtests))
	}
}

func TestKtapBailMidStream(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..3",
		"KTAP\tnot ok 1 - bootstrap_glob_match # time_ms=1",
		"KTAP\t  ---",
		"KTAP\t  outcome: Fail",
		"KTAP\t  file: bootstrap.rs:5",
		"KTAP\t  log: |",
		"KTAP\t   regex broken",
		"KTAP\t  ...",
		"KTAP\tBail out! bootstrap_glob_match",
	})
	bails := filterBails(events)
	if len(bails) != 1 || bails[0].Reason != "bootstrap_glob_match" || bails[0].PhaseIdx != 1 {
		t.Errorf("bail: got %+v", bails)
	}
}

func TestKtapTruncatedStream(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..3",
		"KTAP\tok 1 - mod::a # time_ms=1",
		"KTAP\tok 2 - mod::b # time_ms=2",
	})
	rec := NewRecorder()
	for _, ev := range events {
		rec.Record(ev)
	}
	rec.Finalize(nil)
	if !rec.Summary.Truncated {
		t.Errorf("expected Truncated=true, got false")
	}
	// mod::b is still pending — a footer would have flushed it — so only
	// mod::a is observable.
	if len(rec.Summary.Phases[0].Tests) != 1 {
		t.Errorf("expected 1 emitted test, got %d", len(rec.Summary.Phases[0].Tests))
	}
}

func TestKtapKlogTailAttachedToPanicFailureWithEmptyLog(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..1",
		"[boot] interesting klog line A",
		"[boot] interesting klog line B",
		"KTAP\tnot ok 1 - mod::panicker # time_ms=4",
		"KTAP\t  ---",
		"KTAP\t  outcome: Panic",
		"KTAP\t  file: x.rs:1",
		"KTAP\t  log: |",
		"KTAP\t  ...",
		"KTAP\t# elapsed_ms=4 pass=0 fail=1 skip=0 over_time=0",
	})
	tests := filterTests(events)
	if len(tests) != 1 {
		t.Fatalf("expected 1 test, got %d", len(tests))
	}
	rec := tests[0].Record
	if rec.Outcome != OutcomeFail {
		t.Errorf("outcome: got %s, want fail", rec.Outcome)
	}
	if rec.Log == nil || *rec.Log != "" {
		t.Errorf("log: got %v, want empty string", rec.Log)
	}
	joined := strings.Join(rec.PreFailKlogTail, "\n")
	if !strings.Contains(joined, "interesting klog line A") {
		t.Errorf("klog tail missing line A: %v", rec.PreFailKlogTail)
	}
	if !strings.Contains(joined, "interesting klog line B") {
		t.Errorf("klog tail missing line B: %v", rec.PreFailKlogTail)
	}
}

func TestKtapStripAnsiPassesThroughToParser(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"\x1b[32mKTAP\tTAP version 14\x1b[0m",
		"\x1b[32mKTAP\t1..1\x1b[0m",
		"\x1b[32mKTAP\tok 1 - mod::ok # time_ms=1\x1b[0m",
		"\x1b[32mKTAP\t# elapsed_ms=1 pass=1 fail=0 skip=0 over_time=0\x1b[0m",
	})
	tests := filterTests(events)
	if len(tests) != 1 || tests[0].Record.Name != "mod::ok" {
		t.Errorf("got %+v", tests)
	}
}

func TestStripANSIIdempotent(t *testing.T) {
	in := "hello \x1b[31mworld\x1b[0m"
	want := "hello world"
	if got := stripANSI(in); got != want {
		t.Errorf("stripANSI: got %q, want %q", got, want)
	}
	if got := stripANSI(stripANSI(in)); got != want {
		t.Errorf("stripANSI idempotent: got %q, want %q", got, want)
	}
}

func TestKtapLeadingGarbageBeforeKtapPrefixIsRecognized(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..3",
		"KTAP\tok 1 - mod::clean # time_ms=0",
		"helloKTAP\tok 2 - mod::with_garbage_prefix # time_ms=0",
		"\x11\x13KTAP\tok 3 - mod::xon_xoff_prefix # time_ms=0",
		"KTAP\t# elapsed_ms=0 pass=3 fail=0 skip=0 over_time=0",
	})
	tests := filterTests(events)
	if len(tests) != 3 {
		t.Fatalf("expected 3 tests, got %d", len(tests))
	}
	wantNames := []string{"mod::clean", "mod::with_garbage_prefix", "mod::xon_xoff_prefix"}
	for i, tt := range tests {
		if tt.Record.Name != wantNames[i] {
			t.Errorf("test %d: name = %q, want %q", i, tt.Record.Name, wantNames[i])
		}
		if tt.Record.Outcome != OutcomePass {
			t.Errorf("test %d: outcome = %s, want pass", i, tt.Record.Outcome)
		}
	}
}

func TestKtapKtapSubstringInsideLogLiteralIsNotRescued(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..1",
		"KTAP\tnot ok 1 - mod::leaks_ktap # time_ms=0",
		"KTAP\t  ---",
		"KTAP\t  outcome: Fail",
		"KTAP\t  file: x.rs:1",
		"KTAP\t  log: |",
		"KTAP\t   harmless KTAP\tok 1 - fake substring",
		"KTAP\t  ...",
		"KTAP\t# elapsed_ms=0 pass=0 fail=1 skip=0 over_time=0",
	})
	tests := filterTests(events)
	if len(tests) != 1 {
		t.Fatalf("expected 1 test, got %d", len(tests))
	}
	rec := tests[0].Record
	if rec.Outcome != OutcomeFail {
		t.Errorf("outcome: got %s, want fail", rec.Outcome)
	}
	if rec.Log == nil || !strings.Contains(*rec.Log, "harmless KTAP\tok 1 - fake substring") {
		t.Errorf("log content not preserved: %v", rec.Log)
	}
}

func TestRecorderAggregatesTwoPhases(t *testing.T) {
	p := NewKtapParser()
	rec := NewRecorder()
	for _, ln := range []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..2",
		"KTAP\tok 1 - mod::a # time_ms=1",
		"KTAP\tnot ok 2 - mod::b # time_ms=2",
		"KTAP\t  ---",
		"KTAP\t  outcome: Fail",
		"KTAP\t  file: a.rs:1",
		"KTAP\t  log: |",
		"KTAP\t   bad",
		"KTAP\t  ...",
		"KTAP\t# elapsed_ms=3 pass=1 fail=1 skip=0 over_time=0",
		"KTAP\tTAP version 14",
		"KTAP\t1..1",
		"KTAP\tok 1 - utest_x # time_ms=10",
		"KTAP\t# elapsed_ms=10 pass=1 fail=0 skip=0 over_time=0",
	} {
		for _, ev := range p.Feed(ln) {
			rec.Record(ev)
		}
	}
	three := 3
	rec.Finalize(&three)
	if rec.Summary.Total() != 3 {
		t.Errorf("total: got %d, want 3", rec.Summary.Total())
	}
	failures := rec.Summary.Failures()
	if len(failures) != 1 || failures[0].Name != "mod::b" {
		t.Errorf("failures: got %+v, want 1 mod::b", failures)
	}
	if rec.Summary.CounterSum("pass") != 2 {
		t.Errorf("pass sum: %d", rec.Summary.CounterSum("pass"))
	}
	if rec.Summary.CounterSum("fail") != 1 {
		t.Errorf("fail sum: %d", rec.Summary.CounterSum("fail"))
	}
	if rec.Summary.Truncated {
		t.Errorf("truncated should be false")
	}
}

func filterAborts(events []Event) []*EvKernelAbort {
	var out []*EvKernelAbort
	for _, e := range events {
		if a, ok := e.(*EvKernelAbort); ok {
			out = append(out, a)
		}
	}
	return out
}

func TestKernelAbortBannerEmitsEvent(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"",
		KernelAbortBanner,
		"NMI watchdog: CPU made no progress, sustained",
		"System halted.",
	})
	aborts := filterAborts(events)
	if len(aborts) != 1 {
		t.Fatalf("want exactly 1 abort event, got %d", len(aborts))
	}
	if aborts[0].Reason != "NMI watchdog: CPU made no progress, sustained" {
		t.Fatalf("reason drifted: %q", aborts[0].Reason)
	}
}

// The kernel writes "\n\n=== KERNEL ABORT ===", so a blank line precedes the
// banner; a blank line must never be mistaken for the reason.
func TestKernelAbortIgnoresBlankLinesBeforeTheReason(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{KernelAbortBanner, "   ", "panic core abort"})
	aborts := filterAborts(events)
	if len(aborts) != 1 || aborts[0].Reason != "panic core abort" {
		t.Fatalf("want one abort with the reason line, got %+v", aborts)
	}
}

// The fully-dead-machine case: the banner is the last thing the kernel wrote.
func TestBareKernelAbortBannerStillEmitsOnFlush(t *testing.T) {
	p := NewKtapParser()
	if n := len(filterAborts(feedAll(p, []string{KernelAbortBanner}))); n != 0 {
		t.Fatalf("banner alone should not emit until resolved, got %d", n)
	}
	aborts := filterAborts(p.Flush())
	if len(aborts) != 1 {
		t.Fatalf("Flush must emit the pending abort, got %d", len(aborts))
	}
	if aborts[0].Reason != "" {
		t.Fatalf("want empty reason, got %q", aborts[0].Reason)
	}
	if n := len(filterAborts(p.Flush())); n != 0 {
		t.Fatalf("a second Flush must emit nothing, got %d", n)
	}
}

// The recorded case: the abort was CPU 1 only and the BSP produced thousands
// of further results. A result line arriving straight after the banner must
// still parse, and the abort must still be reported.
func TestKernelAbortDoesNotDisturbKtapParsing(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..2",
		"KTAP\tok 1 - mod::a # time_ms=1",
		KernelAbortBanner,
		"KTAP\tok 2 - mod::b # time_ms=2",
		"KTAP\t# elapsed_ms=9 pass=2 fail=0 skip=0 over_time=0",
	})
	if n := len(filterAborts(events)); n != 1 {
		t.Fatalf("a KTAP line must resolve the banner, got %d aborts", n)
	}
	tests := filterTests(events)
	if len(tests) != 2 {
		t.Fatalf("want 2 test records across the abort, got %d", len(tests))
	}
	if tests[1].Record.Name != "mod::b" || tests[1].Record.Outcome != OutcomePass {
		t.Fatalf("result after the banner mis-parsed: %+v", tests[1].Record)
	}
	if len(filterPhaseEnds(events)) != 1 {
		t.Fatalf("footer must still parse after an abort")
	}
}

// Two CPUs aborting back to back are two events, not one swallowed by the
// other; and the second banner is never read as the first one's reason.
func TestBackToBackKernelAbortsEmitSeparateEvents(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		KernelAbortBanner,
		KernelAbortBanner,
		"panic core abort",
	})
	aborts := filterAborts(events)
	if len(aborts) != 2 {
		t.Fatalf("want 2 abort events, got %d", len(aborts))
	}
	if aborts[0].Reason != "" {
		t.Fatalf("first abort has no reason of its own, got %q", aborts[0].Reason)
	}
	if aborts[1].Reason != "panic core abort" {
		t.Fatalf("second abort lost its reason: %q", aborts[1].Reason)
	}
}

// The banner goes out over the polling early console while peers may still be
// writing klog, so it can arrive with a foreign prefix glued to its head.
func TestKernelAbortDetectedWithInterleavedPrefix(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"SCHED: cpu 3 idle" + KernelAbortBanner,
		"NMI watchdog: sustained",
	})
	if aborts := filterAborts(events); len(aborts) != 1 ||
		aborts[0].Reason != "NMI watchdog: sustained" {
		t.Fatalf("interleaved banner not recognised: %+v", aborts)
	}
}

// The banner is klog like any other klog: it stays in the tail attached to the
// next failure, and it still surfaces as EvNonKtap for --raw.
func TestKernelAbortStillReachesKlogTailAndNonKtap(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{KernelAbortBanner, "panic core abort"})
	var nonKtap int
	for _, e := range events {
		if _, ok := e.(*EvNonKtap); ok {
			nonKtap++
		}
	}
	if nonKtap != 2 {
		t.Fatalf("want both lines as EvNonKtap, got %d", nonKtap)
	}
	tail := p.KlogTail()
	if len(tail) != 2 || tail[0] != KernelAbortBanner {
		t.Fatalf("banner missing from klog tail: %q", tail)
	}
}

// A pending banner must not survive into the next phase: whatever the next
// phase's first klog line says, it is not this abort's reason.
func TestKernelAbortDoesNotCrossPhaseBoundary(t *testing.T) {
	p := NewKtapParser()
	events := feedAll(p, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..1",
		"KTAP\tok 1 - mod::a # time_ms=1",
		KernelAbortBanner,
		"KTAP\tTAP version 14",
		"USERLAND: launched /sbin/init",
	})
	aborts := filterAborts(events)
	if len(aborts) != 1 {
		t.Fatalf("want 1 abort, got %d", len(aborts))
	}
	if aborts[0].Reason != "" {
		t.Fatalf("next phase's klog must not become the reason, got %q", aborts[0].Reason)
	}
	if n := len(filterPhaseStarts(events)); n != 2 {
		t.Fatalf("both phases must still open, got %d", n)
	}
}

// A record left uncommitted when the stream died is evidence, not litter.
func TestFlushCommitsTheTrailingRecord(t *testing.T) {
	p := NewKtapParser()
	feedAll(p, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..2",
		"KTAP\tnot ok 1 - mod::a # time_ms=1",
	})
	tests := filterTests(p.Flush())
	if len(tests) != 1 || tests[0].Record.Name != "mod::a" {
		t.Fatalf("Flush must commit the pending record, got %+v", tests)
	}
}

func TestKernelAbortRecordedOnSummary(t *testing.T) {
	p, r := NewKtapParser(), NewRecorder()
	for _, ln := range []string{KernelAbortBanner, "NMI watchdog: sustained"} {
		for _, ev := range p.Feed(ln) {
			r.Record(ev)
		}
	}
	if !r.Summary.KernelAbort {
		t.Fatalf("recorder must latch KernelAbort")
	}
	if r.Summary.KernelAbortReason != "NMI watchdog: sustained" {
		t.Fatalf("reason not recorded: %q", r.Summary.KernelAbortReason)
	}
}

// The first reason names the CPU that died first; a later abort is usually the
// cascade and must not overwrite it.
func TestKernelAbortRecorderKeepsTheFirstReason(t *testing.T) {
	r := NewRecorder()
	r.Record(&EvKernelAbort{Reason: "first"})
	r.Record(&EvKernelAbort{Reason: "second"})
	if r.Summary.KernelAbortReason != "first" {
		t.Fatalf("first reason must win, got %q", r.Summary.KernelAbortReason)
	}
}

// A reasonless abort followed by one that has a reason still records it: the
// latch is on the flag, the reason is filled by whichever event first has one.
func TestKernelAbortRecorderTakesTheFirstNonEmptyReason(t *testing.T) {
	r := NewRecorder()
	r.Record(&EvKernelAbort{})
	if !r.Summary.KernelAbort {
		t.Fatalf("a reasonless abort must still latch the flag")
	}
	r.Record(&EvKernelAbort{Reason: "panic core abort"})
	if r.Summary.KernelAbortReason != "panic core abort" {
		t.Fatalf("reason not backfilled: %q", r.Summary.KernelAbortReason)
	}
}
