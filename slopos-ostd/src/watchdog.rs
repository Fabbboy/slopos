//! Cross-CPU lockup detector.
//!
//! Each CPU watches the next eligible one and compares its progress
//! counter — [`ProcessorControlRegion::heartbeat`] — against the watcher's
//! *own* previous reading. N consecutive unchanged samples is a breach.
//!
//! There is no clock in that predicate: a wall-time threshold has to be
//! calibrated against the slowest machine the kernel will ever run on, and
//! emulation and host steal time both stretch that without bound. A host that
//! stalls the target stalls the watcher identically.
//!
//! A breach means "this CPU has not taken a timer interrupt recently", not
//! that it is stopped — a CPU spinning on a lock with interrupts masked is
//! executing hard. So the first breach only reports; only a sustained breach
//! is fatal.
//!
//! [`ProcessorControlRegion::heartbeat`]: crate::cpu::x86_64::pcr::ProcessorControlRegion::heartbeat

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::cpu::x86_64::pcr::{self, MAX_CPUS};

/// Consecutive unchanged samples before a CPU is reported; one second at the
/// 100 Hz LAPIC tick. Not less: the kernel has legitimate interrupts-off
/// sections in the hundreds of milliseconds, and the LAPIC timer tests stop
/// the timer outright.
pub const DEFAULT_MISS_THRESHOLD: u32 = 100;

/// Multiple of the miss threshold at which a breach becomes fatal.
const FATAL_MULTIPLE: u32 = 5;

/// The same, once the wait-for chain has closed on itself. A cycle cannot
/// resolve on its own, so waiting longer only delays the report.
const FATAL_MULTIPLE_CYCLE: u32 = 1;

/// No CPU. `u32::MAX` rather than 0, which is a real CPU index.
const NO_CPU: u32 = u32::MAX;

static ENABLED: AtomicBool = AtomicBool::new(true);
const PANIC_OVERRIDE_UNSET: u32 = 0;
const PANIC_OVERRIDE_ON: u32 = 1;
const PANIC_OVERRIDE_OFF: u32 = 2;

static PANIC_OVERRIDE: AtomicU32 = AtomicU32::new(PANIC_OVERRIDE_UNSET);
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
    /// An operator asked, through the diagnostic console, for this CPU to
    /// describe itself. Dump context and resume.
    ///
    /// Distinct from [`Self::Report`] because it is not evidence of a fault:
    /// it must not spend the recovered-fault budget `panic.oops_limit=` bounds.
    Probe = 4,
}

