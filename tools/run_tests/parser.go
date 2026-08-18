package main

import (
	"strconv"
	"strings"
)

// Event is the sealed interface every parser-emitted event implements.
type Event interface {
	event()
}

// EvPhaseStart fires the first time a phase's `KTAP\tTAP version 14` arrives.
type EvPhaseStart struct {
	PhaseIdx int
	Name     string
}

// EvPlan carries the `1..N` plan number for the phase that's currently open.
type EvPlan struct {
	PhaseIdx int
	N        int
}

// EvTest is the parser's commit of one fully-formed test result, with any
// pending diag block and subtests already attached.
type EvTest struct {
	Record *TestRecord
}

// EvPhaseEnd carries the kernel's footer counters for a phase.
type EvPhaseEnd struct {
	PhaseIdx  int
	Name      string
	ElapsedMs int
	Pass      int
	Fail      int
	Skip      int
	OverTime  int
}

// EvBail signals an aborted phase (a bootstrap test failed; no further
// tests will run in this phase).
type EvBail struct {
	PhaseIdx int
	Reason   string
}

// EvNonKtap surfaces every line that isn't part of the harness wire format.
type EvNonKtap struct {
	Line string
}

func (*EvPhaseStart) event() {}
func (*EvPlan) event()       {}
func (*EvTest) event()       {}
func (*EvPhaseEnd) event()   {}
func (*EvBail) event()       {}
func (*EvNonKtap) event()    {}

// parserState is the FSM state. The diag-vs-log split exists because a
// `log: |` literal block is closed only by `KTAP\t  ...`; a `KTAP\t` substring
// inside the captured klog is content, not a new top-level line.
type parserState int

const (
	stateOutside parserState = iota
	stateInDiag
	stateInLogLiteral
)

// PhaseNames maps phase ordinal to label by convention (phase 1 = kernel,
// phase 2 = userland): the kernel emits phase names only as non-KTAP klog,
// so they are never read off the wire.
var PhaseNames = []string{"kernel", "userland"}

// phaseNameFor maps a 1-based phase index to its label.
func phaseNameFor(idx int) string {
	if idx >= 1 && idx <= len(PhaseNames) {
		return PhaseNames[idx-1]
	}
	return "phase" + strconv.Itoa(idx)
}

// KtapParser is a streaming line-by-line parser of the kernel's KTAP output.
// One instance per run; goroutine-unsafe.
type KtapParser struct {
	state           parserState
	phaseIdx        int
	phaseName       string
	tapVersionSeen  bool
	pendingSubtests []Subtest
	currentRecord   *TestRecord
	logLines        []string
	logBlockOpened  bool
	klogTail        []string
}

// NewKtapParser returns an empty parser ready to consume `Feed` calls.
func NewKtapParser() *KtapParser {
	return &KtapParser{}
}

// KlogTail returns a copy of the rolling window of recent non-KTAP klog lines.
func (p *KtapParser) KlogTail() []string {
	if len(p.klogTail) == 0 {
		return nil
	}
	out := make([]string, len(p.klogTail))
	copy(out, p.klogTail)
	return out
}

// Feed consumes one input line, which must NOT include its trailing newline,
// and returns zero or more events the caller then owns outright.
func (p *KtapParser) Feed(rawLine string) []Event {
	line := stripANSI(rawLine)

	// TTY-layer tests write fixture bytes straight to COM1, bypassing klog, so
	// the next result line arrives prefixed: `helloKTAP\tok 994 - …`. Rescue
	// only outside a `log: |` literal, where a `KTAP\t` substring is genuine
	// captured-log content rather than a result line.
	if p.state == stateOutside &&
		!strings.HasPrefix(line, KtapPrefix) &&
		strings.Contains(line, KtapPrefix) {
		line = line[strings.Index(line, KtapPrefix):]
	}

	if !strings.HasPrefix(line, KtapPrefix) {
		// A non-KTAP line arriving mid-log-literal means the kernel was
		// interrupted (e.g. a panic on another CPU writing plain klog).
		var events []Event
		if p.state == stateInLogLiteral {
			if ev := p.commitCurrentRecord(); ev != nil {
				events = append(events, ev)
			}
		}
		p.appendKlogTail(line)
		events = append(events, &EvNonKtap{Line: line})
		return events
	}

	body := line[len(KtapPrefix):]
	return p.feedKtap(body)
}

