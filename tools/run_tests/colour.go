package main

import (
	"os"
	"strings"

	"golang.org/x/term"
)

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

	ansiCR         = "\r"
	ansiEraseToEnd = "\x1b[K"

	// These blocks read well across kitty, iTerm, Terminal.app, Windows
	// Terminal, and standard xterm-256color.
	barFill  = "█"
	barEmpty = "░"
)

// Paint wraps `s` in `colour` + reset if `enabled`; otherwise returns `s`
// unchanged.
func Paint(s, colour string, enabled bool) string {
	if !enabled {
		return s
	}
	return colour + s + ansiReset
}

// VisibleLen counts on-screen columns by stripping ANSI escapes first.
func VisibleLen(s string) int {
	return len(ansiRE.ReplaceAllString(s, ""))
}

// UseColour decides whether to emit colour codes. Honours the conventional
// `NO_COLOR` env var (https://no-color.org). `mode` is one of `"auto"`,
// `"always"`, `"never"`.
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

// IsTTY reports whether stdout is connected to a terminal.
func IsTTY(fd uintptr) bool {
	return term.IsTerminal(int(fd))
}

// TerminalCols reports the current width of the terminal stdout is bound
// to, or 100 if it can't be determined (e.g., not a TTY).
func TerminalCols(fd uintptr) int {
	w, _, err := term.GetSize(int(fd))
	if err != nil || w <= 0 {
		return 100
	}
	if w < 60 {
		// Floor so prefix + suffix always fit and the bar fragment gets
		// ≥ 10 cols.
		return 60
	}
	return w
}

func stripANSI(s string) string {
	if !strings.Contains(s, "\x1b") {
		return s
	}
	return ansiRE.ReplaceAllString(s, "")
}
