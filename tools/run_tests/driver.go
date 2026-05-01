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
}

// DriverResult is what `Run` returns to main.
type DriverResult struct {
	QemuStatus  *int // exit status from qemu_run.sh; nil if process never finished
	UserAborted bool // SIGINT received from the user
	TimedOut    bool // wall-clock guard fired
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

	// Wall-clock guard. WhenWallTimeoutSec > 0, set a deadline; otherwise
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

	// Single goroutine owns cmd.Wait. Result lands on `waitCh`. Anyone
	// who wants to know "child has exited" reads from `waitCh` exactly
	// once; we forward it through `waitDone` to allow multiple consumers.
	waitCh := make(chan error, 1)
	go func() { waitCh <- cmd.Wait() }()

	// Watcher goroutine: cancels on context (timeout / SIGINT) by sending
	// SIGTERM, then SIGKILL after a 5-second grace. If the child exits on
	// its own first, this goroutine no-ops.
	type abortFlags struct {
		userAborted bool
		timedOut    bool
	}
	abortCh := make(chan abortFlags, 1)
	stopWatcher := make(chan struct{})
	go func() {
		select {
		case <-runCtx.Done():
			af := abortFlags{}
			if errors.Is(runCtx.Err(), context.DeadlineExceeded) {
				af.timedOut = true
			} else {
				af.userAborted = true
			}
			if cmd.Process != nil {
				_ = cmd.Process.Signal(syscall.SIGTERM)
			}
			grace := time.NewTimer(5 * time.Second)
			defer grace.Stop()
			select {
			case <-grace.C:
				if cmd.Process != nil {
					_ = cmd.Process.Kill()
				}
			case <-stopWatcher:
				// Child exited on its own (or via SIGTERM) before grace.
			}
			abortCh <- af
		case <-stopWatcher:
			abortCh <- abortFlags{}
		}
	}()

	// Line loop. bufio.Scanner with 1 MiB ceiling — KTAP lines + log
	// blocks fit comfortably; only a wedge would exceed it.
	scanner := bufio.NewScanner(stdout)
	scanner.Buffer(make([]byte, 0, 64<<10), 1<<20)
	for scanner.Scan() {
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
	close(stopWatcher)
	af := <-abortCh

	res := DriverResult{
		UserAborted: af.userAborted,
		TimedOut:    af.timedOut,
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