// appendKlogTail keeps a bounded rolling window of recent non-KTAP klog lines.
func (p *KtapParser) appendKlogTail(line string) {
	p.klogTail = append(p.klogTail, line)
	if len(p.klogTail) > KlogTailLines {
		p.klogTail = p.klogTail[len(p.klogTail)-KlogTailLines:]
	}
}

func (p *KtapParser) openPhase() *EvPhaseStart {
	p.phaseIdx++
	p.phaseName = phaseNameFor(p.phaseIdx)
	p.pendingSubtests = nil
	p.currentRecord = nil
	p.logLines = nil
	p.logBlockOpened = false
	p.state = stateOutside
	p.tapVersionSeen = true
	return &EvPhaseStart{PhaseIdx: p.phaseIdx, Name: p.phaseName}
}

func (p *KtapParser) commitCurrentRecord() *EvTest {
	if p.currentRecord == nil {
		return nil
	}
	rec := p.currentRecord
	if p.logBlockOpened {
		joined := strings.Join(p.logLines, "\n")
		rec.Log = &joined
	}
	if rec.Outcome.IsFailure() && (rec.Log == nil || *rec.Log == "") && len(p.klogTail) > 0 {
		rec.PreFailKlogTail = append(rec.PreFailKlogTail, p.klogTail...)
	}
	if len(p.pendingSubtests) > 0 {
		rec.Subtests = append(rec.Subtests, p.pendingSubtests...)
		p.pendingSubtests = nil
	}
	p.currentRecord = nil
	p.logLines = nil
	p.logBlockOpened = false
	p.state = stateOutside
	return &EvTest{Record: rec}
}

