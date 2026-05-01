package main

import (
	"strings"
	"testing"
)

const testBase = "tests=on tests.shutdown=on tests.verbosity=summary boot.debug=on"

func TestAssembleNoFilterNoSkipNoVerbosity(t *testing.T) {
	got := assembleTestCmdline(testBase, nil, nil, "", "")
	if got != testBase {
		t.Errorf("got %q, want %q", got, testBase)
	}
}

func TestAssembleSingleFilter(t *testing.T) {
	got := assembleTestCmdline(testBase, []string{"mm::*"}, nil, "", "")
	if !strings.Contains(got, "tests.run=mm::*") {
		t.Errorf("missing tests.run=mm::* in %q", got)
	}
}

func TestAssembleMultipleFiltersJoinedWithComma(t *testing.T) {
	got := assembleTestCmdline(testBase, []string{"mm::*", "core::*"}, nil, "", "")
	if !strings.Contains(got, "tests.run=mm::*,core::*") {
		t.Errorf("missing comma-joined filters in %q", got)
	}
}

func TestAssembleSkipGlob(t *testing.T) {
	got := assembleTestCmdline(testBase, nil, []string{"*::tcp_live::*"}, "", "")
	if !strings.Contains(got, "tests.skip=*::tcp_live::*") {
		t.Errorf("missing tests.skip in %q", got)
	}
}

func TestAssembleVerbosityReplacesDefault(t *testing.T) {
	got := assembleTestCmdline(testBase, nil, nil, "verbose", "")
	if !strings.Contains(got, "tests.verbosity=verbose") {
		t.Errorf("missing tests.verbosity=verbose in %q", got)
	}
	if strings.Contains(got, "tests.verbosity=summary") {
		t.Errorf("default tests.verbosity=summary should have been replaced in %q", got)
	}
}

func TestAssembleExtraAppended(t *testing.T) {
	got := assembleTestCmdline(testBase, nil, nil, "", "tests.warn_ms=500")
	if !strings.HasSuffix(got, "tests.warn_ms=500") {
		t.Errorf("expected suffix tests.warn_ms=500, got %q", got)
	}
}

// -----------------------------------------------------------------------
//  Argv preprocessing — POSIX `--flag value` → `--flag=value`
// -----------------------------------------------------------------------

func TestPreprocessFlagValueFusion(t *testing.T) {
	in := []string{"--filter", "mm::*", "--skip", "tcp_live::*", "--verbose"}
	out := preprocessArgv(in)
	want := []string{"--filter=mm::*", "--skip=tcp_live::*", "--verbose"}
	if !equalSlices(out, want) {
		t.Errorf("got %v, want %v", out, want)
	}
}

func TestPreprocessAlreadyFusedPassesThrough(t *testing.T) {
	in := []string{"--filter=mm::*", "--no-color"}
	out := preprocessArgv(in)
	if !equalSlices(out, in) {
		t.Errorf("got %v, want unchanged %v", out, in)
	}
}

func TestPreprocessLeavesBooleansAlone(t *testing.T) {
	in := []string{"--rerun-failed", "--verbose", "--quiet", "--raw", "--no-build", "--dry-run"}
	out := preprocessArgv(in)
	if !equalSlices(out, in) {
		t.Errorf("booleans should pass through unchanged; got %v", out)
	}
}

// -----------------------------------------------------------------------
//  Parsing into Args
// -----------------------------------------------------------------------

func TestParseArgsNoColorAlias(t *testing.T) {
	args, err := parseArgs(preprocessArgv([]string{"--no-color"}))
	if err != nil {
		t.Fatal(err)
	}
	if args.ColorMode != "never" {
		t.Errorf("ColorMode: got %q, want never", args.ColorMode)
	}
}

func TestParseArgsEmptyFilterDropped(t *testing.T) {
	// Justfile invokes `--filter ""`; preprocess fuses to `--filter=`,
	// parseArgs filters out empty entries.
	args, err := parseArgs(preprocessArgv([]string{"--filter="}))
	if err != nil {
		t.Fatal(err)
	}
	if len(args.Filters) != 0 {
		t.Errorf("filters: got %v, want empty", args.Filters)
	}
}

func TestParseArgsFilterAndRerunFailedMutuallyExclusive(t *testing.T) {
	_, err := parseArgs(preprocessArgv([]string{"--filter", "x", "--rerun-failed"}))
	if err == nil {
		t.Errorf("expected mutex error, got nil")
	}
}

func TestParseArgsVerbosityModesMutuallyExclusive(t *testing.T) {
	_, err := parseArgs(preprocessArgv([]string{"--verbose", "--quiet"}))
	if err == nil {
		t.Errorf("expected verbose+quiet mutex error")
	}
	_, err = parseArgs(preprocessArgv([]string{"--quiet", "--raw"}))
	if err == nil {
		t.Errorf("expected quiet+raw mutex error")
	}
}

func equalSlices(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}