impl NmiDisposition {
    fn from_raw(raw: u32) -> Self {
        match raw {
            1 => Self::Report,
            2 => Self::Fatal,
            3 => Self::TlbLadder,
            4 => Self::Probe,
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
    /// Largest `stale` ever observed, packed with the CPU it was observed
    /// against: `target` moves when this watcher retargets, so a maximum
    /// paired with the current target names the wrong CPU.
    worst_stall: AtomicU64,
    /// The CPU this one watches, cached across ticks.
    target: AtomicU32,
    /// Disposition of the NMI this CPU is being sent, and the interlock
    /// that stops a second one arriving while the first is being handled.
    probe: AtomicU32,
    /// Odd while this CPU is spinning on a lock. A walker records it per hop
    /// and re-reads it afterwards, so a chain assembled out of links that were
    /// released and re-taken underneath it is rejected.
    wait_seq: AtomicU64,
    /// Holder CPU of the lock this one is spinning on; `NO_CPU` otherwise.
    blocked_on: AtomicU32,
    /// Address of that lock. Printed, never dereferenced.
    waiting_on: AtomicU64,
    /// `rip` the last NMI probe stopped this CPU at, or 0. The handler's own
    /// emitters go straight to the UART, which a machine may not have, so the
    /// answer is left where a normal-context reader can format it.
    probe_rip: AtomicU64,
}

/// Longest wait-for chain the walker will follow. A real deadlock cycle is
/// short; a chain this long is contention, and the bound keeps the walker's
/// frame small enough to run from a stalled spin loop.
pub const MAX_WAIT_HOPS: usize = 8;

/// One step of a wait-for chain: `cpu` is spinning on `lock`.
#[derive(Clone, Copy)]
pub struct WaitHop {
    pub cpu: u32,
    pub seq: u64,
    pub lock: u64,
}

impl CpuSlot {
    const fn new() -> Self {
        Self {
            last_seen: AtomicU64::new(0),
            stale: AtomicU32::new(0),
            next_report: AtomicU32::new(0),
            worst_stall: AtomicU64::new(0),
            target: AtomicU32::new(NO_CPU),
            probe: AtomicU32::new(NmiDisposition::Unsolicited as u32),
            wait_seq: AtomicU64::new(0),
            blocked_on: AtomicU32::new(NO_CPU),
            waiting_on: AtomicU64::new(0),
            probe_rip: AtomicU64::new(0),
        }
    }
}

static SLOTS: [CpuSlot; MAX_CPUS] = [const { CpuSlot::new() }; MAX_CPUS];

static SNAPSHOT: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static SNAPSHOT_CLAIMED: AtomicBool = AtomicBool::new(false);
static SNAPSHOT_READY: AtomicBool = AtomicBool::new(false);

const fn pack_stall(target: u32, samples: u32) -> u64 {
    ((target as u64) << 32) | samples as u64
}

fn unpack_stall(packed: u64) -> Option<(usize, u32)> {
    let samples = packed as u32;
    (samples != 0).then(|| ((packed >> 32) as usize, samples))
}

/// Turn the detector off entirely (`watchdog=off`).
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Release);
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// Consecutive unchanged samples before a CPU is reported
/// (`watchdog.miss_threshold=`). Zero is rejected — it would report every tick.
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

/// `0` means "no override".
#[cfg(any(test, feature = "test-helpers"))]
static MISS_THRESHOLD_OVERRIDE: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];

#[cfg(any(test, feature = "test-helpers"))]
fn miss_threshold_for(target: usize) -> u32 {
    match MISS_THRESHOLD_OVERRIDE
        .get(target)
        .map_or(0, |cell| cell.load(Ordering::Acquire))
    {
        0 => miss_threshold(),
        samples => samples,
    }
}

/// Nothing can install a per-CPU override outside the tests build, so the
/// watchdog tick reads the machine-wide threshold directly.
#[cfg(not(any(test, feature = "test-helpers")))]
#[inline(always)]
fn miss_threshold_for(_target: usize) -> u32 {
    miss_threshold()
}

/// Judge one CPU on a shorter fuse than the rest of the machine, for the
/// token's lifetime.
#[cfg(any(test, feature = "test-helpers"))]
pub struct MissThresholdOverride {
    cpu: usize,
    previous: u32,
}

#[cfg(any(test, feature = "test-helpers"))]
impl MissThresholdOverride {
    pub fn for_cpu(cpu: usize, samples: u32) -> Option<Self> {
        if samples == 0 {
            return None;
        }
        let previous = MISS_THRESHOLD_OVERRIDE
            .get(cpu)?
            .swap(samples, Ordering::AcqRel);
        Some(Self { cpu, previous })
    }
}

#[cfg(any(test, feature = "test-helpers"))]
impl Drop for MissThresholdOverride {
    fn drop(&mut self) {
        if let Some(cell) = MISS_THRESHOLD_OVERRIDE.get(self.cpu) {
            cell.store(self.previous, Ordering::Release);
        }
    }
}

/// Whether a sustained breach may take the machine down
/// (`watchdog.panic=on|off`).
pub fn set_panic_enabled(enabled: bool) {
    PANIC_OVERRIDE.store(
        if enabled {
            PANIC_OVERRIDE_ON
        } else {
            PANIC_OVERRIDE_OFF
        },
        Ordering::Release,
    );
}

