package main

import (
	"strconv"
	"strings"
)

// Event is the sealed interface every parser-emitted event implements.
// The unexported `event()` method prevents external packages from
// satisfying it (Go's idiomatic equivalent of a Rust enum).
type Event interface {
	event()
}

// EvPhaseStart fires the first time a phase's `KTAP\tTAP version 14`
// arrives. The renderer uses it to clear per-phase counters and re-draw
// the bar.
type EvPhaseStart struct {
	PhaseIdx int
	Name     string
}

// EvPlan carries the `1..N` plan number for the phase that's currently
// open. The bar's denominator and ETA derivation depend on it.
type EvPlan struct {
	PhaseIdx int
	N        int
}

// EvTest is the parser's commit of one fully-formed test result, after
// any pending diag block (for fails) and any subtests (for utests) have
// been attached. The recorder routes the embedded TestRecord onto the
// active phase.
type EvTest struct {
	Record *TestRecord
}

// EvPhaseEnd carries the kernel's footer counters for a phase. The
// renderer replaces the live bar with a one-line summary at this point.
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

// EvNonKtap surfaces every line that isn't part of the harness wire
// format. The raw renderer mirrors them; the bar renderer ignores them
// (keeping a rolling tail for failure-context attachment).
type EvNonKtap struct {
	Line string
}

func (*EvPhaseStart) event() {}
func (*EvPlan) event()       {}
func (*EvTest) event()       {}
func (*EvPhaseEnd) event()   {}
func (*EvBail) event()       {}
func (*EvNonKtap) event()    {}

// parserState is the FSM state. The diag-vs-log split exists because once
// a `log: |` line opens a YAML literal block, the only thing that closes
// it is `KTAP\t  ...` — `KTAP\t…` substrings inside the captured klog
// must be treated as content, not as new top-level lines.
type parserState int

const (
	stateOutside parserState = iota
	stateInDiag
	stateInLogLiteral
)

// PhaseNames maps phase ordinal to a human-readable label. The kernel
// emits `TESTS: Starting kernel phase` / `… userland phase` as klog
// (non-KTAP) hints, so we don't extract them from the wire — we match
// the prose by ordinal convention (phase 1 = kernel, phase 2 = userland).
var PhaseNames = []string{"kernel", "userland"}

// phaseNameFor maps a 1-based phase index to its conventional label,
// falling back to `phaseN` for any unexpected ordinal.
func phaseNameFor(idx int) string {
	if idx >= 1 && idx <= len(PhaseNames) {
		return PhaseNames[idx-1]
	}
	return "phase" + strconv.Itoa(idx)
}

// KtapParser is a streaming line-by-line parser of the kernel's KTAP
// output. Pure FSM — one instance per run. Goroutine-unsafe; intended
// to be driven from the same loop that reads from QEMU's stdout pipe.
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

// KlogTail returns a copy of the rolling window of recent non-KTAP klog
// lines. Used by the timeout / silence / truncation paths to surface
// what the kernel was doing right before the wedge — invaluable when
// the wrapper aborts a run that never reached an orderly footer.
func (p *KtapParser) KlogTail() []string {
	if len(p.klogTail) == 0 {
		return nil
	}
	out := make([]string, len(p.klogTail))
	copy(out, p.klogTail)
	return out
}

