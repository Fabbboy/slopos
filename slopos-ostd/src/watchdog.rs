//! Cross-CPU lockup detector.
//!
//! Each CPU watches the next eligible one and compares its progress
//! counter — [`ProcessorControlRegion::heartbeat`] — against the watcher's
//! *own* previous reading. N consecutive unchanged samples is a breach.
//!
//! There is no clock in that predicate, which is the point. A wall-time
//! threshold has to be calibrated against the slowest machine the kernel
//! will ever run on, and emulation and host steal time both stretch that
//! without bound. Consecutive samples cannot be stretched: a host that
//! stalls the target stalls the watcher identically, and the watcher's own
//! samples come from its own timer interrupts.
//!
//! # Detection is not execution
//!
//! A breach means "this CPU has not taken a timer interrupt recently". It
//! does not mean the CPU is stopped — a CPU spinning on a lock with
//! interrupts masked is executing hard. So the first breach reports and
//! the machine survives it; only a sustained breach is fatal. Making every
//! detection lethal is what forces a threshold to be tuned for zero false
//! positives everywhere, which is how a detector loses its sharpness.
//!
//! [`ProcessorControlRegion::heartbeat`]: crate::cpu::x86_64::pcr::ProcessorControlRegion::heartbeat

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::cpu::x86_64::pcr::{self, MAX_CPUS};

/// Consecutive unchanged samples before a CPU is reported. At the 100 Hz
/// LAPIC tick this is one second.
///
/// Not the tens of milliseconds the sample-based predicate would allow:
/// the kernel has legitimate interrupts-off sections in the hundreds of
/// milliseconds, and the LAPIC timer tests stop the timer outright.
pub const DEFAULT_MISS_THRESHOLD: u32 = 100;

/// Multiple of the miss threshold at which a breach becomes fatal.
const FATAL_MULTIPLE: u32 = 5;

/// The same, once the wait-for chain has closed on itself. A cycle cannot
/// resolve on its own, so waiting longer only delays the report.
const FATAL_MULTIPLE_CYCLE: u32 = 1;

/// No CPU. `u32::MAX` rather than 0, which is a real CPU index.
const NO_CPU: u32 = u32::MAX;

static ENABLED: AtomicBool = AtomicBool::new(true);
static PANIC_ENABLED: AtomicBool = AtomicBool::new(true);
static MISS_THRESHOLD: AtomicU32 = AtomicU32::new(DEFAULT_MISS_THRESHOLD);

/// What a CPU should do about the NMI it is taking.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum NmiDisposition {
    /// No probe was armed for this CPU: a spurious or third-party NMI.
    Unsolicited = 0,
    /// Dump context and resume.
    Report = 1,
    /// Dump context and take the machine down.
    Fatal = 2,
    /// The TLB shootdown ladder gave up on this CPU and wants its context.
    TlbLadder = 3,
}

impl NmiDisposition {
    fn from_raw(raw: u32) -> Self {
        match raw {
            1 => Self::Report,
            2 => Self::Fatal,
            3 => Self::TlbLadder,
            _ => Self::Unsolicited,
        }
    }
}

/// Per-CPU detector state, one cache line each so a watcher's per-tick
/// stores do not invalidate its neighbours' lines.
#[repr(align(64))]
struct CpuSlot {
    /// Heartbeat of `target` at this watcher's previous sample.
    last_seen: AtomicU64,
    /// Consecutive samples in which it did not move.
    stale: AtomicU32,
    /// `stale` value at which the next report fires. Doubles after each,
    /// so a long legitimate section logs a handful of lines rather than
    /// one per tick.
    next_report: AtomicU32,
    /// Largest `stale` ever observed, for the shutdown summary. Answers
    /// "what is the real worst-case interrupts-off section" from
    /// measurement rather than from bug reports.
    max_stale: AtomicU32,
    /// The CPU this one watches, cached across ticks.
    target: AtomicU32,
    /// Disposition of the NMI this CPU is being sent, and the interlock
    /// that stops a second one arriving while the first is being handled.
    probe: AtomicU32,
    /// Odd while this CPU is spinning on a lock, bumped on entry and exit.
    /// A walker records it per hop and re-reads it afterwards, so a chain
    /// assembled out of links that were released and re-taken underneath
    /// it is rejected rather than reported as a cycle.
    wait_seq: AtomicU64,
    /// Holder CPU of the lock this one is spinning on; `NO_CPU` otherwise.
    blocked_on: AtomicU32,
    /// Address of that lock. Printed, never dereferenced — a `SpinLock` can
    /// live in a heap allocation the spinner frees the moment it wins, and
    /// the fault a walker would take is one an NMI handler cannot afford.
    waiting_on: AtomicU64,
}

