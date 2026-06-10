package main

import (
	"fmt"
	"io"
	"sort"
	"strings"
	"time"
)

// BarRenderer is the default UX: a single live progress bar pinned to
// the bottom of the output. Fail / skip / over-time test events scroll
// into history above the bar as one-line warnings. When a phase ends
// the bar is replaced in place by that phase's one-line summary, and a
// fresh bar appears for the next phase. After all phases finish the
// bar is erased and the post-run sections (failure detail blocks,
// slow-tests roundup, summary line) print without it.
//
// On non-TTY output (CI logs, pipes, redirects) we suppress the bar
// entirely and print only the streaming warnings + per-phase summary
// + post-run sections.
type BarRenderer struct {
	Out          io.Writer
	Verbosity    string
	Colour       bool
	WarnMs       int
	TTY          bool
	TerminalCols int

	testsInPhase    int
	phaseStarted    time.Time
	currentPhase    *PhaseRecord
	failedInPhase   int
	skippedInPhase  int
	overTimeInPhase int
	barActive       bool
}

// NewBarRenderer creates a renderer bound to a Writer. `tty` and
// `cols` are passed in (rather than re-detected from the Writer) so
// tests can fake them without involving a real terminal.
func NewBarRenderer(out io.Writer, verbosity string, colour bool, warnMs int, tty bool, cols int) *BarRenderer {
	return &BarRenderer{
		Out:          out,
		Verbosity:    verbosity,
		Colour:       colour,
		WarnMs:       warnMs,
		TTY:          tty,
		TerminalCols: cols,
	}
}

// OnEvent dispatches one parser event to its renderer handler.
func (r *BarRenderer) OnEvent(ev Event, summary *RunSummary) {
	switch e := ev.(type) {
	case *EvPhaseStart:
		r.beginPhase(e, summary)
	case *EvPlan:
		r.onPlan(e, summary)
	case *EvTest:
		r.onTest(e.Record, summary)
	case *EvPhaseEnd:
		r.endPhase(e, summary)
	case *EvBail:
		r.onBail(e)
	}
	// EvNonKtap ignored.
}

func (r *BarRenderer) beginPhase(ev *EvPhaseStart, summary *RunSummary) {
	r.testsInPhase = 0
	r.failedInPhase = 0
	r.skippedInPhase = 0
	r.overTimeInPhase = 0
	r.phaseStarted = time.Now()
	r.barActive = false
	r.currentPhase = nil
	for _, p := range summary.Phases {
		if p.Idx == ev.PhaseIdx {
			r.currentPhase = p
			break
		}
	}
	r.drawBar()
}

func (r *BarRenderer) onPlan(_ *EvPlan, _ *RunSummary) {
	// Plan number is now visible in the bar — redraw to pick up the
	// `1/N` denominator immediately.
	r.drawBar()
}

func (r *BarRenderer) onTest(rec *TestRecord, _ *RunSummary) {
	if r.Verbosity == "quiet" && rec.Outcome == OutcomePass {
		return
	}
	r.testsInPhase++
	switch rec.Outcome {
	case OutcomeFail:
		r.failedInPhase++
	case OutcomeSkip:
		r.skippedInPhase++
	}
	if rec.OverTime {
		r.overTimeInPhase++
	}

	switch {
	case rec.Outcome == OutcomeFail:
		r.printAboveBar(r.formatWarningLine(rec, "F", ansiRedBold, "FAIL"))
	case rec.Outcome == OutcomeSkip:
		r.printAboveBar(r.formatWarningLine(rec, "s", ansiGrey, "SKIP"))
	case rec.OverTime:
		r.printAboveBar(r.formatWarningLine(rec, "o", ansiYellow, "SLOW"))
	default:
		r.drawBar()
	}
}