// feedKtap dispatches one already-stripped (no `KTAP\t` prefix) line.
func (p *KtapParser) feedKtap(body string) []Event {
	if p.state == stateInLogLiteral {
		if body == "  ..." {
			if ev := p.commitCurrentRecord(); ev != nil {
				return []Event{ev}
			}
			return nil
		}
		// The kernel emits literal log content as `KTAP\t   <text>`;
		// anything else survives as-is.
		if strings.HasPrefix(body, "   ") {
			p.logLines = append(p.logLines, body[3:])
		} else {
			p.logLines = append(p.logLines, body)
		}
		return nil
	}

	if body == "TAP version 14" {
		var events []Event
		if p.currentRecord != nil {
			if ev := p.commitCurrentRecord(); ev != nil {
				events = append(events, ev)
			}
		}
		events = append(events, p.openPhase())
		return events
	}

	if m := planRE.FindStringSubmatch(body); m != nil {
		var events []Event
		if p.phaseIdx == 0 || !p.tapVersionSeen {
			events = append(events, p.openPhase())
		}
		n, _ := strconv.Atoi(m[1])
		events = append(events, &EvPlan{PhaseIdx: p.phaseIdx, N: n})
		return events
	}

	if m := footerRE.FindStringSubmatch(body); m != nil {
		var events []Event
		if p.currentRecord != nil {
			if ev := p.commitCurrentRecord(); ev != nil {
				events = append(events, ev)
			}
		}
		ms, _ := strconv.Atoi(m[1])
		passN, _ := strconv.Atoi(m[2])
		failN, _ := strconv.Atoi(m[3])
		skipN, _ := strconv.Atoi(m[4])
		overN, _ := strconv.Atoi(m[5])
		events = append(events, &EvPhaseEnd{
			PhaseIdx:  p.phaseIdx,
			Name:      p.phaseName,
			ElapsedMs: ms,
			Pass:      passN,
			Fail:      failN,
			Skip:      skipN,
			OverTime:  overN,
		})
		p.tapVersionSeen = false
		return events
	}

	if strings.HasPrefix(body, BailKey) {
		reason := strings.TrimSpace(body[len(BailKey):])
		var events []Event
		if p.currentRecord != nil {
			if ev := p.commitCurrentRecord(); ev != nil {
				events = append(events, ev)
			}
		}
		events = append(events, &EvBail{PhaseIdx: p.phaseIdx, Reason: reason})
		return events
	}

	if m := topResultRE.FindStringSubmatch(body); m != nil {
		var events []Event
		if p.currentRecord != nil {
			if ev := p.commitCurrentRecord(); ev != nil {
				events = append(events, ev)
			}
		}
		status, idxStr, name := m[1], m[2], m[3]
		suffix := ""
		if len(m) >= 5 {
			suffix = m[4]
		}
		idx, _ := strconv.Atoi(idxStr)
		rec := recordFromTopResult(p.phaseIdx, p.phaseName, idx, name, status, suffix)
		if len(p.pendingSubtests) > 0 {
			rec.Subtests = append([]Subtest{}, p.pendingSubtests...)
			p.pendingSubtests = nil
		}
		p.currentRecord = rec
		// Don't commit yet: a `---` block may follow, and the next
		// top-level event commits this record.
		return events
	}

	if m := subtestRE.FindStringSubmatch(body); m != nil {
		p.pendingSubtests = append(p.pendingSubtests, subtestFromMatch(m))
		return nil
	}

	switch body {
	case "  ---":
		p.state = stateInDiag
		return nil
	case "  ...":
		if ev := p.commitCurrentRecord(); ev != nil {
			return []Event{ev}
		}
		return nil
	}

	if p.state == stateInDiag {
		if m := diagFieldRE.FindStringSubmatch(body); m != nil && p.currentRecord != nil {
			key, val := m[1], m[2]
			switch key {
			case "outcome":
				v := strings.TrimSpace(val)
				p.currentRecord.FailOutcomeKind = &v
			case "file":
				v := strings.TrimSpace(val)
				p.currentRecord.FailFile = &v
			case "log":
				p.state = stateInLogLiteral
				p.logBlockOpened = true
			}
			return nil
		}
	}

	// Unrecognised KTAP line — silently survive.
	return nil
}

func recordFromTopResult(
	phaseIdx int, phaseName string,
	idx int, name, status, suffix string,
) *TestRecord {
	rec := &TestRecord{
		PhaseIdx:  phaseIdx,
		PhaseName: phaseName,
		Idx:       idx,
		Name:      name,
		Subtests:  []Subtest{},
	}
	if strings.Contains(suffix, " OVER_TIME") {
		rec.OverTime = true
	}
	if strings.Contains(suffix, " EXPECTED_PANIC") {
		rec.ExpectedPanic = true
	}
	if m := skipRE.FindStringSubmatch(suffix); m != nil {
		rec.Outcome = OutcomeSkip
		reason := ""
		if len(m) >= 2 {
			reason = strings.TrimSpace(m[1])
		}
		rec.SkipReason = &reason
		return rec
	}
	if m := timeMsRE.FindStringSubmatch(suffix); m != nil {
		t, _ := strconv.Atoi(m[1])
		rec.TimeMs = &t
	}
	if status == "not ok" {
		rec.Outcome = OutcomeFail
	} else {
		rec.Outcome = OutcomePass
	}
	return rec
}

// subtestFromMatch decodes a `subtestRE` match; skips are encoded as
// `ok M - name # SKIP`.
func subtestFromMatch(m []string) Subtest {
	status, idxStr, name := m[1], m[2], m[3]
	suffix := ""
	if len(m) >= 5 {
		suffix = m[4]
	}
	idx, _ := strconv.Atoi(idxStr)
	st := Subtest{Idx: idx, Name: strings.TrimRight(name, " ")}
	if status == "not ok" {
		st.Outcome = OutcomeFail
		st.Msg = strings.TrimSpace(suffix)
		return st
	}
	if skipRE.MatchString(suffix) {
		st.Outcome = OutcomeSkip
		return st
	}
	st.Outcome = OutcomePass
	return st
}