/// Longest wait-for chain the walker will follow.
///
/// A real deadlock cycle is short; a chain this long is contention, and
/// bounding it keeps the walker's frame small enough to run from a stalled
/// spin loop.
const MAX_WAIT_HOPS: usize = 8;

#[derive(Clone, Copy)]
struct WaitHop {
    cpu: u32,
    seq: u64,
    lock: u64,
}

impl CpuSlot {
    const fn new() -> Self {
        Self {
            last_seen: AtomicU64::new(0),
            stale: AtomicU32::new(0),
            next_report: AtomicU32::new(0),
            max_stale: AtomicU32::new(0),
            target: AtomicU32::new(NO_CPU),
            probe: AtomicU32::new(NmiDisposition::Unsolicited as u32),
            wait_seq: AtomicU64::new(0),
            blocked_on: AtomicU32::new(NO_CPU),
            waiting_on: AtomicU64::new(0),
        }
    }
}

static SLOTS: [CpuSlot; MAX_CPUS] = [const { CpuSlot::new() }; MAX_CPUS];

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Turn the detector off entirely (`watchdog=off`).
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Release);
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// Consecutive unchanged samples before a CPU is reported
/// (`watchdog.miss_threshold=`). Zero is rejected — it would report every
/// tick.
pub fn set_miss_threshold(samples: u32) -> bool {
    if samples == 0 {
        return false;
    }
    MISS_THRESHOLD.store(samples, Ordering::Release);
    true
}

pub fn miss_threshold() -> u32 {
    MISS_THRESHOLD.load(Ordering::Acquire)
}

