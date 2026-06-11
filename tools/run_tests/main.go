// run_tests — host-side wrapper that drives `just _iso-tests` to bake
// the kernel test cmdline, launches the resulting ISO under QEMU via
// `scripts/qemu_run.sh test`, parses the kernel's KTAP-grammar serial
// output line-by-line, and renders a developer-friendly progress bar
// plus per-failure detail blocks.
//
// Wire format documented in the public KTAP docs. Replaces the earlier
// `scripts/run_tests.py` Phase 4 prototype 1:1 — same flags, same UX,
// same JSONL event schema, same exit-code policy.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
)

// repoRoot resolves to the SlopOS repo root by walking up from the
// running binary's location until a `justfile` is found. Falls back to
// the current working directory if none is found (useful for unit
// tests that don't actually invoke QEMU).
func repoRoot() string {
	if cwd, err := os.Getwd(); err == nil {
		dir := cwd
		for {
			if _, err := os.Stat(filepath.Join(dir, "justfile")); err == nil {
				return dir
			}
			parent := filepath.Dir(dir)
			if parent == dir {
				break
			}
			dir = parent
		}
	}
	if exe, err := os.Executable(); err == nil {
		dir := filepath.Dir(exe)
		for {
			if _, err := os.Stat(filepath.Join(dir, "justfile")); err == nil {
				return dir
			}
			parent := filepath.Dir(dir)
			if parent == dir {
				break
			}
			dir = parent
		}
	}
	return "."
}

// rendererIface decouples main from the concrete renderer type.
type rendererIface interface {
	OnEvent(Event, *RunSummary)
	Finalize(*RunSummary)
}

func main() {
	os.Exit(run(os.Args[1:]))
}

func run(rawArgv []string) int {
	args, err := parseArgs(preprocessArgv(rawArgv))
	if err != nil {
		// flag.Parse already printed usage on parse errors; for our own
		// validation errors we print here.
		if !errors.Is(err, flag.ErrHelp) {
			fmt.Fprintln(os.Stderr, "run_tests:", err)
		}
		return 64
	}

	root := repoRoot()
	buildDir := filepath.Join(root, "builddir")
	defaultIso := filepath.Join(buildDir, "slop-tests.iso")
	defaultFsImage := filepath.Join(root, "fs/assets/ext2-tests.img")
	lastFailList := filepath.Join(buildDir, "last-fail.list")

	// --- assemble cmdline
	filters := append([]string(nil), args.Filters...)
	if args.RerunFailed {
		names, err := readLastFailList(lastFailList)
		if err != nil {
			fmt.Fprintln(os.Stderr, "run_tests:", err)
			return 64
		}
		filters = names
	}
	verbosity := ""
	switch {
	case args.Verbose:
		verbosity = "verbose"
	case args.Quiet:
		verbosity = "quiet"
	}
	extra := ""
	if args.WarnMs != 0 {
		extra = fmt.Sprintf("tests.warn_ms=%d", args.WarnMs)
	}
	cmdline := assembleTestCmdline(DefaultBaseCmdline, filters, args.Skips, verbosity, extra)

	if args.DryRun {
		fmt.Printf("TEST_CMDLINE=%q\n", cmdline)
		fmt.Printf("would run: scripts/qemu_run.sh test %s %s\n",
			pickPath(args.Iso, defaultIso), pickPath(args.FsImage, defaultFsImage))
		return 0
	}

	// --- build ISO
	if !args.NoBuild {
		if err := runJustIsoTests(root, cmdline); err != nil {
			fmt.Fprintln(os.Stderr, "run_tests:", err)
			return 64
		}
	}

	iso := pickPath(args.Iso, defaultIso)
	fsImage := pickPath(args.FsImage, defaultFsImage)
	if _, err := os.Stat(iso); err != nil {
		fmt.Fprintf(os.Stderr, "run_tests: ISO not found at %s\n", iso)
		return 64
	}
	if _, err := os.Stat(fsImage); err != nil {
		fmt.Fprintf(os.Stderr, "run_tests: fs image not found at %s\n", fsImage)
		return 64
	}

	// --- set up parser, recorder, renderer, JSONL sink
	parser := NewKtapParser()
	recorder := NewRecorder()

	var jsonlSink *JsonlSink
	if args.JsonPath != "" {
		s, err := NewJsonlSink(args.JsonPath)
		if err != nil {
			fmt.Fprintln(os.Stderr, "run_tests:", err)
			return 64
		}
		jsonlSink = s
		defer jsonlSink.Close()
	}

	stdoutFd := os.Stdout.Fd()
	tty := IsTTY(stdoutFd)
	colour := UseColour(args.ColorMode, stdoutFd)
	cols := TerminalCols(stdoutFd)

	rawPassthrough := args.Raw
	verbosityRender := "summary"
	switch {
	case args.Verbose:
		verbosityRender = "verbose"
	case args.Quiet:
		verbosityRender = "quiet"
	}

	var renderer rendererIface
	if rawPassthrough {
		renderer = &RawRenderer{Out: os.Stdout}
	} else {
		renderer = NewBarRenderer(os.Stdout, verbosityRender, colour, args.WarnMs, tty, cols)
	}

	// --- driver
	ctx, cancelSignal := signal.NotifyContext(context.Background(),
		os.Interrupt, syscall.SIGTERM)
	defer cancelSignal()

	driver := &QemuDriver{
		RepoRoot:       root,
		Iso:            iso,
		FsImage:        fsImage,
		WallTimeoutSec: float64(args.TimeoutSecs),
		SilenceSec:     float64(args.SilenceSecs),
	}
	onLine := func(line string) {
		if rawPassthrough {
			fmt.Fprintln(os.Stdout, line)
		}
		for _, ev := range parser.Feed(line) {
			recorder.Record(ev)
			renderer.OnEvent(ev, recorder.Summary)
			if jsonlSink != nil {
				if err := jsonlSink.Write(ev, recorder.Summary); err != nil {
					fmt.Fprintln(os.Stderr, "run_tests: jsonl:", err)
				}
			}
		}
	}
	driverRes, err := driver.Run(ctx, onLine)
	if err != nil {
		fmt.Fprintln(os.Stderr, "run_tests:", err)
		return 64
	}

	recorder.Finalize(driverRes.QemuStatus)
	recorder.Summary.UserAborted = driverRes.UserAborted
	recorder.Summary.TimedOut = driverRes.TimedOut
	recorder.Summary.SilenceHit = driverRes.SilenceHit
	// Snapshot the parser's klog ring buffer when the run aborted —
	// without it, summary-verbosity CI failures show a "TIMED OUT"
	// banner with no kernel context.
	if driverRes.TimedOut || driverRes.UserAborted || recorder.Summary.Truncated {
		recorder.Summary.AbortKlogTail = parser.KlogTail()
	}

	renderer.Finalize(recorder.Summary)

	// --- compute exit + persist last-fail.list
	failures := recorder.Summary.Failures()
	bailed := false
	for _, p := range recorder.Summary.Phases {
		if p.BailReason != nil {
			bailed = true
			break
		}
	}
	failedOverall := len(failures) > 0 || bailed || driverRes.TimedOut || recorder.Summary.Truncated

	exitCode := 0
	switch {
	case driverRes.UserAborted:
		exitCode = 130
	case failedOverall:
		exitCode = 1
		if driverRes.QemuStatus != nil && *driverRes.QemuStatus != 0 && *driverRes.QemuStatus != 1 {
			fmt.Fprintf(os.Stderr,
				"run_tests: warning: unexpected qemu_run.sh exit status %d "+
					"(kernel did not reach isa-debug-exit cleanly)\n",
				*driverRes.QemuStatus)
			exitCode = 2
		}
	default:
		if driverRes.QemuStatus != nil && *driverRes.QemuStatus != 0 {
			fmt.Fprintf(os.Stderr,
				"run_tests: warning: green run but qemu_run.sh exit status was %d; "+
					"treating as wrapper failure\n", *driverRes.QemuStatus)
			exitCode = 2
		}
	}

	if !driverRes.UserAborted {
		if err := writeLastFailList(lastFailList, failures); err != nil {
			fmt.Fprintln(os.Stderr, "run_tests:", err)
		}
		if !rawPassthrough {
			rel, _ := filepath.Rel(root, lastFailList)
			n := len(failures)
			plural := "ies"
			if n == 1 {
				plural = "y"
			}
			if n > 0 {
				fmt.Printf("last-fail.list updated → %s (%d entr%s)\n", rel, n, plural)
			} else {
				fmt.Printf("last-fail.list cleared → %s\n", rel)
			}
		}
	}

	if !rawPassthrough {
		fmt.Printf("exit: %d\n", exitCode)
	}

	if jsonlSink != nil {
		if err := jsonlSink.WriteRunEnd(recorder.Summary, exitCode); err != nil {
			fmt.Fprintln(os.Stderr, "run_tests:", err)
		}
	}
	return exitCode
}

