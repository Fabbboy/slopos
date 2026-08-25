// Tests for the silence watchdog's live budget. None of these start QEMU:
// TightenSilence only touches the atomic the watchdog goroutine re-reads.

package main

import (
	"context"
	"sync/atomic"
	"testing"
	"time"
)

func TestTightenSilenceLowersOnlyDownward(t *testing.T) {
	d := &QemuDriver{SilenceSec: 120}
	d.silenceLimitNs.Store(int64(120 * time.Second))
	d.TightenSilence(20 * time.Second)
	if got := time.Duration(d.silenceLimitNs.Load()); got != 20*time.Second {
		t.Fatalf("want 20s, got %v", got)
	}
	d.TightenSilence(90 * time.Second)
	if got := time.Duration(d.silenceLimitNs.Load()); got != 20*time.Second {
		t.Fatalf("TightenSilence must never raise the limit, got %v", got)
	}
}

// --silence-secs=0 means "do not abort on silence"; no watchdog goroutine was
// started, so the tightening is a documented no-op rather than a silent one.
func TestTightenSilenceIsNoOpWhenDisabled(t *testing.T) {
	d := &QemuDriver{SilenceSec: 0}
	d.TightenSilence(20 * time.Second)
	if d.silenceLimitNs.Load() != 0 {
		t.Fatalf("--silence-secs=0 must leave the watchdog unarmed")
	}
}

// A second abort event must not walk the budget back up, and a nonsense limit
// must not disarm the watchdog by storing zero.
func TestTightenSilenceIgnoresNonPositiveLimits(t *testing.T) {
	d := &QemuDriver{SilenceSec: 120}
	d.silenceLimitNs.Store(int64(20 * time.Second))
	d.TightenSilence(0)
	d.TightenSilence(-5 * time.Second)
	if got := time.Duration(d.silenceLimitNs.Load()); got != 20*time.Second {
		t.Fatalf("want the budget untouched at 20s, got %v", got)
	}
}

// Repeated aborts are idempotent: the watchdog keeps the tightest budget seen.
func TestTightenSilenceIsIdempotent(t *testing.T) {
	d := &QemuDriver{SilenceSec: 120}
	d.silenceLimitNs.Store(int64(120 * time.Second))
	for i := 0; i < 4; i++ {
		d.TightenSilence(PostAbortSilenceSec * time.Second)
	}
	if got := time.Duration(d.silenceLimitNs.Load()); got != PostAbortSilenceSec*time.Second {
		t.Fatalf("want %ds, got %v", PostAbortSilenceSec, got)
	}
}

// The whole point of the tightening: a watchdog already ticking against a huge
// budget must notice a lowered one. The tick is stepped by hand, so the first
// send proves the loop is running (and that a limit captured before the loop
// has already been captured) before the budget moves — that ordering is what
// makes this reject a stale limit rather than racing it.
func TestTightenSilenceShortensAnAlreadyRunningWatchdog(t *testing.T) {
	d := &QemuDriver{SilenceSec: 3600}
	d.silenceLimitNs.Store(int64(3600 * time.Second))

	var lastLine atomic.Int64
	lastLine.Store(time.Now().UnixNano())
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	tick := make(chan time.Time)
	tripped := make(chan struct{}, 1)
	go d.watchSilence(ctx, tick, &lastLine, tripped)

	tick <- time.Now()
	select {
	case <-tripped:
		t.Fatalf("watchdog tripped against its untightened 1h budget")
	default:
	}

	d.TightenSilence(time.Nanosecond)
	tick <- time.Now()
	select {
	case <-tripped:
	case <-time.After(5 * time.Second):
		t.Fatalf("watchdog never re-read the tightened budget")
	}
}

// The tick has to be short relative to the post-abort budget, or the tightening
// is noticed only after it has already elapsed.
func TestSilenceTickIsShortEnoughToServeTheAbortBudget(t *testing.T) {
	if silenceTickInterval > time.Second {
		t.Fatalf("tick %v is too coarse", silenceTickInterval)
	}
	if silenceTickInterval >= PostAbortSilenceSec*time.Second {
		t.Fatalf("tick %v cannot detect a %ds budget", silenceTickInterval, PostAbortSilenceSec)
	}
}

// An untightened watchdog must still trip on its own budget.
func TestWatchSilenceTripsOnTheInitialBudget(t *testing.T) {
	d := &QemuDriver{SilenceSec: 1}
	d.silenceLimitNs.Store(1)

	var lastLine atomic.Int64
	lastLine.Store(time.Now().UnixNano())
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	tick := make(chan time.Time)
	tripped := make(chan struct{}, 1)
	go d.watchSilence(ctx, tick, &lastLine, tripped)

	tick <- time.Now()
	select {
	case <-tripped:
	case <-time.After(5 * time.Second):
		t.Fatalf("watchdog never tripped on its own budget")
	}
}

// Cancelling the context must stop the goroutine without tripping: a run that
// ended normally is not a silence.
func TestWatchSilenceStopsOnContextCancel(t *testing.T) {
	d := &QemuDriver{SilenceSec: 3600}
	d.silenceLimitNs.Store(int64(3600 * time.Second))

	var lastLine atomic.Int64
	lastLine.Store(time.Now().UnixNano())
	ctx, cancel := context.WithCancel(context.Background())
	tripped := make(chan struct{}, 1)
	done := make(chan struct{})
	go func() {
		d.watchSilence(ctx, make(chan time.Time), &lastLine, tripped)
		close(done)
	}()
	cancel()
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatalf("watchSilence ignored its context")
	}
	select {
	case <-tripped:
		t.Fatalf("a cancelled watchdog must not trip")
	default:
	}
}