/// Whether a sustained breach may take the machine down
/// (`watchdog.panic=on|off`).
pub fn set_panic_enabled(enabled: bool) {
    PANIC_ENABLED.store(enabled, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

/// Record that this CPU has made progress.
///
/// > Touch only from a loop whose trip count is bounded by data already in
/// > hand, which acquires no lock and performs no wait. **Never from a wait
/// > loop.**
///
/// That rule is the whole difference between a progress heartbeat and a
/// renamed grace period. A touch inside a wait loop converts a real
/// deadlock into a silent permanent hang: it makes the CPU look alive
/// precisely while it is doing nothing.
#[inline]
pub fn touch() {
    pcr::heartbeat_bump();
}

/// Suppress watching of the current CPU for the token's lifetime.
///
/// For code that deliberately runs without timer ticks — stopping or
/// masking the LAPIC timer to test it, for instance. Unlike a touch this
/// cannot hide a deadlock: it is scoped, and a CPU that wedges inside the
/// scope stops running the `Drop` that ends it, so the suppression outlives
/// nothing.
pub struct Suppress {
    previous: bool,
}

impl Suppress {
    pub fn for_current_cpu() -> Self {
        Self {
            previous: pcr::set_watchdog_suppressed(true),
        }
    }
}

impl Drop for Suppress {
    fn drop(&mut self) {
        // Bump before unsuppressing: the first sample after the scope would
        // otherwise compare against a reading taken before it and count as
        // stale through no fault of this CPU.
        pcr::heartbeat_bump();
        pcr::set_watchdog_suppressed(self.previous);
    }
}

// ---------------------------------------------------------------------------
// The check
// ---------------------------------------------------------------------------

/// Timer-interrupt hook: record progress, then sample the neighbour.
///
/// Called from the timer interrupt rather than from the idle loop. A CPU
/// busy running a task never reaches the idle loop, so an idle-only check
/// leaves every loaded CPU unwatched.
#[inline]
pub fn tick() {
    // Before any lock is taken, so a heartbeat never depends on acquiring
    // one.
    pcr::heartbeat_bump();
    if !is_enabled() {
        return;
    }
    check_neighbour(pcr::get_current_cpu());
}

fn eligible(cpu: usize) -> bool {
    pcr::is_cpu_online(cpu) && pcr::timer_is_armed(cpu) && !pcr::watchdog_is_suppressed(cpu)
}

/// The next eligible CPU after `me`, scanning round-robin.
fn scan_for_target(me: usize) -> Option<usize> {
    let count = pcr::get_cpu_count().min(MAX_CPUS);
    (1..count)
        .map(|step| (me + step) % count)
        .find(|&cand| cand != me && eligible(cand))
}

fn check_neighbour(me: usize) {
    let Some(slot) = SLOTS.get(me) else {
        return;
    };

    let cached = slot.target.load(Ordering::Relaxed);
    let target = if cached != NO_CPU && eligible(cached as usize) {
        cached as usize
    } else {
        let Some(found) = scan_for_target(me) else {
            slot.target.store(NO_CPU, Ordering::Relaxed);
            reset(slot, 0);
            return;
        };
        slot.target.store(found as u32, Ordering::Relaxed);
        reset(slot, pcr::heartbeat_for_cpu(found));
        return;
    };

    let beat = pcr::heartbeat_for_cpu(target);
    if beat != slot.last_seen.load(Ordering::Relaxed) {
        reset(slot, beat);
        return;
    }

    let stale = slot.stale.load(Ordering::Relaxed).saturating_add(1);
    slot.stale.store(stale, Ordering::Relaxed);
    if stale > slot.max_stale.load(Ordering::Relaxed) {
        slot.max_stale.store(stale, Ordering::Relaxed);
    }
    if stale < slot.next_report.load(Ordering::Relaxed) {
        return;
    }
    slot.next_report
        .store(stale.saturating_mul(2), Ordering::Relaxed);
    report_stalled_cpu(me, target, stale);
}

/// Which CPU, if any, is currently watching `target`.
pub fn watcher_of(target: usize) -> Option<usize> {
    let count = pcr::get_cpu_count().min(MAX_CPUS);
    (0..count).find(|&cpu| {
        SLOTS
            .get(cpu)
            .is_some_and(|slot| slot.target.load(Ordering::Relaxed) == target as u32)
    })
}

fn reset(slot: &CpuSlot, beat: u64) {
    slot.last_seen.store(beat, Ordering::Relaxed);
    slot.stale.store(0, Ordering::Relaxed);
    slot.next_report.store(miss_threshold(), Ordering::Relaxed);
}

/// Announce the stall and NMI the target so it dumps its own context —
/// nobody else can see its registers.
fn report_stalled_cpu(me: usize, target: usize, stale: u32) {
    let cycle = wait_chain_closes_cycle(target);
    let fatal_at = miss_threshold().saturating_mul(if cycle {
        FATAL_MULTIPLE_CYCLE
    } else {
        FATAL_MULTIPLE
    });
    let disposition = if PANIC_ENABLED.load(Ordering::Acquire) && stale >= fatal_at {
        NmiDisposition::Fatal
    } else {
        NmiDisposition::Report
    };

    if !arm_probe(target, disposition) {
        // A probe is still being handled. Re-sending now would restart the
        // target's dump instead of letting it finish.
        return;
    }

    nmi_emit("WATCHDOG: cpu ");
    nmi_emit_dec(target as u64);
    nmi_emit(" made no progress for ");
    nmi_emit_dec(stale as u64);
    nmi_emit(" samples (watcher cpu ");
    nmi_emit_dec(me as u64);
    nmi_emit_line(")");
    dump_wait_chain(target);

    match pcr::apic_id_from_cpu_index(target) {
        Some(apic_id) => pcr::send_nmi_to_cpu(apic_id),
        None => release_probe(target),
    }
}

// ---------------------------------------------------------------------------
// Wait-for graph
// ---------------------------------------------------------------------------
//
// The graph is over CPU indices, never over lock pointers. A spinner holds
// `&self` on the lock it is waiting for, so reading that lock's holder is
// sound — but a *walker* on another CPU following a published pointer is
// not: the spinner can win, release and free the lock between the walker's
// two loads. The fault that would follow lands in an NMI handler, whose own
// `iretq` unblocks NMI mid-handler. Publishing the holder's CPU index
// instead makes the walk a bounded array traversal with nothing to
// dereference.

/// Publish that this CPU has begun spinning on the lock at `lock_addr`.
///
/// Returns `false` if a wait is already published. The contended-spin relax
/// hook takes a lock of its own, and it is the outer wait that describes
/// why this CPU is stuck.
pub fn begin_wait(lock_addr: u64) -> bool {
    let Some(slot) = SLOTS.get(pcr::get_current_cpu()) else {
        return false;
    };
    let seq = slot.wait_seq.load(Ordering::Relaxed);
    if seq % 2 == 1 {
        return false;
    }
    slot.waiting_on.store(lock_addr, Ordering::Relaxed);
    slot.blocked_on.store(NO_CPU, Ordering::Relaxed);
    slot.wait_seq.store(seq.wrapping_add(1), Ordering::Release);
    true
}

/// Republish which CPU holds the lock this one is waiting for. Called as
/// the spin progresses, because the holder changes as the queue drains.
pub fn publish_wait_holder(holder: Option<usize>) {
    let Some(slot) = SLOTS.get(pcr::get_current_cpu()) else {
        return;
    };
    let next = holder.map_or(NO_CPU, |cpu| cpu as u32);
    if slot.blocked_on.load(Ordering::Relaxed) == next {
        return;
    }
    // A seqlock write: `wait_seq` goes even for the duration, so a walker
    // spanning the update either breaks on the even parity or sees a
    // changed sequence and rejects. Without it `wait_seq` would prove only
    // that each CPU stayed in *some* wait, not that the edges it published
    // were current at one instant — and a cycle assembled from two edges a
    // spin iteration apart is not a proof.
    let seq = slot.wait_seq.load(Ordering::Relaxed);
    slot.wait_seq.store(seq.wrapping_add(1), Ordering::Release);
    slot.blocked_on.store(next, Ordering::Release);
    slot.wait_seq.store(seq.wrapping_add(2), Ordering::Release);
}

/// Retract this CPU's wait.
pub fn end_wait() {
    if let Some(slot) = SLOTS.get(pcr::get_current_cpu()) {
        slot.blocked_on.store(NO_CPU, Ordering::Relaxed);
        slot.waiting_on.store(0, Ordering::Relaxed);
        let seq = slot.wait_seq.load(Ordering::Relaxed);
        slot.wait_seq.store(seq.wrapping_add(1), Ordering::Release);
    }
}

fn collect_wait_chain(start: usize, hops: &mut [WaitHop; MAX_WAIT_HOPS]) -> (usize, bool) {
    let mut cpu = start;
    let mut len = 0;
    while len < MAX_WAIT_HOPS {
        let Some(slot) = SLOTS.get(cpu) else {
            break;
        };
        let seq = slot.wait_seq.load(Ordering::Acquire);
        if seq % 2 == 0 {
            // Either not spinning — stuck somewhere the graph cannot
            // describe, or running normally — or mid-update, whose edge is
            // not safe to read.
            break;
        }
        let next = slot.blocked_on.load(Ordering::Acquire);
        if next == NO_CPU {
            break;
        }
        hops[len] = WaitHop {
            cpu: cpu as u32,
            seq,
            lock: slot.waiting_on.load(Ordering::Relaxed),
        };
        len += 1;
        cpu = next as usize;
        if cpu == start {
            return (len, true);
        }
    }
    (len, false)
}

/// Whether the wait-for chain from `start` returns to `start` with every
/// link still in the wait it was read in.
///
/// The re-read is what separates a proof from a coincidence: each hop is
/// two unsynchronised loads, so a chain can otherwise be assembled from
/// links that never existed at the same instant.
pub fn wait_chain_closes_cycle(start: usize) -> bool {
    let mut hops = [WaitHop {
        cpu: 0,
        seq: 0,
        lock: 0,
    }; MAX_WAIT_HOPS];
    let (len, closed) = collect_wait_chain(start, &mut hops);
    closed
        && hops[..len].iter().all(|hop| {
            SLOTS
                .get(hop.cpu as usize)
                .map(|slot| slot.wait_seq.load(Ordering::Acquire) == hop.seq)
                .unwrap_or(false)
        })
}

/// Print the wait-for chain from `start`.
///
/// Ends with an explicit terminator rather than trailing off: a chain can
/// leave the graph entirely, because `PreemptMutex`, `IrqRwLock` (whose
/// readers are a count, not an owner), `Mutex` and the klog ticket pair
/// publish no holder. "Holder unknown" and "no cycle" are different
/// answers and the reader is entitled to know which one this is.
pub fn dump_wait_chain(start: usize) {
    let mut hops = [WaitHop {
        cpu: 0,
        seq: 0,
        lock: 0,
    }; MAX_WAIT_HOPS];
    let (len, closed) = collect_wait_chain(start, &mut hops);
    if len == 0 {
        nmi_emit_line("WATCHDOG:   not spinning on a tracked lock");
        return;
    }
    for hop in &hops[..len] {
        nmi_emit("WATCHDOG:   cpu ");
        nmi_emit_dec(hop.cpu as u64);
        nmi_emit(" waits on lock ");
        nmi_emit_hex(hop.lock);
        nmi_emit_line("");
    }
    if closed {
        nmi_emit_line("WATCHDOG:   chain closes on itself — deadlock cycle");
    } else if len == MAX_WAIT_HOPS {
        nmi_emit_line("WATCHDOG:   chain truncated, no cycle within the bound");
    } else {
        nmi_emit_line("WATCHDOG:   chain ends: holder unknown");
    }
}

// ---------------------------------------------------------------------------
// NMI disposition
// ---------------------------------------------------------------------------

/// Claim `target`'s probe slot. Fails if one is already in flight, which is
/// what stops a per-tick check from storming a CPU that is still emitting
/// its dump.
pub fn arm_probe(target: usize, disposition: NmiDisposition) -> bool {
    let Some(slot) = SLOTS.get(target) else {
        return false;
    };
    slot.probe
        .compare_exchange(
            NmiDisposition::Unsolicited as u32,
            disposition as u32,
            Ordering::AcqRel,
            Ordering::Relaxed,
        )
        .is_ok()
}

/// What the NMI this CPU is taking was sent for.
pub fn probe_disposition(cpu: usize) -> NmiDisposition {
    SLOTS
        .get(cpu)
        .map(|slot| NmiDisposition::from_raw(slot.probe.load(Ordering::Acquire)))
        .unwrap_or(NmiDisposition::Unsolicited)
}

/// Free `cpu`'s probe slot. The handler's last act, so the next check can
/// arm a fresh one.
pub fn release_probe(cpu: usize) {
    if let Some(slot) = SLOTS.get(cpu) {
        slot.probe
            .store(NmiDisposition::Unsolicited as u32, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Write a fragment to the serial console from NMI or spin-stall context.
///
/// Deliberately not `klog!` and not `early_console::write_bytes`. The
/// former's serial backend spins on a blocking ticket lock that the
/// interrupted CPU may already hold; the latter funnels through
/// `fblog::capture`, whose `try_lock` runs `push_lock`, which takes a
/// `&mut` on a per-CPU cell this NMI may have interrupted mid-update.
/// `early_console::write_byte` polls the UART and touches nothing else.
pub fn nmi_emit(text: &str) {
    for byte in text.as_bytes() {
        if *byte == b'\n' {
            crate::early_console::write_byte(b'\r');
        }
        crate::early_console::write_byte(*byte);
    }
}

/// [`nmi_emit`] plus a newline.
pub fn nmi_emit_line(text: &str) {
    nmi_emit(text);
    nmi_emit("\n");
}

/// Emit `value` in decimal. Format-free: `core::fmt` on this path would
/// pull in machinery that allocates stack the interrupted context may not
/// have.
pub fn nmi_emit_dec(value: u64) {
    let mut buf = [0u8; 20];
    let mut len = 0;
    let mut rest = value;
    loop {
        buf[len] = b'0' + (rest % 10) as u8;
        len += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    while len > 0 {
        len -= 1;
        crate::early_console::write_byte(buf[len]);
    }
}

/// Emit `value` as `0x`-prefixed hex.
pub fn nmi_emit_hex(value: u64) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    nmi_emit("0x");
    for shift in (0..16).rev() {
        let nibble = ((value >> (shift * 4)) & 0xF) as usize;
        crate::early_console::write_byte(DIGITS[nibble]);
    }
}

/// Print the worst stall each CPU was ever observed in, in samples.
///
/// The honest way to size a threshold: measure what the kernel actually
/// produces rather than infer it from whichever bug report arrived last.
pub fn report_max_stalls() {
    let count = pcr::get_cpu_count().min(MAX_CPUS);
    for cpu in 0..count {
        let Some(slot) = SLOTS.get(cpu) else { continue };
        let max = slot.max_stale.load(Ordering::Relaxed);
        if max == 0 {
            continue;
        }
        nmi_emit("WATCHDOG: cpu ");
        nmi_emit_dec(slot.target.load(Ordering::Relaxed) as u64);
        nmi_emit(" worst observed stall ");
        nmi_emit_dec(max as u64);
        nmi_emit(" samples (watcher cpu ");
        nmi_emit_dec(cpu as u64);
        nmi_emit_line(")");
    }
}

#[cfg(any(test, feature = "test-helpers"))]
pub mod test_support {
    use super::*;

    /// Drive one watcher sample of `target` with an injected heartbeat,
    /// returning the resulting consecutive-stale count.
    ///
    /// The real [`check_neighbour`] reads the heartbeat out of a live PCR,
    /// which a host test has no way to move; the state machine it drives is
    /// the part worth pinning.
    pub fn sample(watcher: usize, beat: u64, threshold: u32) -> u32 {
        let slot = &SLOTS[watcher];
        if beat != slot.last_seen.load(Ordering::Relaxed) {
            slot.last_seen.store(beat, Ordering::Relaxed);
            slot.stale.store(0, Ordering::Relaxed);
            slot.next_report.store(threshold, Ordering::Relaxed);
            return 0;
        }
        let stale = slot.stale.load(Ordering::Relaxed).saturating_add(1);
        slot.stale.store(stale, Ordering::Relaxed);
        stale
    }

    /// Plant a wait-for edge as if `cpu` were spinning on a lock held by
    /// `blocked_on`. The real publishers only ever describe the CPU they
    /// run on, so a graph with more than one node has no other way to
    /// exist in a single-threaded test.
    pub fn plant_wait(cpu: usize, blocked_on: Option<usize>, lock: u64) {
        let slot = &SLOTS[cpu];
        slot.waiting_on.store(lock, Ordering::Relaxed);
        slot.blocked_on
            .store(blocked_on.map_or(NO_CPU, |c| c as u32), Ordering::Relaxed);
        let seq = slot.wait_seq.load(Ordering::Relaxed);
        if seq % 2 == 0 {
            slot.wait_seq.store(seq + 1, Ordering::Relaxed);
        }
    }

    /// Leave `cpu` in the middle of republishing its edge, the state
    /// [`publish_wait_holder`] passes through.
    pub fn plant_mid_update(cpu: usize, blocked_on: usize) {
        let slot = &SLOTS[cpu];
        slot.blocked_on.store(blocked_on as u32, Ordering::Relaxed);
        let seq = slot.wait_seq.load(Ordering::Relaxed);
        if seq % 2 == 1 {
            slot.wait_seq.store(seq + 1, Ordering::Relaxed);
        }
    }

    /// Retract a planted edge, as leaving the wait would.
    pub fn clear_wait(cpu: usize) {
        let slot = &SLOTS[cpu];
        slot.blocked_on.store(NO_CPU, Ordering::Relaxed);
        slot.waiting_on.store(0, Ordering::Relaxed);
        let seq = slot.wait_seq.load(Ordering::Relaxed);
        if seq % 2 == 1 {
            slot.wait_seq.store(seq + 1, Ordering::Relaxed);
        }
    }

    pub fn reset_slot(watcher: usize) {
        let slot = &SLOTS[watcher];
        slot.last_seen.store(0, Ordering::Relaxed);
        slot.stale.store(0, Ordering::Relaxed);
        slot.next_report.store(0, Ordering::Relaxed);
        slot.max_stale.store(0, Ordering::Relaxed);
        slot.target.store(NO_CPU, Ordering::Relaxed);
        slot.probe
            .store(NmiDisposition::Unsolicited as u32, Ordering::Relaxed);
        slot.blocked_on.store(NO_CPU, Ordering::Relaxed);
        slot.waiting_on.store(0, Ordering::Relaxed);
        slot.wait_seq.store(0, Ordering::Relaxed);
    }
}
