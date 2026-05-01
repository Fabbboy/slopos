package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
)

// JsonlSink emits one compact JSON object per line, machine-consumable
// downstream (CI dashboards, JUnit converters, test-history regression
// detectors). Schema is documented in `docs/test_output.md` §10.
type JsonlSink struct {
	path string
	f    *os.File
}

// NewJsonlSink creates the output file (parent dirs included) and returns
// an open sink. Caller must `Close` at end of run.
func NewJsonlSink(path string) (*JsonlSink, error) {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return nil, fmt.Errorf("jsonl: mkdir %q: %w", filepath.Dir(path), err)
	}
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0o644)
	if err != nil {
		return nil, fmt.Errorf("jsonl: open %q: %w", path, err)
	}
	return &JsonlSink{path: path, f: f}, nil
}

// Write emits one event. Returns nil for events that are intentionally
// not surfaced (currently EvNonKtap — too noisy and not stable enough
// to belong in the machine-consumable stream).
func (s *JsonlSink) Write(ev Event, _ *RunSummary) error {
	obj := encodeEvent(ev)
	if obj == nil {
		return nil
	}
	b, err := json.Marshal(obj)
	if err != nil {
		return fmt.Errorf("jsonl: marshal: %w", err)
	}
	if _, err := s.f.Write(append(b, '\n')); err != nil {
		return fmt.Errorf("jsonl: write: %w", err)
	}
	return nil
}

// WriteRunEnd is the last event in any run — a `run_end` summary that
// downstream tools can use to build dashboards / regression history
// without scanning the full per-test event sequence.
func (s *JsonlSink) WriteRunEnd(summary *RunSummary, exitCode int) error {
	phases := make([]map[string]any, 0, len(summary.Phases))
	for _, p := range summary.Phases {
		var planN any
		if p.PlanN != nil {
			planN = *p.PlanN
		}
		var elapsed any
		if p.ElapsedMs != nil {
			elapsed = *p.ElapsedMs
		}
		var bail any
		if p.BailReason != nil {
			bail = *p.BailReason
		}
		phases = append(phases, map[string]any{
			"idx":        p.Idx,
			"name":       p.Name,
			"plan_n":     planN,
			"elapsed_ms": elapsed,
			"pass":       p.Counters.Pass,
			"fail":       p.Counters.Fail,
			"skip":       p.Counters.Skip,
			"over_time":  p.Counters.OverTime,
			"bail":       bail,
		})
	}
	var qemuStatus any
	if summary.QemuStatus != nil {
		qemuStatus = *summary.QemuStatus
	}
	obj := map[string]any{
		"t":            "run_end",
		"wall_ms":      summary.WallMs(),
		"exit":         exitCode,
		"qemu_status":  qemuStatus,
		"user_aborted": summary.UserAborted,
		"timed_out":    summary.TimedOut,
		"truncated":    summary.Truncated,
		"phases":       phases,
	}
	b, err := json.Marshal(obj)
	if err != nil {
		return fmt.Errorf("jsonl: marshal run_end: %w", err)
	}
	if _, err := s.f.Write(append(b, '\n')); err != nil {
		return fmt.Errorf("jsonl: write run_end: %w", err)
	}
	return nil
}

// Close flushes and closes the underlying file. Best-effort fsync so a
// crashed wrapper doesn't lose the last few events.
func (s *JsonlSink) Close() error {
	if s.f == nil {
		return nil
	}
	_ = s.f.Sync()
	err := s.f.Close()
	s.f = nil
	return err
}

// encodeEvent turns a parser Event into a marshallable map. Returns nil
// for events deliberately suppressed from the JSONL stream.
func encodeEvent(ev Event) map[string]any {
	switch e := ev.(type) {
	case *EvPhaseStart:
		return map[string]any{
			"t":    "phase_start",
			"idx":  e.PhaseIdx,
			"name": e.Name,
		}
	case *EvPlan:
		return map[string]any{
			"t":         "plan",
			"phase_idx": e.PhaseIdx,
			"n":         e.N,
		}
	case *EvTest:
		r := e.Record
		subs := make([]map[string]any, 0, len(r.Subtests))
		for _, s := range r.Subtests {
			subs = append(subs, map[string]any{
				"idx":     s.Idx,
				"name":    s.Name,
				"outcome": string(s.Outcome),
				"msg":     s.Msg,
			})
		}
		// Build the dict with `nil` rather than `nil any` so JSON renders
		// as `null` not omitted — keeps schema parity with the Python
		// dataclass that always emitted these keys.
		out := map[string]any{
			"t":                  "test",
			"phase":              r.PhaseName,
			"phase_idx":          r.PhaseIdx,
			"idx":                r.Idx,
			"name":               r.Name,
			"outcome":            string(r.Outcome),
			"over_time":          r.OverTime,
			"expected_panic":     r.ExpectedPanic,
			"subtests":           subs,
			"pre_fail_klog_tail": r.PreFailKlogTail,
		}
		if r.TimeMs != nil {
			out["time_ms"] = *r.TimeMs
		} else {
			out["time_ms"] = nil
		}
		out["skip_reason"] = ptrOrNil(r.SkipReason)
		out["fail_file"] = ptrOrNil(r.FailFile)
		out["fail_outcome_kind"] = ptrOrNil(r.FailOutcomeKind)
		out["log"] = ptrOrNil(r.Log)
		return out
	case *EvPhaseEnd:
		return map[string]any{
			"t":          "phase_end",
			"idx":        e.PhaseIdx,
			"name":       e.Name,
			"elapsed_ms": e.ElapsedMs,
			"pass":       e.Pass,
			"fail":       e.Fail,
			"skip":       e.Skip,
			"over_time":  e.OverTime,
		}
	case *EvBail:
		return map[string]any{
			"t":         "bail",
			"phase_idx": e.PhaseIdx,
			"reason":    e.Reason,
		}
	}
	return nil
}

// ptrOrNil unwraps a `*string` into either its value (rendered as a JSON
// string) or `nil` (rendered as JSON null) — mirroring the Python wrapper's
// emit of `null`-valued keys for absent fields.
func ptrOrNil(p *string) any {
	if p == nil {
		return nil
	}
	return *p
}
