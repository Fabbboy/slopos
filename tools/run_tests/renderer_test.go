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