// Feed consumes one input line and returns zero or more events. Each
// returned event is fully owned by the caller; the parser keeps no
// reference to it after the function returns.
//
// The line should NOT include the trailing newline.
func (p *KtapParser) Feed(rawLine string) []Event {
	line := stripANSI(rawLine)

	// Leading-garbage rescue. TTY-layer regression tests (drivers/tty_tests)
	// write fixture strings ("hello", "abc", XON/XOFF) directly to COM1 via
	// the TTY hardware backend, bypassing klog. The next emit_ok then arrives
	// with those bytes prefixed: `helloKTAP\tok 994 - test_tcoon_resumes_write…`.
	// Without this we'd classify the line as klog noise and the truncation
	// guard would mis-diagnose the kernel as dropping output. Only rescue
	// when we're not currently inside a `log: |` literal — there a `KTAP\t`
	// substring is genuine captured-log content, not a result line.
	if p.state == stateOutside &&
		!strings.HasPrefix(line, KtapPrefix) &&
		strings.Contains(line, KtapPrefix) {
		line = line[strings.Index(line, KtapPrefix):]
	}

	if !strings.HasPrefix(line, KtapPrefix) {
		// A non-KTAP line that arrives while we're collecting a captured
		// log literal means the kernel got interrupted (e.g., a panic on
		// another CPU writing plain klog). Close the current record's log
		// gracefully and surface the line.
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

// appendKlogTail keeps a bounded rolling window of recent non-KTAP klog
// lines. When the next failure arrives without a captured log block (rare:
// kernel hard-panic before orderly emit can flush) we attach this tail
// for context.
func (p *KtapParser) appendKlogTail(line string) {
	p.klogTail = append(p.klogTail, line)
	if len(p.klogTail) > KlogTailLines {
		p.klogTail = p.klogTail[len(p.klogTail)-KlogTailLines:]
	}
}

// openPhase advances the phase counter, resets per-phase parser state,
// and emits an EvPhaseStart for the new phase.
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

// commitCurrentRecord finalises whatever pending TestRecord exists,
// attaches accumulated subtests / log content / klog tail, and emits a
// commit event. Returns nil if there's nothing to commit.
func (p *KtapParser) commitCurrentRecord() *EvTest {
	if p.currentRecord == nil {
		return nil
	}
	rec := p.currentRecord
	if p.logBlockOpened {
		joined := strings.Join(p.logLines, "\n")
		rec.Log = &joined
	}
	// If a failure has no useful captured log, attach the klog tail so the
	// failure block shows the recent non-KTAP context (covers kernel
	// hard-panic before orderly emit could flush).
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
	// In-log-literal state: only `  ...` exits; everything else is content.
	if p.state == stateInLogLiteral {
		if body == "  ..." {
			if ev := p.commitCurrentRecord(); ev != nil {
				return []Event{ev}
			}
			return nil
		}
		// 3-space indent denotes literal log content (kernel emits with
		// `KTAP\t   <text>`). Strip the 3 indent spaces; anything else
		// (malformed-but-survive) goes in as-is.
		if strings.HasPrefix(body, "   ") {
			p.logLines = append(p.logLines, body[3:])
		} else {
			p.logLines = append(p.logLines, body)
		}
		return nil
	}

	// Header.
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

	// Plan.
	if m := planRE.FindStringSubmatch(body); m != nil {
		var events []Event
		if p.phaseIdx == 0 || !p.tapVersionSeen {
			events = append(events, p.openPhase())
		}
		n, _ := strconv.Atoi(m[1])
		events = append(events, &EvPlan{PhaseIdx: p.phaseIdx, N: n})
		return events
	}

	// Footer.
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

	// Bail.
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

	// Top-level result. Commits any prior pending diag/log block first.
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
		// Don't commit yet: a `---` block (verbose pass log or fail diag)
		// may follow. The next top-level event commits this record.
		return events
	}

	// Subtest (2-space indent + ok/not ok).
	if m := subtestRE.FindStringSubmatch(body); m != nil {
		p.pendingSubtests = append(p.pendingSubtests, subtestFromMatch(m))
		return nil
	}

	// Diag-block delimiters / fields.
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

// recordFromTopResult builds a TestRecord from a parsed top-level result
// line's components. Outcome / OverTime / ExpectedPanic / SkipReason /
// TimeMs are all decoded from the suffix.
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

// subtestFromMatch decodes a regex match group from `subtestRE` into a
// Subtest record. Skips are encoded as `ok M - name # SKIP`.
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