func (r *BarRenderer) onBail(ev *EvBail) {
	r.eraseBar()
	r.println(Paint(
		fmt.Sprintf("BAIL OUT in %s: %s", phaseNameFor(ev.PhaseIdx), ev.Reason),
		ansiRedBold, r.Colour,
	))
}

func (r *BarRenderer) endPhase(ev *EvPhaseEnd, _ *RunSummary) {
	r.eraseBar()
	secs := float64(ev.ElapsedMs) / 1000.0

	passColour := ansiGreen
	if ev.Fail > 0 {
		passColour = ansiDim
	}
	failColour := ansiDim
	if ev.Fail > 0 {
		failColour = ansiRedBold
	}
	skipColour := ansiDim
	if ev.Skip > 0 {
		skipColour = ansiYellow
	}
	overColour := ansiDim
	if ev.OverTime > 0 {
		overColour = ansiYellow
	}
	parts := []string{
		Paint(fmt.Sprintf("%d pass", ev.Pass), passColour, r.Colour),
		Paint(fmt.Sprintf("%d fail", ev.Fail), failColour, r.Colour),
		Paint(fmt.Sprintf("%d skip", ev.Skip), skipColour, r.Colour),
		Paint(fmt.Sprintf("%d over-time", ev.OverTime), overColour, r.Colour),
	}
	marker := Paint("✓", ansiGreen, r.Colour)
	if ev.Fail > 0 {
		marker = Paint("✗", ansiRedBold, r.Colour)
	}
	if !r.Colour {
		marker = "OK"
		if ev.Fail > 0 {
			marker = "FAIL"
		}
	}
	r.println(fmt.Sprintf("%s %s: %s in %.2fs", marker, ev.Name, strings.Join(parts, ", "), secs))
}

// Finalize emits all post-run sections: failure blocks, optional verbose
// pass-logs, slow-tests roundup, summary line. Always runs on a clean
// line — `eraseBar` + blank line first.
func (r *BarRenderer) Finalize(summary *RunSummary) {
	r.eraseBar()
	r.println("")
	r.renderFailures(summary)
	if r.Verbosity == "verbose" {
		r.renderVerbosePassLogs(summary)
	}
	if r.WarnMs > 0 {
		r.renderSlowTests(summary)
	}
	r.renderSummary(summary)
}

// ---------------------------------------------------------------------
//  Bar machinery
// ---------------------------------------------------------------------

func (r *BarRenderer) drawBar() {
	if !r.TTY {
		return
	}
	bar := r.formatBar()
	fmt.Fprint(r.Out, ansiCR+ansiEraseToEnd+bar)
	r.barActive = true
}

func (r *BarRenderer) eraseBar() {
	if !r.TTY || !r.barActive {
		r.barActive = false
		return
	}
	fmt.Fprint(r.Out, ansiCR+ansiEraseToEnd)
	r.barActive = false
}

func (r *BarRenderer) printAboveBar(line string) {
	if r.TTY {
		r.eraseBar()
	}
	fmt.Fprintln(r.Out, line)
	if r.TTY {
		r.drawBar()
	}
}

func (r *BarRenderer) formatWarningLine(rec *TestRecord, marker, colour, label string) string {
	head := fmt.Sprintf(
		"  %s %s %d %s",
		Paint(marker, colour, r.Colour),
		Paint(label, colour, r.Colour),
		rec.Idx, rec.Name,
	)
	if rec.TimeMs != nil {
		head += fmt.Sprintf("  %dms", *rec.TimeMs)
	}
	if rec.Outcome == OutcomeSkip && rec.SkipReason != nil && *rec.SkipReason != "" {
		head += "  (" + *rec.SkipReason + ")"
	}
	return head
}

