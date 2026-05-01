package main

import "time"

// Outcome is the union of result states a test (or sub-test) can land in.
// Strings are stable and match the JSONL `outcome:` field the Python
// wrapper emits — Phase 5N's differential diff depends on byte-identity.
type Outcome string

const (
	OutcomePass    Outcome = "pass"
	OutcomeFail    Outcome = "fail"
	OutcomeSkip    Outcome = "skip"
	OutcomeBail    Outcome = "bail"
	OutcomeNotRun  Outcome = "not_run"
	OutcomeTimeout Outcome = "timeout"
)

// IsFailure groups every outcome the count-the-fails loops should treat as
// "this is what we want to surface above the bar / in the failure block".
func (o Outcome) IsFailure() bool {
	return o == OutcomeFail || o == OutcomeBail || o == OutcomeTimeout
}

// Subtest is a single nested test result emitted by a userland (`utest!`)
// runner. The kernel wraps multiple of them inside one parent `ok N - …`
// line; the parser buffers them and attaches at parent-commit time.
type Subtest struct {
	Idx     int     `json:"idx"`
	Name    string  `json:"name"`
	Outcome Outcome `json:"outcome"`
	Msg     string  `json:"msg,omitempty"`
}

// TestRecord is the per-test datum recorded once and then read by both
// the renderer and the JSONL sink. Field ordering in JSON tags mirrors
// the Python @dataclass exactly so a byte-level diff between the two
// wrappers' --json outputs stays empty (see Phase 5N).
type TestRecord struct {
	PhaseIdx        int       `json:"phase_idx"`
	PhaseName       string    `json:"phase"`
	Idx             int       `json:"idx"`
	Name            string    `json:"name"`
	Outcome         Outcome   `json:"outcome"`
	TimeMs          *int      `json:"time_ms"`
	OverTime        bool      `json:"over_time"`
	ExpectedPanic   bool      `json:"expected_panic"`
	SkipReason      *string   `json:"skip_reason"`
	FailFile        *string   `json:"fail_file"`
	FailOutcomeKind *string   `json:"fail_outcome_kind"`
	Log             *string   `json:"log"`
	Subtests        []Subtest `json:"subtests"`
	PreFailKlogTail []string  `json:"pre_fail_klog_tail"`
}

// PhaseCounters aggregates per-phase outcome counts.
type PhaseCounters struct {
	Pass     int `json:"pass"`
	Fail     int `json:"fail"`
	Skip     int `json:"skip"`
	OverTime int `json:"over_time"`
}

// PhaseRecord describes one phase's plan + observed-test list.
type PhaseRecord struct {
	Idx        int           `json:"idx"`
	Name       string        `json:"name"`
	PlanN      *int          `json:"plan_n"`
	ElapsedMs  *int          `json:"elapsed_ms"`
	Counters   PhaseCounters `json:"counters"`
	BailReason *string       `json:"bail"`
	Tests      []*TestRecord `json:"-"`
}

// RunSummary is the top-level result aggregator. The renderer reads it at
// finalize-time; the JSONL sink writes a `run_end` event from it.
type RunSummary struct {
	Phases            []*PhaseRecord
	StartedMonotonic  time.Time
	FinishedMonotonic time.Time
	Truncated         bool
	UserAborted       bool
	TimedOut          bool
	QemuStatus        *int
	phaseByIdx        map[int]*PhaseRecord
}

// WallMs returns the run's wall-clock duration in milliseconds. If the
// run hasn't finished yet (unusual; only happens before Finalize), it
// reports the elapsed time so far.
func (s *RunSummary) WallMs() int {
	end := s.FinishedMonotonic
	if end.IsZero() {
		end = time.Now()
	}
	return int(end.Sub(s.StartedMonotonic) / time.Millisecond)
}

// AllTests yields every test record across every phase in encounter order.
func (s *RunSummary) AllTests() []*TestRecord {
	n := 0
	for _, p := range s.Phases {
		n += len(p.Tests)
	}
	out := make([]*TestRecord, 0, n)
	for _, p := range s.Phases {
		out = append(out, p.Tests...)
	}
	return out
}

