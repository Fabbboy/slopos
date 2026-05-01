package main

import (
	"os"
	"strings"

	"golang.org/x/term"
)

// ANSI escape constants. Bytes only — no library "styled string" wrapper;
// callers compose with `Paint`.
const (
	ansiReset   = "\x1b[0m"
	ansiBold    = "\x1b[1m"
	ansiDim     = "\x1b[2m"
	ansiGreen   = "\x1b[32m"
	ansiRed     = "\x1b[31m"
	ansiYellow  = "\x1b[33m"
	ansiMagenta = "\x1b[35m"
	ansiBlue    = "\x1b[34m"
	ansiGrey    = "\x1b[90m"
	ansiRedBold = "\x1b[31;1m"

	// Cursor / line control. Used only when stdout is a TTY; CI logs (no
	// TTY) wouldn't render these anyway and we skip them entirely.
	ansiCR         = "\r"
	ansiEraseToEnd = "\x1b[K"

	// Bar character set. Filled / empty blocks read well across kitty,
	// iTerm, Terminal.app, Windows Terminal, and standard xterm-256color.
	barFill  = "█"
	barEmpty = "░"
)

// Paint wraps `s` in `colour` + reset if `enabled`; otherwise returns `s`
// unchanged. Callers that conditionally colourise pass `Paint(s, X, r.colour)`.
func Paint(s, colour string, enabled bool) string {
	if !enabled {
		return s
	}
	return colour + s + ansiReset
}

// VisibleLen counts on-screen columns by stripping ANSI escapes first.
// Used by the bar layout so right-aligned suffix doesn't drift when colour
// codes are present.
func VisibleLen(s string) int {
	return len(ansiRE.ReplaceAllString(s, ""))
}

// UseColour decides whether to emit colour codes given the user's `--color`
// choice and whether stdout is actually a TTY. Honours the conventional
// `NO_COLOR` env var (https://no-color.org).
//
// `mode` is one of `"auto"`, `"always"`, `"never"`.
func UseColour(mode string, fd uintptr) bool {
	switch mode {
	case "always":
		return true
	case "never":
		return false
	}
	if _, ok := os.LookupEnv("NO_COLOR"); ok {
		return false
	}
	return term.IsTerminal(int(fd))
}

// IsTTY reports whether stdout is connected to a terminal. The bar
// renderer's in-place updates depend on this; CI logs (pipes, redirects,
// GitHub Actions log capture) all return false.
func IsTTY(fd uintptr) bool {
	return term.IsTerminal(int(fd))
}

// TerminalCols reports the current width of the terminal stdout is bound
// to, or 100 if it can't be determined (e.g., not a TTY). Matches the
// fallback the Python wrapper used.
func TerminalCols(fd uintptr) int {
	w, _, err := term.GetSize(int(fd))
	if err != nil || w <= 0 {
		return 100
	}
	if w < 60 {
		// Don't try to render the bar in a sub-60-col terminal — pin a
		// floor so the prefix + suffix always fit and the bar fragment
		// gets ≥ 10 cols.
		return 60
	}
	return w
}

// stripANSI returns `s` with all ANSI CSI sequences removed.
func stripANSI(s string) string {
	if !strings.Contains(s, "\x1b") {
		return s
	}
	return ansiRE.ReplaceAllString(s, "")
}
