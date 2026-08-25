package main

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"strings"
	"sync/atomic"
	"syscall"
	"time"
)

// QemuDriver wraps `scripts/qemu_run.sh test <iso> <fs_image>`. Exit-code
// semantics: 0 = kernel wrote 0 to isa-debug-exit (tests passed); 1 = kernel
// wrote 1 (tests failed); other = kernel didn't reach exit (hang, panic
// loop, hard fault).
type QemuDriver struct {
	RepoRoot       string
	Iso            string
	FsImage        string
	WallTimeoutSec float64
	// SilenceSec aborts the run when the QEMU stdout pipe stays silent for
	// this many seconds; 0 disables. Catches inter-phase wedges that would
	// otherwise burn the full WallTimeoutSec.
	SilenceSec float64

	// silenceLimitNs is the live silence budget the watchdog re-reads every
	// tick. It starts at SilenceSec and only TightenSilence moves it.
	silenceLimitNs atomic.Int64
}

// TightenSilence lowers the live silence budget, never raises it.
//
// A no-op when --silence-secs=0: no watchdog goroutine was ever started, so
// there is no budget to tighten and the run keeps the full wall timeout. That
// is deliberate — --silence-secs=0 means "do not abort on silence", and a
// kernel abort does not override the operator on that.
func (d *QemuDriver) TightenSilence(limit time.Duration) {
	if d.SilenceSec <= 0 || limit <= 0 {
		return
	}
	for {
		cur := d.silenceLimitNs.Load()
		if cur != 0 && cur <= int64(limit) {
			return
		}
		if d.silenceLimitNs.CompareAndSwap(cur, int64(limit)) {
			return
		}
	}
}

// DriverResult is what `Run` returns to main.
type DriverResult struct {
	QemuStatus  *int // exit status from qemu_run.sh; nil if process never finished
	UserAborted bool // SIGINT received from the user
	TimedOut    bool // wall-clock guard or silence watchdog fired
	SilenceHit  bool // silence watchdog (not wall-clock) was the trigger
}

// silenceTickInterval is how often the watchdog re-reads the live budget.
//
// Fixed, not derived from SilenceSec: at the 120 s default a SilenceSec/4 tick
// is 30 s, and a budget TightenSilence lowered to 20 s would not be read until
// after it had already elapsed. The limit and the interval are both captured
// state, so lowering only the limit shortens nothing.
const silenceTickInterval = time.Second

// watchSilence trips `tripped` the first time the gap since `lastLine` reaches
// the live budget, then returns. Split out of Run, and taking its tick as a
// channel rather than owning a Ticker, so a test can step it one tick at a
// time and observe that the budget is re-read on every one.
func (d *QemuDriver) watchSilence(
	ctx context.Context, tick <-chan time.Time,
	lastLine *atomic.Int64, tripped chan<- struct{},
) {
	for {
		select {
		case <-ctx.Done():
			return
		case <-tick:
			limit := time.Duration(d.silenceLimitNs.Load())
			last := time.Unix(0, lastLine.Load())
			if time.Since(last) >= limit {
				select {
				case tripped <- struct{}{}:
				default:
				}
				return
			}
		}
	}
}

