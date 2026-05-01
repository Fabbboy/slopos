package main

import (
	"flag"
	"fmt"
	"os"
	"regexp"
	"strings"
)

// DefaultBaseCmdline is the standard kernel cmdline plumbed into the test
// ISO. `assembleTestCmdline` adds tests.run / tests.skip / verbosity
// overrides on top.
const DefaultBaseCmdline = "tests=on tests.shutdown=on tests.verbosity=summary " +
	"boot.debug=on roulette=skip"

// Args is the parsed CLI flag set after preprocessing + flag.Parse.
type Args struct {
	Filters     []string
	Skips       []string
	RerunFailed bool
	Verbose     bool
	Quiet       bool
	Raw         bool
	JsonPath    string
	ColorMode   string
	NoColor     bool
	TimeoutSecs int
	WarnMs      int
	Iso         string
	FsImage     string
	NoBuild     bool
	DryRun      bool
}

// stringSlice implements flag.Value to accept repeatable flags
// (`--filter foo --filter bar`).
type stringSlice []string

func (s *stringSlice) String() string {
	if s == nil {
		return ""
	}
	return strings.Join(*s, ",")
}

func (s *stringSlice) Set(v string) error {
	*s = append(*s, v)
	return nil
}

// preprocessArgv normalises POSIX-style `--flag value` into Go flag
// package's accepted `--flag=value`. Boolean flags (no value) are
// passed through unchanged. This lets users keep typing
// `--filter slopos_mm::*` exactly as they did with the Python wrapper.
func preprocessArgv(in []string) []string {
	booleans := map[string]bool{
		"--rerun-failed": true,
		"--verbose":      true,
		"--quiet":        true,
		"--raw":          true,
		"--no-build":     true,
		"--no-color":     true,
		"--dry-run":      true,
		"-h":             true,
		"--help":         true,
	}
	out := make([]string, 0, len(in))
	i := 0
	for i < len(in) {
		tok := in[i]
		if !strings.HasPrefix(tok, "--") || strings.Contains(tok, "=") || booleans[tok] {
			out = append(out, tok)
			i++
			continue
		}
		// `--flag` followed by a value-looking token — fuse them.
		if i+1 < len(in) && !strings.HasPrefix(in[i+1], "-") {
			out = append(out, tok+"="+in[i+1])
			i += 2
			continue
		}
		out = append(out, tok)
		i++
	}
	return out
}

// parseArgs runs flag definitions over a preprocessed argv. Returns Args
// or a non-nil error that the caller should print to stderr (typical: a
// flag.Parse usage message).
func parseArgs(argv []string) (*Args, error) {
	fs := flag.NewFlagSet("run_tests", flag.ContinueOnError)
	a := &Args{
		ColorMode:   "auto",
		TimeoutSecs: 900,
		WarnMs:      500,
	}
	var filters, skips stringSlice
	fs.Var(&filters, "filter", "Module-path glob; repeatable. Joined with ',' as tests.run=. Empty string ignored.")
	fs.Var(&skips, "skip", "Module-path glob; repeatable. Joined with ',' as tests.skip=.")
	fs.BoolVar(&a.RerunFailed, "rerun-failed", false, "Use builddir/last-fail.list as the filter set.")
	fs.BoolVar(&a.Verbose, "verbose", false, "Pass tests.verbosity=verbose; dump captured klog of every test.")
	fs.BoolVar(&a.Quiet, "quiet", false, "Pass tests.verbosity=quiet; render only failures + summary.")
	fs.BoolVar(&a.Raw, "raw", false, "Passthrough QEMU stdout; no rendering.")
	fs.StringVar(&a.JsonPath, "json", "", "Append one JSON event per line to PATH.")
	fs.StringVar(&a.ColorMode, "color", "auto", "Colour mode: auto|always|never.")
	fs.BoolVar(&a.NoColor, "no-color", false, "Alias for --color=never.")
	fs.IntVar(&a.TimeoutSecs, "timeout-secs", 900, "Wall-clock guard for the whole run; 0 disables.")
	fs.IntVar(&a.WarnMs, "warn-ms", 500, "Mark tests slower than this as OVER_TIME.")
	fs.StringVar(&a.Iso, "iso", "", "Test ISO path.")
	fs.StringVar(&a.FsImage, "fs-image", "", "Test fs image path.")
	fs.BoolVar(&a.NoBuild, "no-build", false, "Skip the `just _iso-tests` invocation.")
	fs.BoolVar(&a.DryRun, "dry-run", false, "Print the assembled cmdline + invocation; do not run QEMU.")
	fs.SetOutput(os.Stderr)

	if err := fs.Parse(argv); err != nil {
		return nil, err
	}

	a.Filters = filterEmpty(filters)
	a.Skips = filterEmpty(skips)

	if a.NoColor {
		a.ColorMode = "never"
	}
	// Mutual-exclusion checks.
	if a.RerunFailed && len(a.Filters) > 0 {
		return nil, fmt.Errorf("--rerun-failed and --filter are mutually exclusive")
	}
	modes := 0
	if a.Verbose {
		modes++
	}
	if a.Quiet {
		modes++
	}
	if a.Raw {
		modes++
	}
	if modes > 1 {
		return nil, fmt.Errorf("--verbose, --quiet, and --raw are mutually exclusive")
	}
	return a, nil
}

func filterEmpty(in []string) []string {
	out := make([]string, 0, len(in))
	for _, s := range in {
		if s != "" {
			out = append(out, s)
		}
	}
	return out
}

// assembleTestCmdline composes the full kernel cmdline string that gets
// baked into the test ISO. Verbosity replacement happens via regex (the
// base string already contains `tests.verbosity=summary`); filter / skip
// / extra are appended.
func assembleTestCmdline(base string, filters, skips []string, verbosity, extra string) string {
	parts := []string{base}
	if len(filters) > 0 {
		parts = append(parts, "tests.run="+strings.Join(filters, ","))
	}
	if len(skips) > 0 {
		parts = append(parts, "tests.skip="+strings.Join(skips, ","))
	}
	if verbosity != "" {
		re := regexp.MustCompile(`\btests\.verbosity=\w+\b`)
		parts[0] = re.ReplaceAllString(parts[0], "tests.verbosity="+verbosity)
	}
	if extra != "" {
		parts = append(parts, extra)
	}
	return strings.Join(parts, " ")
}