pub fn clear_panic_override() {
    PANIC_OVERRIDE.store(PANIC_OVERRIDE_UNSET, Ordering::Release);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanicOverride {
    Unset,
    ForcedOn,
    ForcedOff,
}

pub fn panic_override() -> PanicOverride {
    match PANIC_OVERRIDE.load(Ordering::Acquire) {
        PANIC_OVERRIDE_ON => PanicOverride::ForcedOn,
        PANIC_OVERRIDE_OFF => PanicOverride::ForcedOff,
        _ => PanicOverride::Unset,
    }
}

/// A watcher cannot tell a target the host descheduled from one that is wedged:
/// both stop bumping their heartbeat. Under a hypervisor the sustained breach is
/// therefore not evidence, and taking the machine down on it aborts a healthy
/// kernel. `watchdog.panic=` overrides in both directions.
pub const fn fatal_escalation_policy(configured: PanicOverride, hypervisor_present: bool) -> bool {
    match configured {
        PanicOverride::ForcedOn => true,
        PanicOverride::ForcedOff => false,
        PanicOverride::Unset => !hypervisor_present,
    }
}

pub fn fatal_escalation_permitted() -> bool {
    fatal_escalation_policy(
        panic_override(),
        crate::arch::x86_64::cpuid::hypervisor_present(),
    )
}

/// Record that this CPU has made progress.
///
/// > Touch only from a loop whose trip count is bounded by data already in
/// > hand, which acquires no lock and performs no wait. **Never from a wait
/// > loop.**
///
/// A touch inside a wait loop makes the CPU look alive precisely while it is
/// doing nothing, converting a real deadlock into a silent permanent hang.
#[inline]
pub fn touch() {
    pcr::heartbeat_bump();
}

/// Suppress watching of the current CPU for the token's lifetime — for code
/// that deliberately runs without timer ticks, such as stopping or masking the
/// LAPIC timer to test it.
///
/// Unlike a touch this cannot hide a deadlock: it is scoped, and a CPU that
/// wedges inside the scope stops running the `Drop` that ends it.
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

/// Take the calling CPU out of the watched set for good — for a path that
/// stops ticking and does not come back (shutdown, reboot, a permanent park).
///
/// Call it *before* the instruction that stops the ticks; the gap between the
/// two is a window in which a watcher reports a CPU that left on purpose.
pub fn leave_watched_set() {
    pcr::set_watchdog_suppressed(true);
}

impl Drop for Suppress {
    fn drop(&mut self) {
        // Bump before unsuppressing: the first sample after the scope would
        // otherwise compare against a reading taken before it and count stale.
        pcr::heartbeat_bump();
        pcr::set_watchdog_suppressed(self.previous);
    }
}

/// Timer-interrupt hook: record progress, then sample the neighbour.
///
/// From the timer interrupt rather than the idle loop — a CPU busy running a
/// task never reaches the idle loop, so an idle-only check leaves every loaded
/// CPU unwatched.
#[inline]
pub fn tick() {
    // Before any lock is taken, so a heartbeat never depends on acquiring one.
    pcr::heartbeat_bump();
    if !is_enabled() {
        return;
    }
    check_neighbour(pcr::get_current_cpu());
}

fn eligible(cpu: usize) -> bool {
    pcr::is_cpu_online(cpu) && pcr::timer_is_armed(cpu) && !pcr::watchdog_is_suppressed(cpu)
}

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
            reset(slot, 0, miss_threshold());
            return;
        };
        slot.target.store(found as u32, Ordering::Relaxed);
        reset(
            slot,
            pcr::heartbeat_for_cpu(found),
            miss_threshold_for(found),
        );
        return;
    };

    let stale = accumulate(
        slot,
        target,
        pcr::heartbeat_for_cpu(target),
        miss_threshold_for(target),
    );
    if stale == 0 || stale < slot.next_report.load(Ordering::Relaxed) {
        return;
    }
    slot.next_report
        .store(stale.saturating_mul(2), Ordering::Relaxed);
    report_stalled_cpu(me, target, stale);
}