func (r *BarRenderer) formatBar() string {
	phaseName := "?"
	var planN *int
	if r.currentPhase != nil {
		phaseName = r.currentPhase.Name
		planN = r.currentPhase.PlanN
	}
	nDone := r.testsInPhase
	elapsed := time.Since(r.phaseStarted)

	prefixText := fmt.Sprintf("%8s  ", phaseName)
	prefix := Paint(fmt.Sprintf("%8s", phaseName), ansiBold, r.Colour) + "  "

	var countStr string
	if planN != nil && *planN > 0 {
		w := len(fmt.Sprintf("%d", *planN))
		countStr = fmt.Sprintf("%*d/%d", w, nDone, *planN)
	} else {
		countStr = fmt.Sprintf("%d/?", nDone)
	}

	etaStr := ""
	if planN != nil && *planN > 0 && nDone >= 5 && nDone < *planN {
		var avg time.Duration
		if nDone > 0 {
			avg = elapsed / time.Duration(nDone)
		}
		remaining := time.Duration(*planN-nDone) * avg
		etaStr = "ETA " + fmtSecs(remaining.Seconds())
	}

	parts := []string{countStr, fmt.Sprintf("%5.1fs", elapsed.Seconds())}
	if etaStr != "" {
		parts = append(parts, etaStr)
	}
	if r.failedInPhase > 0 {
		parts = append(parts, Paint(
			fmt.Sprintf("%d fail", r.failedInPhase), ansiRedBold, r.Colour))
	}
	if r.skippedInPhase > 0 {
		parts = append(parts, Paint(
			fmt.Sprintf("%d skip", r.skippedInPhase), ansiYellow, r.Colour))
	}
	if r.overTimeInPhase > 0 {
		parts = append(parts, Paint(
			fmt.Sprintf("%d slow", r.overTimeInPhase), ansiYellow, r.Colour))
	}
	suffix := strings.Join(parts, "  ")

	cols := r.TerminalCols
	if cols < 60 {
		cols = 60
	}
	barWidth := cols - VisibleLen(prefixText) - 1 - VisibleLen(suffix) - 2
	if barWidth < 10 {
		barWidth = 10
	}

	filled := 0
	if planN != nil && *planN > 0 {
		filled = (nDone * barWidth) / *planN
		if filled > barWidth {
			filled = barWidth
		}
	}

	fillColour := ansiGreen
	switch {
	case r.failedInPhase > 0:
		fillColour = ansiRed
	case r.overTimeInPhase > 0 || r.skippedInPhase > 0:
		fillColour = ansiYellow
	}

	body := strings.Repeat(barFill, filled) + strings.Repeat(barEmpty, barWidth-filled)
	if r.Colour && filled > 0 {
		body = Paint(strings.Repeat(barFill, filled), fillColour, true) +
			Paint(strings.Repeat(barEmpty, barWidth-filled), ansiGrey, true)
	}

	return prefix + body + "  " + suffix
}

// ---------------------------------------------------------------------
//  Post-run sections
// ---------------------------------------------------------------------

func (r *BarRenderer) renderFailures(summary *RunSummary) {
	failures := summary.Failures()
	if len(failures) == 0 {
		return
	}
	r.println(Paint(strings.Repeat("=", 76), ansiRed, r.Colour))
	for k, rec := range failures {
		r.renderFailureBlock(rec, k+1, len(failures))
	}
}

func (r *BarRenderer) renderFailureBlock(rec *TestRecord, k, total int) {
	header := fmt.Sprintf("==== FAILURE %d of %d — %s ====", k, total, rec.Name)
	r.println(Paint(header, ansiRedBold, r.Colour))
	r.println("  phase:    " + rec.PhaseName)
	r.println(fmt.Sprintf("  idx:      %d", rec.Idx))
	if rec.FailFile != nil {
		r.println("  file:     " + *rec.FailFile)
	}
	if rec.FailOutcomeKind != nil {
		r.println("  outcome:  " + *rec.FailOutcomeKind)
	} else {
		r.println("  outcome:  Fail")
	}
	if rec.TimeMs != nil {
		r.println(fmt.Sprintf("  time_ms:  %d", *rec.TimeMs))
	}
	if len(rec.Subtests) > 0 {
		r.println("  subtests:")
		for _, st := range rec.Subtests {
			marker := r.subtestMarker(st)
			line := fmt.Sprintf("    %s %d %s", marker, st.Idx, st.Name)
			if st.Msg != "" {
				line += " — " + st.Msg
			}
			r.println(line)
		}
	}
	switch {
	case rec.Log != nil && *rec.Log != "":
		r.println("  log:")
		for _, ln := range strings.Split(*rec.Log, "\n") {
			r.println("    " + ln)
		}
	case len(rec.PreFailKlogTail) > 0:
		r.println("  klog tail (no per-test capture):")
		for _, ln := range rec.PreFailKlogTail {
			r.println("    " + ln)
		}
	default:
		r.println("  log: (empty — kernel reported no captured klog)")
	}
	r.println("")
}

