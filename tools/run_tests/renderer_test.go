package main

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func recorderFromLines(lines []string) *RunRecorder {
	p := NewKtapParser()
	rec := NewRecorder()
	for _, ln := range lines {
		for _, ev := range p.Feed(ln) {
			rec.Record(ev)
		}
	}
	return rec
}

func TestRendererTimeoutDumpsKlogTail(t *testing.T) {
	lines := []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..3",
		"KTAP\tok 1 - mod::a # time_ms=1",
		"TESTS: Kernel phase completed successfully",
		"USERLAND: launched /sbin/init as task 5",
		"KERNEL: about to wedge",
	}
	p := NewKtapParser()
	rec := NewRecorder()
	for _, ln := range lines {
		for _, ev := range p.Feed(ln) {
			rec.Record(ev)
		}
	}
	rec.Summary.TimedOut = true
	rec.Summary.SilenceHit = true
	rec.Summary.AbortKlogTail = p.KlogTail()

	var buf bytes.Buffer
	r := NewBarRenderer(&buf, "summary", false, 0, false, 100)
	r.Finalize(rec.Summary)
	out := buf.String()

	for _, want := range []string{
		"klog tail",
		"TESTS: Kernel phase completed successfully",
		"USERLAND: launched /sbin/init as task 5",
		"KERNEL: about to wedge",
		"NO OUTPUT",
	} {
		if !strings.Contains(out, want) {
			t.Errorf("missing %q in:\n%s", want, out)
		}
	}
}

func TestRendererGreenRunSummary(t *testing.T) {
	lines := []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..2",
		"KTAP\tok 1 - mod::a # time_ms=1",
		"KTAP\tok 2 - mod::b # time_ms=2",
		"KTAP\t# elapsed_ms=3 pass=2 fail=0 skip=0 over_time=0",
	}
	rec := recorderFromLines(lines)

	var buf bytes.Buffer
	r := NewBarRenderer(&buf, "summary", false, 0, false, 100)
	p := NewKtapParser()
	for _, ln := range lines {
		for _, ev := range p.Feed(ln) {
			r.OnEvent(ev, rec.Summary)
		}
	}
	one := 1
	rec.Finalize(&one)
	r.Finalize(rec.Summary)

	out := buf.String()
	if !strings.Contains(out, "kernel") {
		t.Errorf("missing kernel header: %q", out)
	}
	if !strings.Contains(out, "2 pass") {
		t.Errorf("missing 2 pass: %q", out)
	}
	if !strings.Contains(out, "0 fail") {
		t.Errorf("missing 0 fail: %q", out)
	}
	if strings.Contains(out, "==== FAILURE") {
		t.Errorf("unexpected failure block in green run: %q", out)
	}
}

func TestRendererFailureBlockWithLog(t *testing.T) {
	lines := []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..1",
		"KTAP\tnot ok 1 - mod::bad # time_ms=11",
		"KTAP\t  ---",
		"KTAP\t  outcome: Fail",
		"KTAP\t  file: x.rs:1",
		"KTAP\t  log: |",
		"KTAP\t   ASSERT_EQ: expected 5, got 9",
		"KTAP\t  ...",
		"KTAP\t# elapsed_ms=11 pass=0 fail=1 skip=0 over_time=0",
	}
	var buf bytes.Buffer
	r := NewBarRenderer(&buf, "summary", false, 0, false, 100)
	rec := NewRecorder()
	p := NewKtapParser()
	for _, ln := range lines {
		for _, ev := range p.Feed(ln) {
			rec.Record(ev)
			r.OnEvent(ev, rec.Summary)
		}
	}
	three := 3
	rec.Finalize(&three)
	r.Finalize(rec.Summary)

	out := buf.String()
	if !strings.Contains(out, "==== FAILURE 1 of 1 — mod::bad ====") {
		t.Errorf("missing failure header: %q", out)
	}
	if !strings.Contains(out, "file:     x.rs:1") {
		t.Errorf("missing file line: %q", out)
	}
	if !strings.Contains(out, "ASSERT_EQ: expected 5, got 9") {
		t.Errorf("missing assert content: %q", out)
	}
	if !strings.Contains(out, "1 failed") {
		t.Errorf("missing 1 failed in summary: %q", out)
	}
}

func TestRendererStreamingFailureWarning(t *testing.T) {
	lines := []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..2",
		"KTAP\tok 1 - mod::a # time_ms=1",
		"KTAP\tnot ok 2 - mod::bad # time_ms=11",
		"KTAP\t  ---",
		"KTAP\t  outcome: Fail",
		"KTAP\t  file: x.rs:1",
		"KTAP\t  log: |",
		"KTAP\t   boom",
		"KTAP\t  ...",
		"KTAP\t# elapsed_ms=12 pass=1 fail=1 skip=0 over_time=0",
	}
	var buf bytes.Buffer
	r := NewBarRenderer(&buf, "summary", false, 0, false, 100)
	rec := NewRecorder()
	p := NewKtapParser()
	for _, ln := range lines {
		for _, ev := range p.Feed(ln) {
			rec.Record(ev)
			r.OnEvent(ev, rec.Summary)
		}
	}
	three := 3
	rec.Finalize(&three)
	r.Finalize(rec.Summary)

	out := buf.String()
	if !strings.Contains(out, "FAIL 2 mod::bad") {
		t.Errorf("missing streaming FAIL warning: %q", out)
	}
}