pub fn watcher_of(target: usize) -> Option<usize> {
    let count = pcr::get_cpu_count().min(MAX_CPUS);
    (0..count).find(|&cpu| {
        SLOTS
            .get(cpu)
            .is_some_and(|slot| slot.target.load(Ordering::Relaxed) == target as u32)
    })
}

fn reset(slot: &CpuSlot, beat: u64, threshold: u32) {
    slot.last_seen.store(beat, Ordering::Relaxed);
    slot.stale.store(0, Ordering::Relaxed);
    slot.next_report.store(threshold, Ordering::Relaxed);
}

/// Fold one sample of `target` into `slot`, returning the consecutive-stale
/// count.
fn accumulate(slot: &CpuSlot, target: usize, beat: u64, threshold: u32) -> u32 {
    if beat != slot.last_seen.load(Ordering::Relaxed) {
        slot.last_seen.store(beat, Ordering::Relaxed);
        slot.stale.store(0, Ordering::Relaxed);
        slot.next_report.store(threshold, Ordering::Relaxed);
        return 0;
    }
    let stale = slot.stale.load(Ordering::Relaxed).saturating_add(1);
    slot.stale.store(stale, Ordering::Relaxed);
    if stale > slot.worst_stall.load(Ordering::Relaxed) as u32 {
        slot.worst_stall
            .store(pack_stall(target as u32, stale), Ordering::Relaxed);
    }
    stale
}

/// Announce the stall and NMI the target so it dumps its own context —
/// nobody else can see its registers.
fn report_stalled_cpu(me: usize, target: usize, stale: u32) {
    let cycle = wait_chain_closes_cycle(target);
    let fatal_at = miss_threshold_for(target).saturating_mul(if cycle {
        FATAL_MULTIPLE_CYCLE
    } else {
        FATAL_MULTIPLE
    });
    let disposition = if fatal_escalation_permitted() && stale >= fatal_at {
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
    // A suppressed abort must be visible, or the next reader concludes the
    // detector is broken.
    if stale >= fatal_at && disposition != NmiDisposition::Fatal {
        nmi_emit_line("WATCHDOG:   fatal escalation suppressed (hypervisor)");
    }
    dump_wait_chain(target);

    match pcr::apic_id_from_cpu_index(target) {
        Some(apic_id) => pcr::send_nmi_to_cpu(apic_id),
        None => release_probe(target),
    }
}

// The wait-for graph is over CPU indices, never over lock pointers: a walker
// on another CPU can be following a pointer the spinner already won, released
// and freed, and that fault lands in an NMI handler, whose own `iretq`
// unblocks NMI mid-handler.

/// Publish that this CPU has begun spinning on the lock at `lock_addr`.
///
/// Returns `false` if a wait is already published: the contended-spin relax
/// hook takes a lock of its own, and it is the outer wait that describes why
/// this CPU is stuck.
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
    // spanning the update either breaks on the even parity or sees a changed
    // sequence and rejects. Without it a cycle could be assembled from two
    // edges published a spin iteration apart.
    let seq = slot.wait_seq.load(Ordering::Relaxed);
    slot.wait_seq.store(seq.wrapping_add(1), Ordering::Release);
    slot.blocked_on.store(next, Ordering::Release);
    slot.wait_seq.store(seq.wrapping_add(2), Ordering::Release);
}

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
            // Either not spinning at all, or mid-update, whose edge is not
            // safe to read.
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

/// Whether the wait-for chain from `start` returns to `start` with every link
/// still in the wait it was read in.
///
/// Each hop is two unsynchronised loads, so without the re-read a chain can be
/// assembled from links that never existed at the same instant.
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

/// How a wait-for chain ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainEnd {
    /// Not spinning on a lock the graph tracks.
    NotWaiting,
    /// The chain returns to a CPU already on it: a deadlock cycle.
    Cycle,
    /// Stopped at [`MAX_WAIT_HOPS`] without closing. Contention, not a cycle.
    Truncated,
    /// Ran out of holders. `PreemptMutex`, `IrqRwLock`, `Mutex` and the klog
    /// ticket pair publish none, so this is "unknown", not "none".
    HolderUnknown,
}