func (r *BarRenderer) subtestMarker(st Subtest) string {
	if !r.Colour {
		switch st.Outcome {
		case OutcomePass:
			return "PASS"
		case OutcomeSkip:
			return "SKIP"
		default:
			return "FAIL"
		}
	}
	switch st.Outcome {
	case OutcomePass:
		return Paint("✓", ansiGreen, true)
	case OutcomeSkip:
		return Paint("◦", ansiGrey, true)
	default:
		return Paint("✗", ansiRedBold, true)
	}
}

func (r *BarRenderer) renderVerbosePassLogs(summary *RunSummary) {
	var passes []*TestRecord
	for _, t := range summary.AllTests() {
		if t.Outcome == OutcomePass && t.Log != nil && *t.Log != "" {
			passes = append(passes, t)
		}
	}
	if len(passes) == 0 {
		return
	}
	r.println(Paint("---- verbose: per-test logs ----", ansiDim, r.Colour))
	for _, rec := range passes {
		r.println(Paint(
			fmt.Sprintf("# %s (%s)", rec.Name, fmtTimeMs(rec.TimeMs)),
			ansiDim, r.Colour,
		))
		for _, ln := range strings.Split(*rec.Log, "\n") {
			r.println("  " + ln)
		}
	}
	r.println("")
}

func (r *BarRenderer) renderSlowTests(summary *RunSummary) {
	var slow []*TestRecord
	for _, t := range summary.AllTests() {
		if t.TimeMs != nil && *t.TimeMs >= r.WarnMs {
			slow = append(slow, t)
		}
	}
	sort.SliceStable(slow, func(i, j int) bool {
		return *slow[i].TimeMs > *slow[j].TimeMs
	})
	if len(slow) == 0 {
		return
	}
	const slowTopN = 10
	if len(slow) > slowTopN {
		slow = slow[:slowTopN]
	}
	r.println(Paint(
		fmt.Sprintf("slow tests (>= %dms):", r.WarnMs),
		ansiYellow, r.Colour,
	))
	for _, rec := range slow {
		mark := " "
		if rec.OverTime {
			mark = Paint("o", ansiYellow, r.Colour)
		}
		r.println(fmt.Sprintf("  %s %dms  %s", mark, *rec.TimeMs, rec.Name))
	}
	r.println("")
}

