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
	// SilenceSec aborts the run when the QEMU stdout pipe stays
	// completely silent for this many seconds. 0 disables. Catches
	// inter-phase wedges that would otherwise burn the full WallTimeoutSec.
	SilenceSec float64
}

// DriverResult is what `Run` returns to main.
type DriverResult struct {
	QemuStatus  *int // exit status from qemu_run.sh; nil if process never finished
	UserAborted bool // SIGINT received from the user
	TimedOut    bool // wall-clock guard or silence watchdog fired
	SilenceHit  bool // silence watchdog (not wall-clock) was the trigger
}

// Run streams QEMU's stdout line-by-line through `onLine`, enforcing the
// configured wall timeout and SIGINT (Ctrl-C) handling. Returns when the
// child exits or is killed.
func (d *QemuDriver) Run(ctx context.Context, onLine func(string)) (DriverResult, error) {
	cmd := exec.Command(
		d.RepoRoot+"/scripts/qemu_run.sh", "test", d.Iso, d.FsImage,
	)
	// Forward every env var from the parent — qemu_run.sh and its callees
	// honour QEMU_BIN/QEMU_SMP/etc.
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

	// Place the child in its own process group so we can signal the whole
	// tree on cancel. Without this, sending SIGTERM/SIGKILL to qemu_run.sh
	// (a bash script) leaves QEMU orphaned — the kernel reparents it to
	// PID 1 and it keeps the stdout pipe open forever, blocking our
	// scanner. (Reproduced on GitHub Actions: a 905s wall-timeout fired
	// but the wrapper hung indefinitely afterwards.)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}

	// Wall-clock guard. When WallTimeoutSec > 0, set a deadline; otherwise
	// inherit the caller's context. SIGINT fires the parent context.
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

	// Single goroutine owns cmd.Wait. Result lands on `waitCh`.
	waitCh := make(chan error, 1)
	go func() { waitCh <- cmd.Wait() }()

	// Silence watchdog. lastLine is the monotonic time of the last byte
	// received from QEMU. Watcher goroutine polls it and trips when the
	// pipe goes dead longer than SilenceSec.
	var lastLine atomic.Int64
	lastLine.Store(time.Now().UnixNano())
	silenceCtx, silenceCancel := context.WithCancel(context.Background())
	defer silenceCancel()
	silenceTripped := make(chan struct{}, 1)
	if d.SilenceSec > 0 {
		go func() {
			interval := time.Duration(d.SilenceSec*float64(time.Second)) / 4
			if interval < time.Second {
				interval = time.Second
			}
			t := time.NewTicker(interval)
			defer t.Stop()
			limit := time.Duration(d.SilenceSec * float64(time.Second))
			for {
				select {
				case <-silenceCtx.Done():
					return
				case <-t.C:
					last := time.Unix(0, lastLine.Load())
					if time.Since(last) >= limit {
						select {
						case silenceTripped <- struct{}{}:
						default:
						}
						return
					}
				}
			}
		}()
	}

	// Watcher goroutine: cancels on context (wall-timeout / SIGINT) or on
	// silence trip by signaling the entire process group with SIGTERM,
	// then SIGKILL after a 5-second grace. Killing the pgrp closes QEMU's
	// stdout, which lets the scanner loop below return.
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
		// Signal the whole process group, not just bash, so QEMU dies too.
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

	// Line loop. bufio.Scanner with 1 MiB ceiling — KTAP lines + log
	// blocks fit comfortably; only a wedge would exceed it.
	scanner := bufio.NewScanner(stdout)
	scanner.Buffer(make([]byte, 0, 64<<10), 1<<20)
	for scanner.Scan() {
		lastLine.Store(time.Now().UnixNano())
		line := scanner.Text()
		// Belt-and-suspenders: kernel klog occasionally emits a stray
		// non-UTF-8 byte under load. Replace each with U+FFFD so the
		// renderer / parser see valid strings only.
		if !isASCII(line) {
			line = strings.ToValidUTF8(line, "�")
		}
		onLine(line)
	}
	if scanErr := scanner.Err(); scanErr != nil && !errors.Is(scanErr, io.EOF) {
		// Don't return; child status takes priority.
		_ = scanErr
	}

	// Wait for child exit, then stop the watcher.
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

// isASCII reports whether every byte of s is < 0x80 (avoids ToValidUTF8
// on the common pure-ASCII klog hot path).
func isASCII(s string) bool {
	for i := 0; i < len(s); i++ {
		if s[i] >= 0x80 {
			return false
		}
	}
	return true
}