// Run streams QEMU's stdout line-by-line through `onLine`, enforcing the
// configured wall timeout and SIGINT (Ctrl-C) handling.
func (d *QemuDriver) Run(ctx context.Context, onLine func(string)) (DriverResult, error) {
	cmd := exec.Command(
		d.RepoRoot+"/scripts/qemu_run.sh", "test", d.Iso, d.FsImage,
	)
	// qemu_run.sh and its callees honour QEMU_BIN/QEMU_SMP/etc.
	cmd.Env = os.Environ()
	cmd.Stderr = os.Stderr

	// Detach from any inherited TTY stdin so QEMU doesn't try to read
	// keystrokes back into the guest's COM1 RX.
	null, err := os.Open(os.DevNull)
	if err != nil {
		return DriverResult{}, fmt.Errorf("driver: open /dev/null: %w", err)
	}
	defer null.Close()
	cmd.Stdin = null

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return DriverResult{}, fmt.Errorf("driver: stdout pipe: %w", err)
	}

	// Own process group so cancel signals the whole tree: signalling the
	// qemu_run.sh bash script alone leaves QEMU orphaned, holding the stdout
	// pipe open forever and blocking the scanner.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}

	// SIGINT fires the parent context.
	runCtx := ctx
	var runCancel context.CancelFunc
	if d.WallTimeoutSec > 0 {
		runCtx, runCancel = context.WithTimeout(
			ctx, time.Duration(d.WallTimeoutSec*float64(time.Second)),
		)
		defer runCancel()
	}

	if err := cmd.Start(); err != nil {
		return DriverResult{}, fmt.Errorf("driver: start: %w", err)
	}
	pgid := cmd.Process.Pid // matches the child's PID since Setpgid+no Pgid set

	// Single goroutine owns cmd.Wait.
	waitCh := make(chan error, 1)
	go func() { waitCh <- cmd.Wait() }()

	// lastLine is the monotonic time of the last byte received from QEMU.
	var lastLine atomic.Int64
	lastLine.Store(time.Now().UnixNano())
	silenceCtx, silenceCancel := context.WithCancel(context.Background())
	defer silenceCancel()
	silenceTripped := make(chan struct{}, 1)
	if d.SilenceSec > 0 {
		d.silenceLimitNs.Store(int64(d.SilenceSec * float64(time.Second)))
		ticker := time.NewTicker(silenceTickInterval)
		defer ticker.Stop()
		go d.watchSilence(silenceCtx, ticker.C, &lastLine, silenceTripped)
	}

	// Killing the process group closes QEMU's stdout, which is what lets the
	// scanner loop below return.
	type abortFlags struct {
		userAborted bool
		timedOut    bool
		silenceHit  bool
	}
	abortCh := make(chan abortFlags, 1)
	stopWatcher := make(chan struct{})
	go func() {
		af := abortFlags{}
		select {
		case <-runCtx.Done():
			if errors.Is(runCtx.Err(), context.DeadlineExceeded) {
				af.timedOut = true
			} else {
				af.userAborted = true
			}
		case <-silenceTripped:
			af.timedOut = true
			af.silenceHit = true
		case <-stopWatcher:
			abortCh <- af
			return
		}
		_ = syscall.Kill(-pgid, syscall.SIGTERM)
		grace := time.NewTimer(5 * time.Second)
		defer grace.Stop()
		select {
		case <-grace.C:
			_ = syscall.Kill(-pgid, syscall.SIGKILL)
		case <-stopWatcher:
		}
		abortCh <- af
	}()

	// 1 MiB line ceiling: KTAP lines + log blocks fit comfortably; only a
	// wedge would exceed it.
	scanner := bufio.NewScanner(stdout)
	scanner.Buffer(make([]byte, 0, 64<<10), 1<<20)
	for scanner.Scan() {
		lastLine.Store(time.Now().UnixNano())
		line := scanner.Text()
		// Kernel klog occasionally emits a stray non-UTF-8 byte under
		// load; the renderer and parser must see valid strings only.
		if !isASCII(line) {
			line = strings.ToValidUTF8(line, "�")
		}
		onLine(line)
	}
	if scanErr := scanner.Err(); scanErr != nil && !errors.Is(scanErr, io.EOF) {
		// Don't return; child status takes priority.
		_ = scanErr
	}

	waitErr := <-waitCh
	silenceCancel()
	close(stopWatcher)
	af := <-abortCh

	res := DriverResult{
		UserAborted: af.userAborted,
		TimedOut:    af.timedOut,
		SilenceHit:  af.silenceHit,
	}
	if waitErr == nil {
		zero := 0
		res.QemuStatus = &zero
	} else if exitErr, ok := waitErr.(*exec.ExitError); ok {
		st := exitErr.ExitCode()
		res.QemuStatus = &st
	} else {
		// I/O error etc. — leave QemuStatus nil; main treats unset as
		// "kernel didn't reach exit cleanly".
	}
	return res, nil
}

// isASCII avoids the ToValidUTF8 call on the common pure-ASCII klog hot path.
func isASCII(s string) bool {
	for i := 0; i < len(s); i++ {
		if s[i] >= 0x80 {
			return false
		}
	}
	return true
}