/// Snapshot the wait-for chain from `start`: the data-returning form of
/// [`dump_wait_chain`], for a caller that formats through a console rather
/// than through the NMI-safe emitters.
pub fn wait_chain_snapshot(start: usize, out: &mut [WaitHop; MAX_WAIT_HOPS]) -> (usize, ChainEnd) {
    let (len, closed) = collect_wait_chain(start, out);
    let end = if len == 0 {
        ChainEnd::NotWaiting
    } else if closed {
        ChainEnd::Cycle
    } else if len == MAX_WAIT_HOPS {
        ChainEnd::Truncated
    } else {
        ChainEnd::HolderUnknown
    };
    (len, end)
}

/// The worst stall `watcher` ever observed, in samples, and who it was
/// watching; `None` if that watcher never saw its target stall.
pub fn max_stall(watcher: usize) -> Option<(usize, u32)> {
    unpack_stall(SLOTS.get(watcher)?.worst_stall.load(Ordering::Relaxed))
}

/// Print the wait-for chain from `start`, ending with an explicit terminator:
/// "holder unknown" and "no cycle" are different answers.
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

/// Called from the NMI handler.
pub fn note_probe_rip(cpu: usize, rip: u64) {
    if let Some(slot) = SLOTS.get(cpu) {
        slot.probe_rip.store(rip, Ordering::Release);
    }
}

/// `None` if `cpu` never answered a probe.
pub fn probe_rip(cpu: usize) -> Option<u64> {
    let rip = SLOTS.get(cpu)?.probe_rip.load(Ordering::Acquire);
    (rip != 0).then_some(rip)
}

/// What the NMI this CPU is taking was sent for.
pub fn probe_disposition(cpu: usize) -> NmiDisposition {
    SLOTS
        .get(cpu)
        .map(|slot| NmiDisposition::from_raw(slot.probe.load(Ordering::Acquire)))
        .unwrap_or(NmiDisposition::Unsolicited)
}

/// Free `cpu`'s probe slot only if it still holds `expected`.
///
/// For reaping a probe whose target never answered: between the timeout and
/// the release the detector may have armed [`NmiDisposition::Fatal`] on the
/// same slot, and clearing that would let a stale NMI arrive at a re-armed
/// slot. A failed exchange means the slot is no longer ours to touch.
pub fn release_probe_if(cpu: usize, expected: NmiDisposition) -> bool {
    let Some(slot) = SLOTS.get(cpu) else {
        return false;
    };
    slot.probe
        .compare_exchange(
            expected as u32,
            NmiDisposition::Unsolicited as u32,
            Ordering::AcqRel,
            Ordering::Relaxed,
        )
        .is_ok()
}

/// Free `cpu`'s probe slot. The handler's last act, so the next check can
/// arm a fresh one.
pub fn release_probe(cpu: usize) {
    if let Some(slot) = SLOTS.get(cpu) {
        slot.probe
            .store(NmiDisposition::Unsolicited as u32, Ordering::Release);
    }
}

/// Write a fragment to the serial console from NMI or spin-stall context.
///
/// Deliberately not `klog!`, whose serial backend spins on a blocking ticket
/// lock the interrupted CPU may already hold, and not
/// `early_console::write_bytes`, which funnels through `fblog::capture` and
/// its `push_lock` — `cli` does not mask an NMI, so that held-stack update
/// cannot be made atomic against this context.
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

/// Emit `value` in decimal. Format-free: `core::fmt` would pull in machinery
/// that allocates stack the interrupted context may not have.
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