func pickPath(override, fallback string) string {
	if override != "" {
		return override
	}
	return fallback
}

func readLastFailList(path string) ([]string, error) {
	if _, err := os.Stat(path); err != nil {
		return nil, fmt.Errorf("--rerun-failed: %s not found. Run `just test` first", path)
	}
	body, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("--rerun-failed: %w", err)
	}
	var names []string
	for _, ln := range strings.Split(string(body), "\n") {
		ln = strings.TrimSpace(ln)
		if ln != "" {
			names = append(names, ln)
		}
	}
	if len(names) == 0 {
		return nil, fmt.Errorf("--rerun-failed: %s is empty (last run was green)", path)
	}
	return names, nil
}

func writeLastFailList(path string, failures []*TestRecord) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return fmt.Errorf("last-fail.list: mkdir: %w", err)
	}
	var b strings.Builder
	for _, rec := range failures {
		b.WriteString(rec.Name)
		b.WriteByte('\n')
	}
	if err := os.WriteFile(path, []byte(b.String()), 0o644); err != nil {
		return fmt.Errorf("last-fail.list: write: %w", err)
	}
	return nil
}

func runJustIsoTests(root, cmdline string) error {
	cmd := exec.Command("just", "_iso-tests")
	cmd.Dir = root
	env := os.Environ()
	env = append(env, "TEST_CMDLINE="+cmdline)
	cmd.Env = env
	cmd.Stdout = os.Stderr
	cmd.Stderr = os.Stderr
	fmt.Fprintf(os.Stderr, "==> TEST_CMDLINE=%q\n", cmdline)
	fmt.Fprintln(os.Stderr, "==> just _iso-tests")
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("just _iso-tests failed: %w", err)
	}
	return nil
}