// Failures returns every test whose outcome counts as a failure (Fail,
// Bail, or Timeout). Order is phase / idx.
func (s *RunSummary) Failures() []*TestRecord {
	var out []*TestRecord
	for _, t := range s.AllTests() {
		if t.Outcome.IsFailure() {
			out = append(out, t)
		}
	}
	return out
}

// Total counts every test the recorder observed across all phases.
func (s *RunSummary) Total() int {
	n := 0
	for _, p := range s.Phases {
		n += len(p.Tests)
	}
	return n
}

// PlannedTotal sums the kernel's `1..N` plan numbers across phases. If a
// phase didn't emit a plan line we fall back to its observed test count.
func (s *RunSummary) PlannedTotal() int {
	n := 0
	for _, p := range s.Phases {
		if p.PlanN != nil {
			n += *p.PlanN
		} else {
			n += len(p.Tests)
		}
	}
	return n
}

// CounterSum returns one counter aggregated across phases.
func (s *RunSummary) CounterSum(field string) int {
	n := 0
	for _, p := range s.Phases {
		switch field {
		case "pass":
			n += p.Counters.Pass
		case "fail":
			n += p.Counters.Fail
		case "skip":
			n += p.Counters.Skip
		case "over_time":
			n += p.Counters.OverTime
		}
	}
	return n
}

// RunRecorder consumes parser events and maintains the live RunSummary.
// It's deliberately separate from the renderer so the JSONL sink and the
// renderer can both read from one consistent view.
type RunRecorder struct {
	Summary *RunSummary
}

// NewRecorder creates a fresh recorder, marking the run start time.
func NewRecorder() *RunRecorder {
	return &RunRecorder{
		Summary: &RunSummary{
			StartedMonotonic: time.Now(),
			phaseByIdx:       make(map[int]*PhaseRecord),
		},
	}
}

// Record applies one event to the summary state. Pure data: no rendering.
func (r *RunRecorder) Record(ev Event) {
	switch e := ev.(type) {
	case *EvPhaseStart:
		p := &PhaseRecord{Idx: e.PhaseIdx, Name: e.Name}
		r.Summary.Phases = append(r.Summary.Phases, p)
		r.Summary.phaseByIdx[e.PhaseIdx] = p
	case *EvPlan:
		if p := r.Summary.phaseByIdx[e.PhaseIdx]; p != nil {
			n := e.N
			p.PlanN = &n
		}
	case *EvTest:
		p := r.Summary.phaseByIdx[e.Record.PhaseIdx]
		if p == nil {
			return
		}
		p.Tests = append(p.Tests, e.Record)
		switch e.Record.Outcome {
		case OutcomePass:
			p.Counters.Pass++
		case OutcomeFail:
			p.Counters.Fail++
		case OutcomeSkip:
			p.Counters.Skip++
		}
		if e.Record.OverTime {
			p.Counters.OverTime++
		}
	case *EvPhaseEnd:
		if p := r.Summary.phaseByIdx[e.PhaseIdx]; p != nil {
			ms := e.ElapsedMs
			p.ElapsedMs = &ms
		}
	case *EvBail:
		if p := r.Summary.phaseByIdx[e.PhaseIdx]; p != nil {
			reason := e.Reason
			p.BailReason = &reason
		}
	}
	// EvNonKtap is consumed only by the raw renderer; recorder ignores it.
}

// Finalize closes the run, stamps wall-clock end time, captures the QEMU
// exit status, and detects truncation (planned > observed in any phase
// that didn't bail intentionally).
func (r *RunRecorder) Finalize(qemuStatus *int) {
	r.Summary.FinishedMonotonic = time.Now()
	r.Summary.QemuStatus = qemuStatus
	for _, p := range r.Summary.Phases {
		observed := len(p.Tests)
		if p.PlanN != nil && observed < *p.PlanN && p.BailReason == nil {
			r.Summary.Truncated = true
		}
	}
}
