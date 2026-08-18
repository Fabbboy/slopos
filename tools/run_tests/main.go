// run_tests — host-side wrapper that drives `just _iso-tests` to bake the
// kernel test cmdline, launches the resulting ISO under QEMU via
// `scripts/qemu_run.sh test`, and renders the kernel's KTAP-grammar serial
// output as a progress bar plus per-failure detail blocks.
//
// Wire format documented in the public KTAP docs.
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

// repoRoot resolves the SlopOS repo root by walking up for a `justfile`,
// falling back to the current working directory.
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
		// flag.Parse already printed usage on parse errors.
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
	// Without this snapshot an aborted run renders its banner with no kernel
	// context at all.
	if driverRes.TimedOut || driverRes.UserAborted || recorder.Summary.Truncated {
		recorder.Summary.AbortKlogTail = parser.KlogTail()
	}

	renderer.Finalize(recorder.Summary)

	failures := recorder.Summary.Failures()
	// A run narrowed by the caller may match nothing; an unfiltered one may
	// not. `--rerun-failed` counts as a selection: `filters` holds the names
	// it read from last-fail.list.
	hasSelection := len(filters) > 0 || len(args.Skips) > 0
	verdict := ClassifyRun(recorder.Summary, driverRes, hasSelection)
	if verdict.QemuStatusWarning != "" {
		fmt.Fprintln(os.Stderr, verdict.QemuStatusWarning)
	}
	if verdict.Diagnostic != "" {
		fmt.Fprintln(os.Stderr, verdict.Diagnostic)
	}
	exitCode := verdict.Code

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