func (r *BarRenderer) renderSummary(summary *RunSummary) {
	total := summary.Total()
	passed := summary.CounterSum("pass")
	failed := summary.CounterSum("fail")
	skipped := summary.CounterSum("skip")
	overTime := summary.CounterSum("over_time")
	wallS := float64(summary.WallMs()) / 1000.0
	nPhases := len(summary.Phases)
	plural := "s"
	if nPhases == 1 {
		plural = ""
	}

	r.println(Paint(strings.Repeat("─", 76), ansiDim, r.Colour))

	// On any abort path (timeout / silence / truncation / user interrupt)
	// surface the parser's klog tail so CI failures aren't a context-free
	// "TIMED OUT" banner. With summary verbosity (the CI default) klog
	// is otherwise suppressed entirely, leaving us blind to where the
	// kernel was when it wedged.
	if (summary.TimedOut || summary.UserAborted || summary.Truncated || summary.HarnessError != nil) &&
		len(summary.AbortKlogTail) > 0 {
		r.println(Paint(
			fmt.Sprintf("klog tail (last %d non-KTAP lines before abort):",
				len(summary.AbortKlogTail)),
			ansiDim, r.Colour,
		))
		for _, ln := range summary.AbortKlogTail {
			r.println(Paint("  "+ln, ansiDim, r.Colour))
		}
		r.println("")
	}

	bailed := false
	var bailedPhase *PhaseRecord
	for _, p := range summary.Phases {
		if p.BailReason != nil {
			bailed = true
			bailedPhase = p
			break
		}
	}

	switch {
	case bailed:
		msg := fmt.Sprintf(
			"BAIL OUT in phase %d (%s): %s  →  %d failed before bail, "+
				"remaining tests in phase not run",
			bailedPhase.Idx, bailedPhase.Name, *bailedPhase.BailReason, failed,
		)
		r.println(Paint(msg, ansiRedBold, r.Colour))
	case summary.TimedOut:
		var msg string
		if summary.SilenceHit {
			msg = fmt.Sprintf(
				"NO OUTPUT — aborted after %.1fs of QEMU silence (%d tests observed)",
				wallS, total,
			)
		} else {
			msg = fmt.Sprintf("TIMED OUT after %.1fs (%d tests observed)", wallS, total)
		}
		r.println(Paint(msg, ansiRedBold, r.Colour))
	case summary.UserAborted:
		r.println(Paint(
			fmt.Sprintf("INTERRUPTED after %.1fs (%d tests observed)", wallS, total),
			ansiYellow, r.Colour,
		))
	case summary.Truncated:
		kernelClaimed := summary.PlannedTotal()
		r.println(Paint(
			fmt.Sprintf(
				"TRUNCATED: kernel plan promised %d tests but the harness "+
					"only emitted %d before the stream ended (%d missing — "+
					"kernel likely hung mid-phase or panicked silently)",
				kernelClaimed, total, kernelClaimed-total),
			ansiYellow, r.Colour,
		))
		r.println(Paint(
			fmt.Sprintf("%d tests across %d phase%s  →  "+
				"%d passed, %d failed, %d skipped, %d over-time",
				total, nPhases, plural, passed, failed, skipped, overTime),
			ansiYellow, r.Colour,
		))
	case summary.HarnessError != nil:
		r.println(Paint(*summary.HarnessError, ansiRedBold, r.Colour))
	case failed == 0 && !summary.Truncated:
		r.println(Paint(
			fmt.Sprintf("%d tests across %d phase%s  →  "+
				"%d passed, 0 failed, %d skipped, %d over-time",
				total, nPhases, plural, passed, skipped, overTime),
			ansiGreen, r.Colour,
		))
	default:
		r.println(Paint(
			fmt.Sprintf("%d tests across %d phase%s  →  "+
				"%d passed, %d failed, %d skipped, %d over-time",
				total, nPhases, plural, passed, failed, skipped, overTime),
			ansiRedBold, r.Colour,
		))
	}
	r.println(fmt.Sprintf("real time: %.2fs", wallS))
}

// ---------------------------------------------------------------------
//  Helpers
// ---------------------------------------------------------------------

func (r *BarRenderer) println(line string) {
	fmt.Fprintln(r.Out, line)
}

func fmtSecs(secs float64) string {
	if secs < 60 {
		return fmt.Sprintf("%4.1fs", secs)
	}
	m := int(secs / 60)
	s := int(secs - float64(m)*60)
	return fmt.Sprintf("%d:%02d", m, s)
}

func fmtTimeMs(ms *int) string {
	if ms == nil {
		return "?ms"
	}
	return fmt.Sprintf("%dms", *ms)
}