/// Freeze the per-watcher maxima for a later [`report_max_stalls`].
///
/// Shutdown runs a long interrupts-off tail on the CPU it is reporting about,
/// so a summary read after that tail measures the shutdown path rather than
/// the steady state. Call this when the request is accepted. First caller wins.
pub fn snapshot_max_stalls() {
    if SNAPSHOT_CLAIMED.swap(true, Ordering::AcqRel) {
        return;
    }
    for cpu in 0..MAX_CPUS {
        let packed = SLOTS
            .get(cpu)
            .map_or(0, |slot| slot.worst_stall.load(Ordering::Relaxed));
        SNAPSHOT[cpu].store(packed, Ordering::Relaxed);
    }
    SNAPSHOT_READY.store(true, Ordering::Release);
}

/// Print the worst interrupts-off section each CPU was observed in, reading
/// [`snapshot_max_stalls`]'s frozen copy when one was taken.
pub fn report_max_stalls() {
    if !is_enabled() {
        return;
    }
    let snapshot = SNAPSHOT_READY.load(Ordering::Acquire);
    let count = pcr::get_cpu_count().min(MAX_CPUS);
    for cpu in 0..count {
        let packed = if snapshot {
            SNAPSHOT[cpu].load(Ordering::Relaxed)
        } else {
            SLOTS
                .get(cpu)
                .map_or(0, |slot| slot.worst_stall.load(Ordering::Relaxed))
        };
        let Some((target, samples)) = unpack_stall(packed) else {
            continue;
        };
        nmi_emit("WATCHDOG: max interrupts-off cpu ");
        nmi_emit_dec(target as u64);
        nmi_emit(": ");
        nmi_emit_dec(samples as u64);
        nmi_emit(" of ");
        nmi_emit_dec(miss_threshold_for(target) as u64);
        nmi_emit(" samples before report (watcher cpu ");
        nmi_emit_dec(cpu as u64);
        nmi_emit_line(")");
    }
}

#[cfg(any(test, feature = "test-helpers"))]
pub mod test_support {
    use super::*;

    /// Drive one watcher sample of `target` with an injected heartbeat,
    /// returning the resulting consecutive-stale count. The real
    /// [`check_neighbour`] reads a live PCR a host test has no way to move.
    pub fn sample_of(watcher: usize, target: usize, beat: u64, threshold: u32) -> u32 {
        accumulate(&SLOTS[watcher], target, beat, threshold)
    }

    pub fn sample(watcher: usize, beat: u64, threshold: u32) -> u32 {
        sample_of(watcher, watcher, beat, threshold)
    }

    /// Retarget `watcher` as [`check_neighbour`] does when its target stops
    /// being eligible, without needing a live PCR.
    pub fn retarget(watcher: usize, target: usize, beat: u64) {
        let slot = &SLOTS[watcher];
        slot.target.store(target as u32, Ordering::Relaxed);
        reset(slot, beat, miss_threshold_for(target));
    }

    /// Plant a wait-for edge as if `cpu` were spinning on a lock held by
    /// `blocked_on`. The real publishers only describe the CPU they run on, so
    /// a multi-node graph cannot otherwise exist in a single-threaded test.
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

    /// Discard any frozen summary, so a test that takes one does not decide
    /// what later readers see.
    pub fn clear_snapshot() {
        SNAPSHOT_READY.store(false, Ordering::Release);
        SNAPSHOT_CLAIMED.store(false, Ordering::Release);
    }

    pub fn reset_slot(watcher: usize) {
        let slot = &SLOTS[watcher];
        slot.last_seen.store(0, Ordering::Relaxed);
        slot.stale.store(0, Ordering::Relaxed);
        slot.next_report.store(0, Ordering::Relaxed);
        slot.worst_stall.store(0, Ordering::Relaxed);
        slot.target.store(NO_CPU, Ordering::Relaxed);
        slot.probe
            .store(NmiDisposition::Unsolicited as u32, Ordering::Relaxed);
        slot.blocked_on.store(NO_CPU, Ordering::Relaxed);
        slot.waiting_on.store(0, Ordering::Relaxed);
        slot.wait_seq.store(0, Ordering::Relaxed);
    }
}
