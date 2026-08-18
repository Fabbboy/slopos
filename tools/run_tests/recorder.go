package main

import "time"

// Outcome is the union of result states a test (or sub-test) can land in.
// The strings are stable: they are the JSONL `outcome:` field verbatim.
type Outcome string

const (
	OutcomePass    Outcome = "pass"
	OutcomeFail    Outcome = "fail"
	OutcomeSkip    Outcome = "skip"
	OutcomeBail    Outcome = "bail"
	OutcomeNotRun  Outcome = "not_run"
	OutcomeTimeout Outcome = "timeout"
)

// IsFailure groups the outcomes that get surfaced as failures.
func (o Outcome) IsFailure() bool {
	return o == OutcomeFail || o == OutcomeBail || o == OutcomeTimeout
}

// Subtest is a single nested test result from a userland (`utest!`) runner;
// the kernel nests several under one parent `ok N - …` line.
type Subtest struct {
	Idx     int     `json:"idx"`
	Name    string  `json:"name"`
	Outcome Outcome `json:"outcome"`
	Msg     string  `json:"msg,omitempty"`
}

// TestRecord is the per-test datum read by both the renderer and the JSONL
// sink.
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

// RunSummary is the top-level result aggregator.
type RunSummary struct {
	Phases            []*PhaseRecord
	StartedMonotonic  time.Time
	FinishedMonotonic time.Time
	Truncated         bool
	UserAborted       bool
	TimedOut          bool
	SilenceHit        bool
	QemuStatus        *int
	// AbortKlogTail is the parser's klog tail, snapshotted by main when the
	// run aborted (timeout / silence / truncation).
	AbortKlogTail []string
	phaseByIdx    map[int]*PhaseRecord
}

// WallMs returns the run's wall-clock duration in milliseconds, or the
// elapsed time so far when called before Finalize.
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

// Failures returns every failing test, in phase / idx order.
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

// Record applies one event to the summary state.
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

// Finalize stamps the end time, captures the QEMU exit status, and flags
// truncation: planned > observed in a phase that did not bail.
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
