package main

import "io"

// RawRenderer is intentionally inert: `main`'s on-line callback writes the
// original bytes, so this only satisfies the `Renderer` interface for `--raw`.
// Re-emitting events here would double every line on the wire.
type RawRenderer struct {
	Out io.Writer
}

// OnEvent is a no-op — main already passed the line through.
func (r *RawRenderer) OnEvent(_ Event, _ *RunSummary) {}

// Finalize is a no-op — raw mode doesn't render summary sections.
func (r *RawRenderer) Finalize(_ *RunSummary) {}