func TestJsonlSinkOneEventPerLine(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "events.jsonl")
	sink, err := NewJsonlSink(path)
	if err != nil {
		t.Fatal(err)
	}
	rec := NewRecorder()
	p := NewKtapParser()
	for _, ln := range []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..1",
		"KTAP\tok 1 - mod::a # time_ms=1",
		"KTAP\t# elapsed_ms=1 pass=1 fail=0 skip=0 over_time=0",
	} {
		for _, ev := range p.Feed(ln) {
			rec.Record(ev)
			if err := sink.Write(ev, rec.Summary); err != nil {
				t.Fatal(err)
			}
		}
	}
	one := 0
	rec.Finalize(&one)
	if err := sink.WriteRunEnd(rec.Summary, 0); err != nil {
		t.Fatal(err)
	}
	if err := sink.Close(); err != nil {
		t.Fatal(err)
	}

	body, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(strings.TrimRight(string(body), "\n"), "\n")
	if len(lines) < 4 {
		t.Fatalf("expected ≥4 lines, got %d: %q", len(lines), body)
	}
	seenTypes := map[string]bool{}
	for _, ln := range lines {
		var obj map[string]any
		if err := json.Unmarshal([]byte(ln), &obj); err != nil {
			t.Errorf("invalid JSON line %q: %v", ln, err)
			continue
		}
		if t, ok := obj["t"].(string); ok {
			seenTypes[t] = true
		}
	}
	for _, want := range []string{"phase_start", "plan", "test", "phase_end", "run_end"} {
		if !seenTypes[want] {
			t.Errorf("missing event type %q in %v", want, seenTypes)
		}
	}
}

// The banner has to reach the operator while the run is still going, and the
// final summary must not paint a zero-failure abort green.
func TestRendererKernelAbortIsReportedAndNotGreen(t *testing.T) {
	lines := []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..2",
		"KTAP\tok 1 - mod::a # time_ms=1",
		KernelAbortBanner,
		"NMI watchdog: CPU made no progress, sustained",
		"KTAP\tok 2 - mod::b # time_ms=2",
		"KTAP\t# elapsed_ms=9 pass=2 fail=0 skip=0 over_time=0",
	}
	// Colour on, or the ansiGreen assertion below is vacuous: Paint is a
	// pass-through when colour is off.
	var buf bytes.Buffer
	r := NewBarRenderer(&buf, "summary", true, 0, false, 100)
	rec := NewRecorder()
	p := NewKtapParser()
	for _, ln := range lines {
		for _, ev := range p.Feed(ln) {
			rec.Record(ev)
			r.OnEvent(ev, rec.Summary)
		}
	}
	zero := 0
	rec.Finalize(&zero)
	r.Finalize(rec.Summary)

	out := buf.String()
	if !strings.Contains(out, ansiRedBold) {
		t.Fatalf("colour is off, so the green assertion below proves nothing:\n%q", out)
	}
	if strings.Count(out, "KERNEL ABORT on some CPU") != 2 {
		t.Errorf("want the abort inline and in the summary, got:\n%s", out)
	}
	if !strings.Contains(out, "NMI watchdog: CPU made no progress, sustained") {
		t.Errorf("summary lost the abort reason:\n%s", out)
	}
	// The per-phase line stays green on purpose: it echoes the kernel's own
	// footer for that phase, which really did report pass=2 fail=0. It is the
	// run summary that must not read as a clean run.
	if !strings.Contains(out, ansiRedBold+"2 tests across 1 phase") {
		t.Errorf("the run summary must be red, not green:\n%q", out)
	}
	if strings.Contains(out, ansiGreen+"2 tests across") {
		t.Errorf("a run with an abort must not summarise green:\n%q", out)
	}
}

func TestJsonlEncodesKernelAbort(t *testing.T) {
	obj := encodeEvent(&EvKernelAbort{Reason: "panic core abort"})
	if obj == nil {
		t.Fatalf("kernel_abort must reach the JSONL stream")
	}
	if obj["t"] != "kernel_abort" || obj["reason"] != "panic core abort" {
		t.Fatalf("unexpected encoding: %+v", obj)
	}
}

func TestJsonlRunEndCarriesKernelAbort(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "events.jsonl")
	sink, err := NewJsonlSink(path)
	if err != nil {
		t.Fatalf("NewJsonlSink: %v", err)
	}
	s := NewRecorder().Summary
	s.KernelAbort = true
	s.KernelAbortReason = "panic core abort"
	if err := sink.WriteRunEnd(s, 1); err != nil {
		t.Fatalf("WriteRunEnd: %v", err)
	}
	if err := sink.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}
	body, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("ReadFile: %v", err)
	}
	var obj map[string]any
	if err := json.Unmarshal(bytes.TrimSpace(body), &obj); err != nil {
		t.Fatalf("unmarshal run_end: %v", err)
	}
	if obj["kernel_abort"] != true {
		t.Fatalf("run_end must carry kernel_abort: %+v", obj)
	}
	if obj["kernel_abort_reason"] != "panic core abort" {
		t.Fatalf("run_end must carry the reason: %+v", obj)
	}
}
