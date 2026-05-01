package main

import "io"

// RawRenderer is intentionally inert. The actual line copy happens in
// `main`'s on-line callback (which writes the original bytes BEFORE the
// parser decomposes them); this struct just satisfies the `Renderer`
// interface so `--raw` mode plumbs through without special-casing.
//
// An earlier wrapper version had `RawRenderer.OnEvent` re-emit
// `EvNonKtap` events on top of the passthrough write — that doubled
// every kernel klog line on the wire, including KTAP\tok lines arriving
// with a leading garbage byte. See Phase 4 Notes (2026-05-01) for the
// history.
type RawRenderer struct {
	Out io.Writer
}

// OnEvent is a no-op — main already passed the line through.
func (r *RawRenderer) OnEvent(_ Event, _ *RunSummary) {}

// Finalize is a no-op — raw mode doesn't render summary sections.
func (r *RawRenderer) Finalize(_ *RunSummary) {}
