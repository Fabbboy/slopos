//! Lock-ordering verification: a per-class dependency DAG with cycle
//! detection over the per-CPU held-lock stack.
//!
//! Class identity is the **declaration site**, via a [`LockClassKey`] minted
//! by [`lock_class!`](crate::lock_class), so an array of N like locks is one
//! class; the instance address is still recorded per held entry, because the
//! poison-unlock walk and the watchdog both name a lock by address.
//! `LOCK_LEVEL_*` is an advisory rank hint for diagnostics — the cycle check
//! is the enforcement.

use core::cell::UnsafeCell;
use core::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering,
};

use crate::cpu::x86_64::interrupts::{restore_flags, save_flags_cli};
use crate::cpu::x86_64::pcr::{MAX_CPUS, get_current_cpu};

use super::cpu_local::CacheAligned;

/// Maximum distinct lock classes (one per [`LockClassKey`] declaration
/// site, plus one per distinct subclass of a site).
///
/// Sized against the number of `lock_class!` sites, not the number of lock
/// instances. Measured high-water is 176; `lockdep_pool_headroom` fails the
/// suite above 70% fill, so slack past that only lets an array-of-locks
/// static that forgot its class key land unnoticed.
pub const MAX_CLASSES: usize = 512;

/// Class slots held back from [`register_class`] so the in-kernel lockdep
/// self-test always has headroom, even after the table has otherwise
/// overflowed.
///
/// Deliberately **not** `cfg`-gated: the class index at which the table fills
/// must be the same under `just test` and `just boot-log`, or the two boot
/// logs cannot be compared.
pub const RESERVED_TEST_CLASSES: usize = 4;

/// Slots [`register_class`] may allocate from. This, not [`MAX_CLASSES`], is
/// the denominator that describes real headroom.
pub const REGISTRABLE_CLASSES: usize = MAX_CLASSES - RESERVED_TEST_CLASSES;

/// Maximum dependency edges in the class graph.
///
/// Measured high-water is 197. Edge count moves with scheduling because an
/// edge records which *orders* were observed, not which locks exist.
pub const MAX_EDGES: usize = 1024;

/// Chain-hash cache slots. Each entry caches an already-validated chain
/// prefix so repeated acquisitions skip the BFS.
///
/// Measured high-water is 391. The cache is what makes a steady-state acquire
/// O(1), so this is sized to keep the miss rate at noise rather than to the
/// smallest table that fits.
pub const MAX_CHAINS: usize = 2048;

/// Number of buckets in the chain-key hash table. Must be a power of two.
pub const CHAIN_HASH_BUCKETS: usize = 256;

/// Number of buckets in the class-id hash table. Must be a power of two.
/// Probed on every acquire, so it tracks [`MAX_CLASSES`].
pub const CLASS_HASH_BUCKETS: usize = 512;

/// Maximum concurrently held locks per CPU.
///
/// Kept at 4x the observed high-water of 4 because the failure mode is
/// asymmetric: a push past this cap is dropped, and a dropped entry is
/// invisible to the poison walk and unfindable by `pop_lock`.
pub const MAX_HELD_LOCKS: usize = 16;

/// Maximum BFS frontier size during cycle check. Bounded so the search
/// never grows the stack, but large enough to hold every class: a frontier
/// that saturates would abandon the search and report "no cycle", which is
/// a false negative in the one tool whose job is to find them.
const MAX_BFS_FRONTIER: usize = MAX_CLASSES;

/// Sentinel value for "no class / null edge / empty bucket".
const NONE_IDX: u16 = u16::MAX;

const INITIAL_CHAIN_KEY: u64 = !0;

/// Advisory rank only: leaf locks are detected by *having no outgoing edges*,
/// not by their level number.
pub const LOCK_LEVEL_UNORDERED: u8 = 0;
pub const LOCK_LEVEL_RESOURCE: u8 = 1;
pub const LOCK_LEVEL_REGISTRY: u8 = 2;
pub const LOCK_LEVEL_ALLOCATOR: u8 = 3;
pub const LOCK_LEVEL_SCHEDULER: u8 = 4;

/// Sentinel level for synthetic Epoch classes pushed by
/// `crate::sync::epoch::Epoch::enter`. Not a real lock: the entry exists so
/// `push_lock` can detect a tracked lock being acquired while an epoch
/// read-side critical section is live, which would risk holding it across a
/// wake site and regress the atomic-publish invariant.
pub const LOCK_LEVEL_EPOCH: u8 = 0xFE;

/// Permit legitimate same-class nesting.
///
/// Two *different* instances of one declaration site share a class, so
/// holding two at once is a real AB-BA risk unless the site guarantees a
/// total order over its instances; this flag is how it says so. Re-acquiring
/// the *same* instance is recursion and is reported separately.
pub const LO_DUPOK: u32 = 1 << 0;

/// Suppress every ordering finding for this class. A blunt instrument: it
/// discards the check rather than expressing an ordering, so prefer
/// [`LO_DUPOK`] or a distinct class.
pub const LO_BLESSED: u32 = 1 << 2;

/// Latched in the class record's flags so an id-collision warning fires once,
/// not per acquire.
const LC_COLLISION_REPORTED: u32 = 1 << 16;

pub const ACQ_NONE: u8 = 0;

/// The primitive permits re-acquiring the *same instance* while it is held.
/// Suppresses the recursion report only; the ordering check against every
/// other held class still runs.
pub const ACQ_RECURSIVE: u8 = 1 << 0;

/// The compile-time identity of a lock **declaration site**.
///
/// Minted only by [`lock_class!`](crate::lock_class). One key exists per
/// source site, so every lock built from one expansion shares one class.
///
/// A release build may duplicate `.rodata` across crates, so [`id`](Self::id)
/// is the identity and this struct's address never is; duplicate copies are
/// recognised as one class by comparing [`name`](Self::name) and
/// [`site`](Self::site) by content.
///
/// Must never grow interior mutability: the macro interns it through
/// `const K: &'static LockClassKey = &…`, which rustc rejects (E0492) the
/// moment an `UnsafeCell` appears anywhere inside.
pub struct LockClassKey {
    /// Never zero — 0 is the free-slot sentinel.
    id: u64,
    name: &'static str,
    /// `file:line:column` of the `lock_class!` invocation.
    site: &'static str,
    /// [`LO_DUPOK`] / [`LO_BLESSED`].
    flags: u32,
    /// Advisory rank hint.
    level: u8,
}

impl LockClassKey {
    /// Mint a key. The id is derived from the site string here, in one place,
    /// so no caller can hand in an id that aliases another site's.
    #[doc(hidden)]
    pub const fn __from_site(
        name: &'static str,
        site: &'static str,
        level: u8,
        flags: u32,
    ) -> Self {
        Self {
            id: class_id(name, site),
            name,
            site,
            flags,
            level,
        }
    }

    #[inline]
    pub const fn id(&self) -> u64 {
        self.id
    }
    #[inline]
    pub const fn name(&self) -> &'static str {
        self.name
    }
    #[inline]
    pub const fn site(&self) -> &'static str {
        self.site
    }
    #[inline]
    pub const fn level(&self) -> u8 {
        self.level
    }
    #[inline]
    pub const fn flags(&self) -> u32 {
        self.flags
    }
}

/// FNV-1a over `site` then `name`, finished with splitmix64's avalanche.
///
/// FNV-1a alone is a poor fit: [`class_bucket`] folds the *high* bits, which
/// FNV-1a barely moves between inputs differing only in their last few
/// characters — exactly what consecutive `line!()` values look like. `name`
/// is folded in too, so two sites sharing a `file:line:column` still separate.
const fn class_id(name: &str, site: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

    const fn fnv1a(bytes: &[u8], seed: u64) -> u64 {
        let mut h = seed;
        let mut i = 0;
        while i < bytes.len() {
            h ^= bytes[i] as u64;
            h = h.wrapping_mul(FNV_PRIME);
            i += 1;
        }
        h
    }

    avalanche(fnv1a(name.as_bytes(), fnv1a(site.as_bytes(), FNV_OFFSET)))
}

/// splitmix64's finaliser. Maps 0 to 1 so the class table's free-slot
/// sentinel stays unambiguous.
const fn avalanche(mut h: u64) -> u64 {
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 31;
    if h == 0 { 1 } else { h }
}

/// Lookup id for `(key, subclass)`. Subclass 0 is the key's own id, so a
/// site that never nests pays nothing.
#[inline]
fn subclass_id(base: u64, subclass: u8) -> u64 {
    if subclass == 0 {
        base
    } else {
        avalanche(base ^ (subclass as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }
}

/// Mint the lock class for this declaration site.
///
/// Every lock built from one expansion shares one class:
/// `[const { SpinLock::new(x, lock_class!("FOO", L)) }; 256]` is 256 lock
/// instances and one class.
///
/// Expands to safe code, so `forbid(unsafe_code)` crates may invoke it, and
/// to a `const` item rather than a `static`, because a const context may name
/// a const but not a static (E0013).
///
/// A site key inside a **generic** function is one static shared by every
/// monomorphisation. Where the instantiations must not merge, the caller
/// passes the key in instead.
#[macro_export]
macro_rules! lock_class {
    ($name:expr, $level:expr) => {
        $crate::lock_class!($name, $level, 0u32)
    };
    ($name:expr, $level:expr, $flags:expr) => {{
        const __SLOPOS_LOCK_CLASS: &'static $crate::sync::lock_graph::LockClassKey =
            &$crate::sync::lock_graph::LockClassKey::__from_site(
                $name,
                ::core::concat!(
                    ::core::file!(),
                    ":",
                    ::core::line!(),
                    ":",
                    ::core::column!()
                ),
                $level,
                $flags,
            );
        __SLOPOS_LOCK_CLASS
    }};
}

/// Mint an [`Epoch`](crate::sync::Epoch) class. Separate from
/// [`lock_class!`] so a caller cannot give an epoch a level other than
/// [`LOCK_LEVEL_EPOCH`], which is what the epoch-scope check keys on.
#[macro_export]
macro_rules! epoch_class {
    ($name:expr) => {
        $crate::lock_class!($name, $crate::sync::lock_graph::LOCK_LEVEL_EPOCH)
    };
}

/// Poison-unlock callback for the panic-recovery held-stack walk.
///
/// # Safety
/// `addr` must point to a live lock matching the type encoded in the
/// closure.
pub type PoisonUnlockFn = unsafe fn(*const ());

/// One class record per `(declaration site, subclass)` pair.
///
/// All fields are interior-mutable so the class table can be initialised
/// lazily under the lock-free CAS protocol; the metadata is written once, on
/// first acquire of a lock built from the site.
struct LockClass {
    /// `subclass_id(key.id(), subclass)` — the class identity. 0 = free.
    id: AtomicU64,
    /// Diagnostics only; see [`LockClassKey`] for why the pointer is not the
    /// identity.
    key: AtomicPtr<LockClassKey>,
    /// First instance address seen, so a report can name a concrete lock.
    /// CAS-once.
    first_addr: AtomicU64,
    /// Advisory rank hint, copied from the key.
    level: AtomicU8,
    subclass: AtomicU8,
    /// `LO_*` copied from the key, plus [`LC_COLLISION_REPORTED`].
    flags: AtomicU32,
    /// Head of the singly-linked list of edges recording "lock A was acquired
    /// while this class was held". Index into EDGES; NONE_IDX = empty.
    edges_after_head: AtomicU16,
    /// Hash-bucket linkage (next class index in the same bucket).
    next_in_bucket: AtomicU16,
    /// Written only by the cold report path under [`REPORT_PATH_LOCK`], which
    /// keeps the path reconstruction's parent array out of a `#[cold]` frame
    /// that also carries `format_args!`.
    bfs_parent: AtomicU16,
    /// IRQ-context usage bits (reserved for future hardirq tracking).
    #[allow(dead_code)]
    usage_mask: AtomicU8,
}

impl LockClass {
    const fn empty() -> Self {
        Self {
            id: AtomicU64::new(0),
            key: AtomicPtr::new(core::ptr::null_mut()),
            first_addr: AtomicU64::new(0),
            level: AtomicU8::new(0),
            subclass: AtomicU8::new(0),
            flags: AtomicU32::new(0),
            edges_after_head: AtomicU16::new(NONE_IDX),
            next_in_bucket: AtomicU16::new(NONE_IDX),
            bfs_parent: AtomicU16::new(NONE_IDX),
            usage_mask: AtomicU8::new(0),
        }
    }

    fn key_ref(&self) -> Option<&'static LockClassKey> {
        let p = self.key.load(Ordering::Acquire);
        if p.is_null() {
            return None;
        }
        // SAFETY: only `register_class` and `reserve_self_test_class` write
        // this field, always from a `&'static LockClassKey`, and keys are
        // immutable for the kernel's lifetime.
        Some(unsafe { &*(p as *const LockClassKey) })
    }

    fn name(&self) -> &'static str {
        self.key_ref().map(|k| k.name()).unwrap_or("<anon>")
    }

    fn site(&self) -> &'static str {
        self.key_ref().map(|k| k.site()).unwrap_or("<none>")
    }
}

/// One edge in the dependency graph: "the source class was held when the
/// target class was acquired".
struct Edge {
    /// Class that was acquired. NONE_IDX if free.
    target: AtomicU16,
    /// Next edge in the source class's `edges_after` linked list.
    next: AtomicU16,
}

impl Edge {
    const fn empty() -> Self {
        Self {
            target: AtomicU16::new(NONE_IDX),
            next: AtomicU16::new(NONE_IDX),
        }
    }
}

/// One entry in the chain-hash cache: "this chain prefix has already been
/// validated; skip the BFS check on a hit."
struct Chain {
    /// Rolling 64-bit hash of the class indices in this chain.
    chain_key: AtomicU64,
    /// Hash-bucket linkage (next chain index in the same bucket).
    next_in_bucket: AtomicU16,
    /// Reserved for future depth/diagnostic.
    _depth: AtomicU8,
}

impl Chain {
    const fn empty() -> Self {
        Self {
            chain_key: AtomicU64::new(0),
            next_in_bucket: AtomicU16::new(NONE_IDX),
            _depth: AtomicU8::new(0),
        }
    }
}

#[derive(Clone, Copy)]
struct HeldLock {
    class_idx: u16,
    /// Instance address, for poison-walk dispatch and duplicate-acquire
    /// detection.
    lock_addr: *const (),
    /// Invoked during fatal-abort cleanup.
    poison_fn: PoisonUnlockFn,
    /// Chain key as it stood before this lock was pushed (for fast pop).
    prev_chain_key: u64,
    /// [`ACQ_RECURSIVE`] and friends, from the acquiring primitive.
    flags: u8,
}

impl HeldLock {
    const EMPTY: Self = Self {
        class_idx: NONE_IDX,
        lock_addr: core::ptr::null(),
        poison_fn: noop_poison,
        prev_chain_key: 0,
        flags: 0,
    };
}

struct HeldStack {
    entries: [HeldLock; MAX_HELD_LOCKS],
    /// Number of entries published in `entries`. Atomic because the watchdog
    /// wait-chain dump and the TLB ack-wait diagnostic read a *foreign* CPU's
    /// stack while its owner is writing it — a mask on this CPU says nothing
    /// about that one.
    depth: AtomicU32,
    /// Running chain key for the currently-held chain. Single-CPU.
    curr_chain_key: u64,
    /// Per-CPU rather than global: two global RMWs per acquire would distort
    /// the very cost these measure.
    chain_hits: AtomicU64,
    chain_misses: AtomicU64,
    /// High-water depth, and pushes [`push_held`] dropped for exceeding
    /// [`MAX_HELD_LOCKS`]. A dropped entry is invisible to the poison walk and
    /// cannot be found by `pop_lock`, so it must be counted.
    depth_max: AtomicU32,
    depth_overflows: AtomicU64,
    /// Releases whose address was not on the stack.
    pop_misses: AtomicU64,
    /// Entries removed by the poison walk, the legitimate source of a pop miss.
    poison_drained: AtomicU64,
}

impl HeldStack {
    const fn new() -> Self {
        Self {
            entries: [HeldLock::EMPTY; MAX_HELD_LOCKS],
            depth: AtomicU32::new(0),
            curr_chain_key: INITIAL_CHAIN_KEY,
            chain_hits: AtomicU64::new(0),
            chain_misses: AtomicU64::new(0),
            depth_max: AtomicU32::new(0),
            depth_overflows: AtomicU64::new(0),
            pop_misses: AtomicU64::new(0),
            poison_drained: AtomicU64::new(0),
        }
    }
}

/// Masks interrupts across a held-stack update.
///
/// An update is a multi-word entry write followed by a separate depth publish,
/// so an interrupt handler acquiring a tracked lock between the two takes the
/// slot the interrupted push had filled but not yet counted — leaving an entry
/// inside `depth` that no `pop_lock` can find. Preemption being disabled does
/// not exclude that: `PreemptMutex` and `Epoch::enter` both acquire with
/// interrupts on.
///
/// The mask spans the whole acquire — depth read, chain key, entry publish,
/// held-stack walk — reports included, so those two sites cannot ack a
/// TLB-shootdown IPI while one prints. Steady state is zero reports.
struct IrqOff(u64);

impl IrqOff {
    #[inline]
    fn enter() -> Self {
        Self(save_flags_cli())
    }
}

impl Drop for IrqOff {
    #[inline]
    fn drop(&mut self) {
        restore_flags(self.0);
    }
}

// Every access goes through a raw place expression rather than a `&mut`. The
// klog ring is itself a tracked lock, so a report — or the overflow warning —
// re-enters `push_lock` from inside a held-stack update; a reference minted
// here would still be live at that point, and two `&mut` to one object is
// undefined however the accesses interleave.

#[inline]
fn held(cpu: usize) -> *mut HeldStack {
    HELD[cpu].0.0.get()
}

/// Publish `entry` at the top of this CPU's stack.
///
/// The entry is written before the depth that publishes it, so a reader this
/// cannot mask — an NMI, or another CPU — sees a prefix of the truth rather
/// than a counted-but-unfilled slot.
#[inline]
fn push_held(cpu: usize, entry: HeldLock, new_chain_key: u64) {
    let _irq = IrqOff::enter();
    record_push_irq_state();
    let stack = held(cpu);
    // SAFETY: per-CPU slot; `_irq` excludes the interrupt handler that is the
    // only other party able to write it, and no reference is minted.
    unsafe {
        let depth = (*stack).depth.load(Ordering::Relaxed);
        if (depth as usize) >= MAX_HELD_LOCKS {
            (*stack).depth_overflows.fetch_add(1, Ordering::Relaxed);
            return;
        }
        (*stack).entries[depth as usize] = entry;
        core::sync::atomic::compiler_fence(Ordering::Release);
        (*stack).depth.store(depth + 1, Ordering::Relaxed);
        (*stack).curr_chain_key = new_chain_key;
        if depth + 1 > (*stack).depth_max.load(Ordering::Relaxed) {
            (*stack).depth_max.store(depth + 1, Ordering::Relaxed);
        }
    }
}

/// Build a held entry, reading the chain key this push follows.
#[inline]
fn held_entry(
    cpu: usize,
    class_idx: u16,
    lock_addr: *const (),
    poison_fn: PoisonUnlockFn,
    flags: u8,
) -> HeldLock {
    let _irq = IrqOff::enter();
    // SAFETY: as `push_held`.
    let prev_chain_key = unsafe { (*held(cpu)).curr_chain_key };
    HeldLock {
        class_idx,
        lock_addr,
        poison_fn,
        prev_chain_key,
        flags,
    }
}

/// Record an acquisition the validator is not checking — off, overflowed, or
/// a class that could not be registered. Leaves the chain key alone, so its
/// pop restores the key unchanged.
///
/// Recorded rather than dropped: the poison walk and every held-lock consumer
/// must still see that this CPU holds a lock, and a push with no matching pop
/// strands the depth for the rest of the boot.
#[inline]
fn push_untracked(cpu: usize, lock_addr: *const (), poison_fn: PoisonUnlockFn) {
    let entry = held_entry(cpu, NONE_IDX, lock_addr, poison_fn, 0);
    let key = entry.prev_chain_key;
    push_held(cpu, entry, key);
}

/// Depth as it stood before the caller's own push, for bounding the
/// validation walk over entries this CPU already holds.
#[inline]
fn held_depth(cpu: usize) -> u32 {
    // SAFETY: as `push_held`; a single aligned load needs no mask.
    unsafe { (*held(cpu)).depth.load(Ordering::Relaxed) }
}

/// One entry from this CPU's held stack.
///
/// Read element by element rather than through a slice: a slice is still one
/// allocation to the compiler, and handing out a reference into it while an
/// interrupt handler writes the array is the aliasing this module avoids.
#[inline]
fn held_entry_at(cpu: usize, index: usize) -> HeldLock {
    // SAFETY: per-CPU slot, `index < MAX_HELD_LOCKS`, and `HeldLock: Copy`
    // so the read borrows nothing.
    unsafe { (*held(cpu)).entries[index] }
}

struct PerCpuHeldStack(UnsafeCell<HeldStack>);

// SAFETY: each CPU touches only its own slot, and every mutation runs under
// [`IrqOff`], so the sole other party that could reach this CPU's slot — an
// interrupt handler acquiring a tracked lock — cannot land mid-update.
// `poison_unlock_all_held` walks only the panicking CPU's slot.
unsafe impl Sync for PerCpuHeldStack {}

struct ClassArray([LockClass; MAX_CLASSES]);
unsafe impl Sync for ClassArray {}

static CLASSES: ClassArray = ClassArray([const { LockClass::empty() }; MAX_CLASSES]);

/// Next class slot to allocate (monotonic; overflow disables the validator).
static CLASS_COUNT: AtomicU16 = AtomicU16::new(0);

struct ClassHash([AtomicU16; CLASS_HASH_BUCKETS]);
unsafe impl Sync for ClassHash {}

static CLASS_HASH: ClassHash = ClassHash([const { AtomicU16::new(NONE_IDX) }; CLASS_HASH_BUCKETS]);

struct EdgeArray([Edge; MAX_EDGES]);
unsafe impl Sync for EdgeArray {}

static EDGES: EdgeArray = EdgeArray([const { Edge::empty() }; MAX_EDGES]);
static EDGE_COUNT: AtomicU32 = AtomicU32::new(0);

struct ChainArray([Chain; MAX_CHAINS]);
unsafe impl Sync for ChainArray {}

static CHAINS: ChainArray = ChainArray([const { Chain::empty() }; MAX_CHAINS]);
static CHAIN_COUNT: AtomicU32 = AtomicU32::new(0);

struct ChainHash([AtomicU16; CHAIN_HASH_BUCKETS]);
unsafe impl Sync for ChainHash {}

static CHAIN_HASH: ChainHash = ChainHash([const { AtomicU16::new(NONE_IDX) }; CHAIN_HASH_BUCKETS]);

static HELD: [CacheAligned<PerCpuHeldStack>; MAX_CPUS] = {
    const INIT: CacheAligned<PerCpuHeldStack> =
        CacheAligned(PerCpuHeldStack(UnsafeCell::new(HeldStack::new())));
    [INIT; MAX_CPUS]
};

/// Master enable. When `false`, all hooks short-circuit. Production boot
/// flips this on after PCR init via [`enable_lock_tracking`].
static TRACKING_ENABLED: AtomicBool = AtomicBool::new(false);

/// Panic-mode bypass: ordering checks suppressed, held-stack walk for
/// poison-unlock still active.
static FATAL_BYPASS: AtomicBool = AtomicBool::new(false);

/// Overflow latch. Set if any pool fills; further events become no-ops
/// to prevent secondary panics during fatal-abort.
static GRAPH_OVERFLOW: AtomicBool = AtomicBool::new(false);

/// One-shot guard so the four latch sites emit exactly one line per boot.
static OVERFLOW_REPORTED: AtomicBool = AtomicBool::new(false);

/// Small because a declaration is a deliberate claim about two named classes,
/// not a per-site annotation.
const MAX_DECLARED_ORDERS: usize = 32;

/// One asserted `outer -> inner` ordering.
struct DeclaredOrder {
    outer: AtomicU16,
    inner: AtomicU16,
    /// Set once an acquisition took the pair in the declared direction.  A
    /// declaration never observed is a dead declaration.
    observed: AtomicBool,
}

impl DeclaredOrder {
    const fn empty() -> Self {
        Self {
            outer: AtomicU16::new(NONE_IDX),
            inner: AtomicU16::new(NONE_IDX),
            observed: AtomicBool::new(false),
        }
    }
}

static DECLARED: [DeclaredOrder; MAX_DECLARED_ORDERS] =
    [const { DeclaredOrder::empty() }; MAX_DECLARED_ORDERS];
static DECLARED_COUNT: AtomicU16 = AtomicU16::new(0);

/// Ordering violations reported since boot (cycle + nesting + recursion +
/// epoch). Self-test and warn-mode reports are counted separately in
/// [`REPORT_ONLY_VIOLATIONS`] so this one stays gateable.
static VIOLATION_COUNT: AtomicU32 = AtomicU32::new(0);

/// Violations reported while the reporter was in report-only mode (the
/// self-test, or [`LockdepMode::Warn`]).
static REPORT_ONLY_VIOLATIONS: AtomicU32 = AtomicU32::new(0);

/// What the validator does when it finds a violation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum LockdepMode {
    /// Track for the poison walk only: no class registration, no edges, no
    /// checks. Lets one binary measure its own validation cost.
    Off = 0,
    /// Report each distinct finding once and keep booting, so one boot
    /// enumerates every inversion in the tree.
    Warn = 1,
    /// Report and panic on the first finding.
    Panic = 2,
}

/// Defaults to [`LockdepMode::Panic`]. `enable_lock_tracking` runs before
/// the cmdline is parsed, so the handful of locks taken in that window are
/// validated under this default.
static LOCKDEP_MODE: AtomicU8 = AtomicU8::new(LockdepMode::Panic as u8);

/// Per-CPU report-in-progress latch.
///
/// Every diagnostic this module emits logs with `klog_warn!`, which takes
/// `KLOG_RING` — a `SpinLock`, whose acquire re-enters [`push_lock`]; warn
/// mode would recurse without bound. Raised, that re-entry still *records*
/// the acquisition — dropping it would strand the depth once the release
/// arrives — but runs no checks.
static IN_REPORT: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// Raises [`IN_REPORT`] across a klog call made from a validation path.
/// Restores the previous value rather than clearing, so a diagnostic nested
/// inside a report leaves the outer latch standing.
struct ReportLatch {
    cpu: usize,
    prev: bool,
}

impl ReportLatch {
    #[inline]
    fn raise(cpu: usize) -> Self {
        Self {
            cpu,
            prev: IN_REPORT[cpu].swap(true, Ordering::Relaxed),
        }
    }
}

impl Drop for ReportLatch {
    #[inline]
    fn drop(&mut self) {
        IN_REPORT[self.cpu].store(self.prev, Ordering::Relaxed);
    }
}

/// Serialises the cold path that reconstructs a cycle's route through
/// [`LockClass::bfs_parent`]. A plain test-and-set with no waiting, because a
/// raced route is a diagnostic imprecision: a second reporter prints
/// endpoints without a route.
static REPORT_PATH_LOCK: AtomicBool = AtomicBool::new(false);

/// Distinct `(kind, held class, new class)` triples reported. Far more than
/// a boot should produce; filling it means something is very wrong.
const MAX_VIOLATION_REPORTS: usize = 256;

struct ViolationKeys([AtomicU64; MAX_VIOLATION_REPORTS]);
// SAFETY: plain atomics; the wrapper exists only to name the array type.
unsafe impl Sync for ViolationKeys {}

static VIOLATION_KEYS: ViolationKeys =
    ViolationKeys([const { AtomicU64::new(0) }; MAX_VIOLATION_REPORTS]);

/// Distinct findings printed.
static VIOLATION_REPORTS: AtomicU32 = AtomicU32::new(0);

/// Set once the dedup table fills; printing stops so the boot can finish.
static VIOLATION_DEDUP_FULL: AtomicBool = AtomicBool::new(false);

/// Distinct class-id collisions observed.
static CLASS_COLLISIONS: AtomicU32 = AtomicU32::new(0);

/// Class slots abandoned to the registration race.
static CLASS_SLOTS_LEAKED: AtomicU32 = AtomicU32::new(0);

/// Puts the reporter in report-only mode for the lifetime of a
/// [`SelfTestGuard`], so the self-test can *provoke* a cycle and assert the
/// detector saw it without taking the machine down.
///
/// It grants nothing else: the self-test asserts [`validator_alive`] up front
/// rather than reaching past [`GRAPH_OVERFLOW`] or [`FATAL_BYPASS`], which
/// would let it pass on a validator that never ran.
#[cfg(any(test, feature = "test-helpers"))]
static SELF_TEST_ACTIVE: AtomicBool = AtomicBool::new(false);

#[cfg(any(test, feature = "test-helpers"))]
#[inline]
fn self_test_active() -> bool {
    SELF_TEST_ACTIVE.load(Ordering::Relaxed)
}

/// Production builds const-fold this to `false`, so the gates that consult it
/// cost nothing.
#[cfg(not(any(test, feature = "test-helpers")))]
#[inline(always)]
fn self_test_active() -> bool {
    false
}

/// Enable lock tracking. Call once after PCR init, before the first
/// SpinLock acquisition we want tracked. Idempotent.
pub fn enable_lock_tracking() {
    TRACKING_ENABLED.store(true, Ordering::Release);
}

/// Select what a violation does. Set from the `lockdep=` cmdline option.
pub fn set_lockdep_mode(mode: LockdepMode) {
    LOCKDEP_MODE.store(mode as u8, Ordering::Release);
}

#[inline]
fn lockdep_mode_raw() -> u8 {
    LOCKDEP_MODE.load(Ordering::Relaxed)
}

pub fn lockdep_mode() -> LockdepMode {
    match lockdep_mode_raw() {
        0 => LockdepMode::Off,
        1 => LockdepMode::Warn,
        _ => LockdepMode::Panic,
    }
}

/// Switch into panic-mode bypass: skip ordering checks (the kernel is
/// halting). The held-stack walk for poison-unlock still works. One-way
/// transition; the kernel never resumes from panic.
pub fn enter_fatal_bypass() {
    FATAL_BYPASS.store(true, Ordering::Release);
}

/// Locks currently held on the calling CPU. Advisory — read with preemption
/// potentially enabled.
#[inline]
pub fn held_lock_count() -> u32 {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) {
        return 0;
    }
    let cpu = get_current_cpu();
    // SAFETY: reading the depth field is a benign race; the value is
    // advisory for debug assertions only.
    unsafe { (*HELD[cpu].0.0.get()).depth.load(Ordering::Relaxed) }
}

/// Copy the addresses of the locks currently held on the calling CPU into
/// `out`, innermost-last. Returns how many entries were written. Advisory,
/// same benign-race caveat as [`held_lock_count`].
pub fn held_lock_addrs(out: &mut [u64]) -> usize {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) {
        return 0;
    }
    let cpu = get_current_cpu();
    // SAFETY: per-CPU slot; reads race only with this CPU's own
    // push/pop, and a torn snapshot is acceptable for diagnostics.
    let stack = unsafe { &*HELD[cpu].0.0.get() };
    let n = (stack.depth.load(Ordering::Relaxed) as usize).min(out.len());
    for (i, slot) in out.iter_mut().enumerate().take(n) {
        *slot = stack.entries[i].lock_addr as u64;
    }
    n
}

/// Names, not addresses: a bare-metal panic screen has no symbol table. Same
/// benign-race caveat as [`held_lock_addrs`].
pub fn for_each_held_lock_name(mut visit: impl FnMut(&'static str)) {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let cpu = get_current_cpu();
    // SAFETY: per-CPU slot; reads race only with this CPU's own push/pop, and a
    // torn snapshot is acceptable for diagnostics.
    let stack = unsafe { &*HELD[cpu].0.0.get() };
    let n = (stack.depth.load(Ordering::Relaxed) as usize).min(MAX_HELD_LOCKS);
    for entry in stack.entries.iter().take(n) {
        if entry.class_idx == NONE_IDX {
            visit("<untracked>");
            continue;
        }
        let key = CLASSES.0[entry.class_idx as usize]
            .key
            .load(Ordering::Relaxed);
        if key.is_null() {
            visit("<unregistered>");
            continue;
        }
        // SAFETY: `key` is published from a `&'static LockClassKey`.
        visit(unsafe { &*key }.name());
    }
}

/// Cross-CPU [`for_each_held_lock_name`]; the snapshot can tear.
pub fn for_each_held_lock_name_for_cpu(cpu: usize, mut visit: impl FnMut(&'static str)) {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) || cpu >= MAX_CPUS {
        return;
    }
    // SAFETY: read-only racy snapshot of another CPU's held stack; same
    // diagnostics-only caveat as `held_lock_addrs_for_cpu`.
    let stack = unsafe { &*HELD[cpu].0.0.get() };
    let n = (stack.depth.load(Ordering::Relaxed) as usize).min(MAX_HELD_LOCKS);
    for entry in stack.entries.iter().take(n) {
        if entry.class_idx == NONE_IDX {
            visit("<untracked>");
            continue;
        }
        let key = CLASSES.0[entry.class_idx as usize]
            .key
            .load(Ordering::Relaxed);
        if key.is_null() {
            visit("<unregistered>");
            continue;
        }
        // SAFETY: as in `for_each_held_lock_name`.
        visit(unsafe { &*key }.name());
    }
}

/// Cross-CPU variant of [`held_lock_addrs`] for post-mortem dumps. The target
/// CPU may be mid push/pop, so the snapshot can tear.
pub fn held_lock_addrs_for_cpu(cpu: usize, out: &mut [u64]) -> usize {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) || cpu >= MAX_CPUS {
        return 0;
    }
    // SAFETY: read-only racy snapshot of another CPU's held stack;
    // same diagnostics-only caveat as `held_lock_addrs`.
    let stack = unsafe { &*HELD[cpu].0.0.get() };
    let n = (stack.depth.load(Ordering::Relaxed) as usize).min(out.len());
    for (i, slot) in out.iter_mut().enumerate().take(n) {
        *slot = stack.entries[i].lock_addr as u64;
    }
    n
}

/// Record that the current CPU acquired a lock.
///
/// # Safety
/// Must be called with preemption disabled (which it is — inside
/// `SpinLock::lock`). `lock_addr` must point to a live lock; the
/// `poison_fn` closure must be able to poison-unlock the lock at that
/// address.
#[inline]
pub unsafe fn push_lock(
    lock_addr: *const (),
    poison_fn: PoisonUnlockFn,
    class: &'static LockClassKey,
) {
    // SAFETY: forwards the caller's contract unchanged.
    unsafe { push_lock_ex(lock_addr, poison_fn, class, 0, ACQ_NONE) }
}

/// [`push_lock`] with an explicit subclass and acquisition flags.
///
/// `subclass` splits one declaration site into distinct classes for
/// instances the site orders among themselves, so nesting them stays
/// *checked* rather than being waved through with [`LO_DUPOK`].
///
/// # Safety
/// As [`push_lock`].
#[inline]
pub unsafe fn push_lock_ex(
    lock_addr: *const (),
    poison_fn: PoisonUnlockFn,
    class: &'static LockClassKey,
    subclass: u8,
    acq_flags: u8,
) {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) {
        return;
    }

    let _irq = IrqOff::enter();
    let cpu = get_current_cpu();

    if lockdep_mode_raw() == LockdepMode::Off as u8 || GRAPH_OVERFLOW.load(Ordering::Relaxed) {
        push_untracked(cpu, lock_addr, poison_fn);
        return;
    }

    let class_idx = match register_class(class, subclass) {
        Some(idx) => idx,
        None => {
            // Record before warning: `latch_overflow` klogs, and the klog
            // ring's own acquire re-enters here onto a stack that must
            // already be whole.
            push_untracked(cpu, lock_addr, poison_fn);
            latch_overflow("class table full", lock_addr, class.level());
            return;
        }
    };

    let entry = held_entry(cpu, class_idx, lock_addr, poison_fn, acq_flags);
    let new_chain_key = iterate_chain_key(entry.prev_chain_key, class_idx);

    // This gate, not just the reporting ones, is what a latched bypass costs:
    // no edge is learned and no cycle is looked for, so the graph stops
    // growing as well as stops complaining.
    if FATAL_BYPASS.load(Ordering::Relaxed) {
        push_held(cpu, entry, new_chain_key);
        return;
    }

    if chain_lookup(new_chain_key) {
        bump_chain_hit(cpu);
        push_held(cpu, entry, new_chain_key);
        return;
    }
    bump_chain_miss(cpu);

    record_first_addr(class_idx, lock_addr);

    // Record the acquisition *before* running the checks: a report may panic,
    // and the poison walk on that unwind can only release locks the held
    // stack knows about.
    let depth_before = held_depth(cpu);
    push_held(cpu, entry, new_chain_key);

    if IN_REPORT[cpu].load(Ordering::Relaxed) {
        return;
    }

    let mut violated = false;

    // Any held entry at `LOCK_LEVEL_EPOCH` means an `Epoch::enter` is live on
    // this CPU. Checked before the regular scan so the diagnostic points at
    // the Epoch rather than at a downstream cycle edge.
    //
    // Skipped in interrupt context: the handler inherits the interrupted
    // context's entries but is not inside that epoch, so reporting it would
    // blame the interrupted code for what the handler did.
    if !crate::cpu::x86_64::pcr::in_interrupt_context() {
        for i in 0..depth_before as usize {
            let h = held_entry_at(cpu, i);
            if h.class_idx == NONE_IDX {
                continue;
            }
            let held_lvl = CLASSES.0[h.class_idx as usize]
                .level
                .load(Ordering::Relaxed);
            if held_lvl == LOCK_LEVEL_EPOCH {
                violated |= report_epoch_violation(class_idx, lock_addr, cpu, i + 1);
            }
        }
    }

    let class_flags = CLASSES.0[class_idx as usize].flags.load(Ordering::Relaxed);
    let mut top_class = NONE_IDX;
    for i in 0..depth_before as usize {
        let h = held_entry_at(cpu, i);
        top_class = h.class_idx;
        if h.class_idx == NONE_IDX {
            continue;
        }
        if h.class_idx == class_idx {
            if h.lock_addr == lock_addr {
                // A ticket lock cannot reach here on its own (the second
                // `lock()` spins on its own ticket), so this is a recursive
                // primitive or a lost pop. Both sides must declare themselves
                // recursive — checking only the incoming one would wave
                // through a write nested inside a read of the same
                // `IrqRwLock`, which deadlocks.
                let recursive_pair = h.flags & ACQ_RECURSIVE != 0 && acq_flags & ACQ_RECURSIVE != 0;
                if !recursive_pair {
                    violated |= report_recursion(class_idx, lock_addr, cpu, i + 1);
                }
            } else if class_flags & LO_DUPOK == 0 {
                violated |=
                    report_same_class_nesting(class_idx, lock_addr, h.lock_addr, cpu, i + 1);
            }
            continue;
        }
        if path_exists(class_idx, h.class_idx) {
            violated |= report_cycle(class_idx, lock_addr, cpu, i + 1);
        }
    }

    // Learn only from acquisitions that passed: an edge that closed a cycle
    // makes the graph cyclic and drowns later findings in derived noise, and a
    // cached chain short-circuits every later occurrence away entirely.
    if !violated {
        if depth_before > 0 {
            // A self-edge is meaningless and would burn a slot per nesting site.
            if top_class != NONE_IDX && top_class != class_idx {
                if let Err(()) = add_edge(top_class, class_idx) {
                    latch_overflow("edge pool full", lock_addr, class.level());
                }
                mark_declared_observed(top_class, class_idx);
            }
        }
        if let Err(()) = chain_insert(new_chain_key) {
            latch_overflow("chain cache full", lock_addr, class.level());
        }
    }
}

/// Record that the current CPU released a lock.
///
/// # Safety
/// Must be called with preemption disabled. `lock_addr` must match a
/// previous `push_lock` call.
#[inline]
pub unsafe fn pop_lock(lock_addr: *const ()) {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) {
        return;
    }

    let _irq = IrqOff::enter();
    let cpu = get_current_cpu();
    let stack = held(cpu);

    // SAFETY: per-CPU slot; `_irq` excludes the interrupt handler that is the
    // only other party able to write it, and no reference is minted.
    unsafe {
        let depth = (*stack).depth.load(Ordering::Relaxed);
        if depth == 0 {
            (*stack).pop_misses.fetch_add(1, Ordering::Relaxed);
            return;
        }

        // Retire the depth before clearing the slot, so a reader this cannot
        // mask — an NMI, or another CPU — never sees a counted-but-empty
        // entry, which is what a corrupt stack looks like.
        let top = (depth - 1) as usize;
        if (*stack).entries[top].lock_addr == lock_addr {
            (*stack).curr_chain_key = (*stack).entries[top].prev_chain_key;
            (*stack).depth.store(depth - 1, Ordering::Relaxed);
            core::sync::atomic::compiler_fence(Ordering::Release);
            (*stack).entries[top] = HeldLock::EMPTY;
            return;
        }

        // Out-of-order release: find and remove. The remaining stack is not
        // re-validated — the chain hash already attests to it.
        //
        // The shift moves `lock_addr` and `poison_fn` as separate words, so a
        // foreign CPU reading mid-shift can see a mismatched pair. Only the
        // owning CPU pairs them: `held_lock_addrs_for_cpu` reads addresses
        // alone, and the poison walk never runs on a foreign stack.
        for i in (0..depth as usize).rev() {
            if (*stack).entries[i].lock_addr != lock_addr {
                continue;
            }
            (*stack).depth.store(depth - 1, Ordering::Relaxed);
            core::sync::atomic::compiler_fence(Ordering::Release);
            for j in i..top {
                (*stack).entries[j] = (*stack).entries[j + 1];
            }
            (*stack).entries[top] = HeldLock::EMPTY;
            // Non-LIFO release breaks the invariant that `prev_chain_key`
            // fields chain back to the initial key; rebuild from scratch.
            let mut key = INITIAL_CHAIN_KEY;
            for j in 0..(depth as usize - 1) {
                key = iterate_chain_key(key, (*stack).entries[j].class_idx);
            }
            (*stack).curr_chain_key = key;
            return;
        }

        // The release is real but the entry is gone — the poison walk drained
        // it, or the push was dropped past `MAX_HELD_LOCKS`. Counted so a leak
        // is a number rather than a silence.
        (*stack).pop_misses.fetch_add(1, Ordering::Relaxed);
    }
}

/// Record that an Epoch read-side critical section opened on the current CPU.
///
/// Pushes a synthetic class entry tagged [`LOCK_LEVEL_EPOCH`]. `push_lock`
/// consults it on every subsequent acquisition and panics if any tracked lock
/// would be taken while the synthetic class is live.
///
/// # Safety
/// Caller must hold a `PreemptGuard` for the lifetime of the synthetic
/// entry (the embedded `RcuReadGuard` in `EpochGuard` provides this).
#[inline]
pub unsafe fn push_epoch(epoch_addr: *const (), class: &'static LockClassKey) {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) {
        return;
    }

    let cpu = get_current_cpu();

    if lockdep_mode_raw() == LockdepMode::Off as u8 || GRAPH_OVERFLOW.load(Ordering::Relaxed) {
        push_untracked(cpu, epoch_addr, noop_poison);
        return;
    }

    let class_idx = match register_class(class, 0) {
        Some(idx) => idx,
        None => {
            // Untracked, not absent: `pop_epoch` searches by address, and a
            // scope that pushed nothing would leave the next entry to be
            // popped in its place.
            push_untracked(cpu, epoch_addr, noop_poison);
            latch_overflow(
                "class table full (epoch enter)",
                epoch_addr,
                LOCK_LEVEL_EPOCH,
            );
            return;
        }
    };
    // Force the sentinel level regardless of what the key says, so a
    // hand-rolled `lock_class!` cannot mint a half-epoch that the
    // epoch-scope check would miss.
    CLASSES.0[class_idx as usize]
        .level
        .store(LOCK_LEVEL_EPOCH, Ordering::Relaxed);

    let entry = held_entry(cpu, class_idx, epoch_addr, noop_poison, 0);
    let new_chain_key = iterate_chain_key(entry.prev_chain_key, class_idx);
    push_held(cpu, entry, new_chain_key);
}

/// Record that an Epoch read-side critical section closed.
///
/// # Safety
/// Must be paired LIFO with the matching [`push_epoch`]. Preemption
/// must still be disabled (the `PreemptGuard` outlives this call by
/// construction in `EpochGuard::drop`).
#[inline]
pub unsafe fn pop_epoch(epoch_addr: *const ()) {
    // SAFETY: caller honours the LIFO + preempt-disabled contract.
    unsafe { pop_lock(epoch_addr) }
}

/// Walk the panicking CPU's held-lock stack, calling each entry's poison
/// callback in reverse order (innermost first).
///
/// **Draining is all this does.** Deciding that the kernel is dying is
/// [`enter_fatal_bypass`]'s job; `call_panic_cleanup` — the *recovered* path —
/// reaches here and must not latch, or the kernel resumes with every later
/// acquisition on every CPU unvalidated.
///
/// Cannot re-enter [`push_lock`]: every registered poison callback is pure
/// atomic stores — no lock, no klog, no allocation.
///
/// # Safety
/// Must only be called from panic recovery on the panicking CPU. All
/// recorded lock addresses must still be valid (true for static locks).
pub unsafe fn poison_unlock_all_held() {
    // SAFETY: as this function's own contract; drains to an empty stack.
    unsafe { poison_unlock_held_above(0) }
}

/// Snapshot this CPU's held depth, for pairing with
/// [`poison_unlock_held_above`].
pub fn held_depth_mark() -> u32 {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) {
        return 0;
    }
    let cpu = get_current_cpu();
    // SAFETY: per-CPU slot; a torn read is impossible for a single u32
    // written only by this CPU.
    unsafe { (*HELD[cpu].0.0.get()).depth.load(Ordering::Relaxed) }
}

/// Poison-unlock only the entries pushed above `mark`.
///
/// A recovery boundary nested inside another must not release the locks the
/// *outer* frame still holds: those guards are alive and their `Drop` will
/// release them again, and a ticket lock double-released admits two holders.
///
/// # Safety
/// As [`poison_unlock_all_held`]; `mark` must come from
/// [`held_depth_mark`] taken on this CPU earlier in the same call chain.
pub unsafe fn poison_unlock_held_above(mark: u32) {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let _irq = IrqOff::enter();
    let cpu = get_current_cpu();
    // A report that panicked left this set so its own unwind logging could not
    // re-enter the reporter; that window is over.
    IN_REPORT[cpu].store(false, Ordering::Relaxed);
    let stack = held(cpu);

    // SAFETY: per-CPU slot; `_irq` excludes the interrupt handler that is the
    // only other party able to write it, and no reference is minted.
    unsafe {
        // The first entry removed carries the chain key as it stood before it
        // was pushed, which is exactly the key for the entries being kept.
        let depth = (*stack).depth.load(Ordering::Relaxed);
        let restored = if depth > mark {
            (*stack).entries[mark as usize].prev_chain_key
        } else {
            (*stack).curr_chain_key
        };

        let mut depth = depth;
        while depth > mark {
            depth -= 1;
            (*stack).depth.store(depth, Ordering::Relaxed);
            core::sync::atomic::compiler_fence(Ordering::Release);
            let entry = (*stack).entries[depth as usize];
            (*stack).entries[depth as usize] = HeldLock::EMPTY;
            (*stack).poison_drained.fetch_add(1, Ordering::Relaxed);
            if !entry.lock_addr.is_null() {
                // SAFETY: caller certifies lock addresses still point to
                // live SpinLocks (statics live for kernel lifetime).
                (entry.poison_fn)(entry.lock_addr);
            }
        }
        (*stack).curr_chain_key = restored;
    }
}

/// Registrable class slots consumed, out of [`REGISTRABLE_CLASSES`].
///
/// Slots, **not** classes: [`register_class`] reserves one before it knows
/// whether it will win the link race, and a loser's slot stays consumed and
/// unlinked. Use [`registered_class_count`] for the number of distinct
/// declaration sites; use this for pool headroom, which the leaked slots do
/// consume.
///
/// **Clamped**: [`register_class`] bumps `CLASS_COUNT` before its bound check,
/// so the raw counter overshoots the moment the table fills and keeps climbing
/// while allocation is still being attempted.
#[inline]
pub fn class_count() -> usize {
    (CLASS_COUNT.load(Ordering::Relaxed) as usize).min(REGISTRABLE_CLASSES)
}

/// Distinct declaration sites registered: [`class_count`] less the slots lost
/// to the link race.
///
/// This is the quantity "one class per declaration site" describes, and the
/// only one of the two that is a property of the kernel rather than of a
/// boot's scheduling. Every CPU that first-acquires an instance of the same
/// class inside one window reserves a slot and all but one leak, so on a
/// 4-vCPU boot the per-CPU run queues alone move [`class_count`] across three
/// values on one image — which a ratchet holding that number to an exact cap
/// reads as a lock appearing and disappearing.
#[inline]
pub fn registered_class_count() -> usize {
    class_count().saturating_sub(class_slots_leaked() as usize)
}

/// Dependency edges learned. Same clamp rationale as [`class_count`]
/// ([`add_edge`] bumps `EDGE_COUNT` before its bound check).
#[inline]
pub fn edge_count() -> usize {
    (EDGE_COUNT.load(Ordering::Relaxed) as usize).min(MAX_EDGES)
}

/// Validated chain prefixes cached. Same clamp rationale as [`class_count`].
#[inline]
pub fn chain_count() -> usize {
    (CHAIN_COUNT.load(Ordering::Relaxed) as usize).min(MAX_CHAINS)
}

/// `true` once any pool has filled. No ordering check runs after this.
#[inline]
pub fn graph_overflowed() -> bool {
    GRAPH_OVERFLOW.load(Ordering::Relaxed)
}

/// Why a [`declare_order`] call did not take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclareOrderError {
    /// The graph already reaches `outer` from `inner`. The edge is **not**
    /// inserted: doing so would make the graph cyclic and drown every later
    /// finding in derived noise.
    Contradicted,
    /// The class table, the edge pool, or the declaration table is full.
    Full,
}

/// Assert that `outer` is always acquired before `inner`.
///
/// Without a declaration, the polarity of a class pair is whatever ran first
/// this boot, so a direction nothing happened to execute is never reported.
/// Declaring the intended order at init makes the first wrong-way acquisition
/// a finding on **every** boot.
///
/// Both classes are registered eagerly at subclass 0, so the declaration is
/// live before either lock is first taken.
///
/// Returns `Ok(())` when tracking is disabled or the graph has overflowed —
/// there is nothing to declare against.
pub fn declare_order(
    outer: &'static LockClassKey,
    inner: &'static LockClassKey,
) -> Result<(), DeclareOrderError> {
    if !TRACKING_ENABLED.load(Ordering::Relaxed)
        || GRAPH_OVERFLOW.load(Ordering::Relaxed)
        || matches!(lockdep_mode(), LockdepMode::Off)
    {
        return Ok(());
    }

    let (Some(outer_idx), Some(inner_idx)) = (register_class(outer, 0), register_class(inner, 0))
    else {
        return Err(DeclareOrderError::Full);
    };
    if outer_idx == inner_idx {
        return Err(DeclareOrderError::Contradicted);
    }

    // Idempotent: a re-initialised subsystem declares the same pair again, and
    // burning a slot per call would fill the table.
    let already = DECLARED_COUNT.load(Ordering::Relaxed) as usize;
    for entry in &DECLARED[..already.min(MAX_DECLARED_ORDERS)] {
        if entry.outer.load(Ordering::Relaxed) == outer_idx
            && entry.inner.load(Ordering::Acquire) == inner_idx
        {
            return Ok(());
        }
    }

    if path_exists(inner_idx, outer_idx) {
        crate::klog_warn!(
            "LOCKDEP: declared order contradicted\n  declared   {} ({}) -> {} ({})\n  but the graph already reaches the declared outer class from the inner one",
            outer.name(),
            outer.site(),
            inner.name(),
            inner.site(),
        );
        print_path(inner_idx, outer_idx);
        return Err(DeclareOrderError::Contradicted);
    }

    let slot = DECLARED_COUNT.fetch_add(1, Ordering::Relaxed) as usize;
    if slot >= MAX_DECLARED_ORDERS {
        return Err(DeclareOrderError::Full);
    }
    DECLARED[slot].outer.store(outer_idx, Ordering::Relaxed);
    DECLARED[slot].inner.store(inner_idx, Ordering::Release);

    if add_edge(outer_idx, inner_idx).is_err() {
        return Err(DeclareOrderError::Full);
    }
    Ok(())
}

/// Called from the learn path only, so it costs nothing once the chain cache
/// is warm.
#[inline]
fn mark_declared_observed(outer_idx: u16, inner_idx: u16) {
    let n = (DECLARED_COUNT.load(Ordering::Relaxed) as usize).min(MAX_DECLARED_ORDERS);
    for entry in &DECLARED[..n] {
        if entry.inner.load(Ordering::Acquire) == inner_idx
            && entry.outer.load(Ordering::Relaxed) == outer_idx
        {
            entry.observed.store(true, Ordering::Relaxed);
            return;
        }
    }
}

/// Orderings declared via [`declare_order`].
#[inline]
pub fn declared_count() -> usize {
    (DECLARED_COUNT.load(Ordering::Relaxed) as usize).min(MAX_DECLARED_ORDERS)
}

/// Declared orderings some acquisition has actually exercised.
///
/// A gate reads this against [`declared_count`]: a declaration nothing ever
/// takes is describing code that no longer runs.
#[inline]
pub fn declared_observed() -> usize {
    DECLARED[..declared_count()]
        .iter()
        .filter(|e| e.observed.load(Ordering::Relaxed))
        .count()
}

/// `true` once the overflow warning has been emitted, so a kernel test can
/// assert a validator that disabled itself said so without scraping serial.
#[inline]
pub fn overflow_reported() -> bool {
    OVERFLOW_REPORTED.load(Ordering::Relaxed)
}

/// `true` once panic-mode bypass has been entered. Ordering checks are
/// suppressed; the held-stack walk stays active.
#[inline]
pub fn fatal_bypassed() -> bool {
    FATAL_BYPASS.load(Ordering::Relaxed)
}

/// `true` once [`enable_lock_tracking`] has run.
#[inline]
pub fn tracking_enabled() -> bool {
    TRACKING_ENABLED.load(Ordering::Relaxed)
}

/// Ordering violations reported since boot with the panic *not* suppressed
/// by report-only mode. A non-zero value in a running kernel is a real
/// finding nobody saw.
#[inline]
pub fn violations_reported() -> u32 {
    VIOLATION_COUNT.load(Ordering::Relaxed)
}

/// Violations reported while the reporter was in report-only mode.
#[inline]
pub fn report_only_violations() -> u32 {
    REPORT_ONLY_VIOLATIONS.load(Ordering::Relaxed)
}

/// Distinct findings printed (deduped per class pair).
#[inline]
pub fn violation_reports() -> u32 {
    VIOLATION_REPORTS.load(Ordering::Relaxed)
}

/// Distinct class-id collisions between different declaration sites.
#[inline]
pub fn class_collisions() -> u32 {
    CLASS_COLLISIONS.load(Ordering::Relaxed)
}

/// Class slots abandoned to the registration race. Diagnostic only.
#[inline]
pub fn class_slots_leaked() -> u32 {
    CLASS_SLOTS_LEAKED.load(Ordering::Relaxed)
}

/// Chain-cache hits summed across CPUs. Torn-snapshot caveat as
/// [`held_lock_addrs_for_cpu`].
pub fn chain_hits() -> u64 {
    let mut n = 0u64;
    for cell in HELD.iter() {
        // SAFETY: read-only racy snapshot of per-CPU counters.
        n = n.wrapping_add(unsafe { (*cell.0.0.get()).chain_hits.load(Ordering::Relaxed) });
    }
    n
}

/// Chain-cache misses summed across CPUs.
pub fn chain_misses() -> u64 {
    let mut n = 0u64;
    for cell in HELD.iter() {
        // SAFETY: as `chain_hits`.
        n = n.wrapping_add(unsafe { (*cell.0.0.get()).chain_misses.load(Ordering::Relaxed) });
    }
    n
}

/// How many locks this CPU holds, and which one is innermost.
///
/// One observation, not two: a caller that asks the count and the name
/// separately runs preemptible between them and can name a lock it never
/// held. Every consumer wants both.
pub fn held_lock_snapshot() -> (u32, Option<(&'static str, &'static str, u64)>) {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) {
        return (0, None);
    }
    let _irq = IrqOff::enter();
    let stack = held(get_current_cpu());
    // SAFETY: per-CPU slot; `_irq` excludes the interrupt handler that is the
    // only other party able to write it, and no reference is minted.
    unsafe {
        let depth = (*stack).depth.load(Ordering::Relaxed);
        // Skip a null-address slot rather than describe one: it can only be a
        // dropped push, and a formatted null address reads as corruption.
        for i in (0..depth as usize).rev() {
            let e = (*stack).entries[i];
            if e.lock_addr.is_null() {
                continue;
            }
            let named = if e.class_idx == NONE_IDX {
                ("<untracked>", "<none>", e.lock_addr as u64)
            } else {
                let cls = &CLASSES.0[e.class_idx as usize];
                (cls.name(), cls.site(), e.lock_addr as u64)
            };
            return (depth, Some(named));
        }
        (depth, None)
    }
}

/// Name and instance of the innermost lock held on the calling CPU, for a
/// diagnostic that would otherwise report only a count.
pub fn innermost_held_lock() -> Option<(&'static str, &'static str, u64)> {
    held_lock_snapshot().1
}

/// Releases whose address was not on the stack, summed across CPUs.
///
/// Never zero once a panic has been recovered: the poison walk drains the
/// stack and every live guard's `Drop` then pops an address that is gone. It
/// is a leak only when it exceeds [`poison_drained`].
pub fn pop_misses() -> u64 {
    let mut n = 0u64;
    for cell in HELD.iter() {
        // SAFETY: as `chain_hits`.
        n = n.wrapping_add(unsafe { (*cell.0.0.get()).pop_misses.load(Ordering::Relaxed) });
    }
    n
}

/// Entries removed by the poison walk, summed across CPUs.
pub fn poison_drained() -> u64 {
    let mut n = 0u64;
    for cell in HELD.iter() {
        // SAFETY: as `chain_hits`.
        n = n.wrapping_add(unsafe { (*cell.0.0.get()).poison_drained.load(Ordering::Relaxed) });
    }
    n
}

/// Deepest held-lock nesting observed on any CPU.
pub fn held_depth_max() -> u32 {
    let mut n = 0u32;
    for cell in HELD.iter() {
        // SAFETY: as `chain_hits`.
        let d = unsafe { (*cell.0.0.get()).depth_max.load(Ordering::Relaxed) };
        if d > n {
            n = d;
        }
    }
    n
}

/// Pushes dropped for exceeding [`MAX_HELD_LOCKS`]. Must be zero: a dropped
/// entry is invisible to the poison walk and to `pop_lock`.
pub fn held_depth_overflows() -> u64 {
    let mut n = 0u64;
    for cell in HELD.iter() {
        // SAFETY: as `chain_hits`.
        n = n.wrapping_add(unsafe { (*cell.0.0.get()).depth_overflows.load(Ordering::Relaxed) });
    }
    n
}

/// Composite health: the validator is actually performing ordering checks.
#[inline]
pub fn validator_alive() -> bool {
    tracking_enabled()
        && !graph_overflowed()
        && !fatal_bypassed()
        && lockdep_mode_raw() != LockdepMode::Off as u8
}

/// Snapshot of one registered class, for diagnostics.
#[derive(Clone, Copy)]
pub struct ClassInfo {
    pub id: u64,
    pub name: &'static str,
    pub site: &'static str,
    /// First instance address registered against this class; 0 if none.
    pub first_addr: u64,
    pub level: u8,
    pub subclass: u8,
    pub flags: u32,
}

/// Read class `idx` if a class has been registered there. Bounded on
/// [`MAX_CLASSES`] rather than [`class_count`] so the reserved self-test slots
/// above the registrable range are dumpable too.
pub fn class_info(idx: usize) -> Option<ClassInfo> {
    if idx >= MAX_CLASSES {
        return None;
    }
    let c = &CLASSES.0[idx];
    let id = c.id.load(Ordering::Acquire);
    if id == 0 {
        return None;
    }
    Some(ClassInfo {
        id,
        name: c.name(),
        site: c.site(),
        first_addr: c.first_addr.load(Ordering::Relaxed),
        level: c.level.load(Ordering::Relaxed),
        subclass: c.subclass.load(Ordering::Relaxed),
        flags: c.flags.load(Ordering::Relaxed),
    })
}

/// Latch [`GRAPH_OVERFLOW`] and, once per boot, say so.
///
/// Reached from inside a held-stack update, so the [`ReportLatch`] is what
/// makes the logging safe: `klog_warn!` takes `KLOG_RING` — a `SpinLock`,
/// whose `try_lock` calls back into [`push_lock`]. That `try_lock` cannot
/// deadlock against a `KLOG_RING` this CPU already holds: it fails the ticket
/// CAS and drops the line.
///
/// Kept `#[cold] #[inline(never)]` because [`push_lock`] is `#[inline]`: an
/// inline `klog_warn!` would push a `format_args!` frame into every tracked
/// lock-acquire site.
#[cold]
#[inline(never)]
fn latch_overflow(reason: &str, addr: *const (), level: u8) {
    GRAPH_OVERFLOW.store(true, Ordering::Relaxed);
    if OVERFLOW_REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let _latch = ReportLatch::raise(get_current_cpu());
    crate::klog_warn!(
        "LOCKDEP: validator DISABLED — {} while handling lock @ {:#x} level {} \
         (classes {}/{} edges {}/{} chains {}/{}); every subsequent lock \
         acquisition is UNVALIDATED",
        reason,
        addr as usize,
        level,
        class_count(),
        REGISTRABLE_CLASSES,
        edge_count(),
        MAX_EDGES,
        chain_count(),
        MAX_CHAINS,
    );
}

/// What the held-stack updates this boot were observed to run under.
///
/// Three states, not a bool: a two-state "saw interrupts enabled" flag cannot
/// tell "every update ran masked" from "no update ever ran", so an assertion
/// built on one passes on a validator that never started.
#[cfg(any(test, feature = "test-helpers"))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum PushIrqState {
    /// No held-stack update has run.
    NotReached = 0,
    /// Every update so far ran with interrupts masked.
    ReachedMasked = 1,
    /// At least one update ran with interrupts enabled.
    ReachedUnmasked = 2,
}

#[cfg(any(test, feature = "test-helpers"))]
static PUSH_IRQ_STATE: AtomicU8 = AtomicU8::new(PushIrqState::NotReached as u8);

/// What interrupt state the held-stack updates ran under since boot.
#[cfg(any(test, feature = "test-helpers"))]
pub fn push_irq_state() -> PushIrqState {
    match PUSH_IRQ_STATE.load(Ordering::Relaxed) {
        0 => PushIrqState::NotReached,
        1 => PushIrqState::ReachedMasked,
        _ => PushIrqState::ReachedUnmasked,
    }
}

#[cfg(any(test, feature = "test-helpers"))]
#[inline]
fn record_push_irq_state() {
    let observed = if crate::cpu::x86_64::interrupts::are_interrupts_enabled() {
        PushIrqState::ReachedUnmasked as u8
    } else {
        PushIrqState::ReachedMasked as u8
    };
    // Monotonic: once one update has been seen unmasked, nothing walks it back.
    let _ = PUSH_IRQ_STATE.fetch_max(observed, Ordering::Relaxed);
}

/// Production builds const-fold this away: the probe costs a `pushfq` the
/// acquire path does not otherwise need.
#[cfg(not(any(test, feature = "test-helpers")))]
#[inline(always)]
fn record_push_irq_state() {}

#[inline]
fn bump_chain_hit(cpu: usize) {
    // SAFETY: per-CPU slot; a relaxed RMW needs no mask.
    unsafe { (*held(cpu)).chain_hits.fetch_add(1, Ordering::Relaxed) };
}

#[inline]
fn bump_chain_miss(cpu: usize) {
    // SAFETY: as `bump_chain_hit`.
    unsafe { (*held(cpu)).chain_misses.fetch_add(1, Ordering::Relaxed) };
}

/// Record the first instance address seen for a class, so a report can name
/// a concrete lock. Slow-path only, and a no-op after the first success.
#[inline]
fn record_first_addr(class_idx: u16, lock_addr: *const ()) {
    let cls = &CLASSES.0[class_idx as usize];
    if cls.first_addr.load(Ordering::Relaxed) == 0 {
        let _ = cls.first_addr.compare_exchange(
            0,
            lock_addr as u64,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }
}

/// Fold the **high** bits: the low bits of a Fibonacci multiply are the weak
/// end, and `id` is already avalanched.
#[inline]
fn class_bucket(id: u64) -> usize {
    ((id.wrapping_mul(0x9E3779B97F4A7C15) >> 40) as usize) & (CLASS_HASH_BUCKETS - 1)
}

/// Lock-free class registration, keyed on the declaration site.
///
/// Returns `None` only when the registrable range is exhausted.
fn register_class(key: &'static LockClassKey, subclass: u8) -> Option<u16> {
    let id = subclass_id(key.id(), subclass);
    let bucket = class_bucket(id);

    let mut idx = CLASS_HASH.0[bucket].load(Ordering::Acquire);
    while idx != NONE_IDX {
        let cls = &CLASSES.0[idx as usize];
        if cls.id.load(Ordering::Acquire) == id {
            // The string compare runs only when the pointers differ — either
            // duplicated rodata or a genuine 64-bit collision.
            if cls.key.load(Ordering::Relaxed) != key as *const LockClassKey as *mut LockClassKey {
                check_class_collision(idx, key);
            }
            return Some(idx);
        }
        idx = cls.next_in_bucket.load(Ordering::Acquire);
    }

    let new_idx = CLASS_COUNT.fetch_add(1, Ordering::Relaxed);
    if (new_idx as usize) >= REGISTRABLE_CLASSES {
        return None;
    }
    let cls = &CLASSES.0[new_idx as usize];
    cls.level.store(key.level(), Ordering::Relaxed);
    cls.subclass.store(subclass, Ordering::Relaxed);
    cls.flags.store(key.flags(), Ordering::Relaxed);
    cls.key.store(
        key as *const LockClassKey as *mut LockClassKey,
        Ordering::Relaxed,
    );
    // Release: publishes every field above to the bucket walker's Acquire.
    cls.id.store(id, Ordering::Release);

    // Re-scan from the observed head each round: linking unconditionally would
    // split one declaration site across two classes whenever two CPUs
    // first-acquire two different instances of it concurrently.
    loop {
        let head = CLASS_HASH.0[bucket].load(Ordering::Acquire);
        let mut probe = head;
        while probe != NONE_IDX {
            let other = &CLASSES.0[probe as usize];
            if other.id.load(Ordering::Acquire) == id {
                // Lost the race; our slot leaks, bounded by the CPUs
                // first-acquiring this class in one window.
                CLASS_SLOTS_LEAKED.fetch_add(1, Ordering::Relaxed);
                return Some(probe);
            }
            probe = other.next_in_bucket.load(Ordering::Acquire);
        }
        cls.next_in_bucket.store(head, Ordering::Relaxed);
        if CLASS_HASH.0[bucket]
            .compare_exchange_weak(head, new_idx, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            return Some(new_idx);
        }
    }
}

/// Two declaration sites whose ids collide are validated as one class. That
/// can only add false positives, never false negatives, so the class is merged
/// and the collision reported rather than rehashed — rehashing would need an
/// unbounded probe on the hot lookup path.
#[cold]
#[inline(never)]
fn check_class_collision(idx: u16, incoming: &'static LockClassKey) {
    let cls = &CLASSES.0[idx as usize];
    let Some(existing) = cls.key_ref() else {
        return;
    };
    // Two addresses for one key is release-build rodata duplication, not a
    // collision; content is what survives duplication.
    if existing.site() == incoming.site() && existing.name() == incoming.name() {
        return;
    }
    if cls.flags.fetch_or(LC_COLLISION_REPORTED, Ordering::Relaxed) & LC_COLLISION_REPORTED != 0 {
        return;
    }
    CLASS_COLLISIONS.fetch_add(1, Ordering::Relaxed);
    // As `latch_overflow`: reached mid-update, so the klog acquire below must
    // not reach back into the held stack.
    let _latch = ReportLatch::raise(get_current_cpu());
    crate::klog_warn!(
        "LOCKDEP: class-id collision — {} ({}) and {} ({}) both hash to {:#x}; \
         they will be validated as ONE class. Rename one site to separate them.",
        existing.name(),
        existing.site(),
        incoming.name(),
        incoming.site(),
        cls.id.load(Ordering::Relaxed),
    );
}

/// Add an edge `from -> to` to the dependency graph if not already present.
fn add_edge(from: u16, to: u16) -> Result<(), ()> {
    let cls_from = &CLASSES.0[from as usize];
    let mut idx = cls_from.edges_after_head.load(Ordering::Acquire);
    while idx != NONE_IDX {
        let e = &EDGES.0[idx as usize];
        if e.target.load(Ordering::Acquire) == to {
            return Ok(());
        }
        idx = e.next.load(Ordering::Acquire);
    }

    let new_idx = EDGE_COUNT.fetch_add(1, Ordering::Relaxed);
    if (new_idx as usize) >= MAX_EDGES {
        return Err(());
    }
    let e = &EDGES.0[new_idx as usize];
    e.target.store(to, Ordering::Relaxed);

    loop {
        let head = cls_from.edges_after_head.load(Ordering::Relaxed);
        e.next.store(head, Ordering::Relaxed);
        if cls_from
            .edges_after_head
            .compare_exchange_weak(head, new_idx as u16, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(());
        }
    }
}

/// BFS from `src`: is `target` reachable via `edges_after` edges?
///
/// Out of line and `#[cold]` so its ~600 bytes of scratch are not hoisted into
/// `LockCore::acquire`'s frame, which `check_stack_sizes.sh` holds to 2 KiB.
#[cold]
#[inline(never)]
fn path_exists(src: u16, target: u16) -> bool {
    if src == target {
        return true;
    }
    let mut visited = [0u64; (MAX_CLASSES + 63) / 64];
    let mut queue = [0u16; MAX_BFS_FRONTIER];
    let mut head: usize = 0;
    let mut tail: usize = 0;

    fn mark(visited: &mut [u64], idx: u16) {
        let w = (idx as usize) / 64;
        let b = (idx as usize) % 64;
        visited[w] |= 1u64 << b;
    }
    fn is_marked(visited: &[u64], idx: u16) -> bool {
        let w = (idx as usize) / 64;
        let b = (idx as usize) % 64;
        (visited[w] >> b) & 1 == 1
    }

    mark(&mut visited, src);
    queue[tail] = src;
    tail += 1;

    while head < tail {
        let cur = queue[head];
        head += 1;

        let mut edge_idx = CLASSES.0[cur as usize]
            .edges_after_head
            .load(Ordering::Acquire);
        while edge_idx != NONE_IDX {
            let e = &EDGES.0[edge_idx as usize];
            let nxt = e.target.load(Ordering::Acquire);
            if nxt == target {
                return true;
            }
            if !is_marked(&visited, nxt) && (nxt as usize) < MAX_CLASSES {
                mark(&mut visited, nxt);
                // Unreachable: `visited` admits each class at most once and
                // the frontier holds MAX_CLASSES. The check is what makes that
                // obvious rather than reasoned.
                if tail < MAX_BFS_FRONTIER {
                    queue[tail] = nxt;
                    tail += 1;
                }
            }
            edge_idx = e.next.load(Ordering::Acquire);
        }
    }
    false
}

/// Chain-hash lookup: has this chain prefix already been validated?
fn chain_lookup(chain_key: u64) -> bool {
    let bucket = chain_bucket(chain_key);
    let mut idx = CHAIN_HASH.0[bucket].load(Ordering::Acquire);
    while idx != NONE_IDX {
        let c = &CHAINS.0[idx as usize];
        if c.chain_key.load(Ordering::Acquire) == chain_key {
            return true;
        }
        idx = c.next_in_bucket.load(Ordering::Acquire);
    }
    false
}

/// Insert a freshly-validated chain into the chain-hash cache.
fn chain_insert(chain_key: u64) -> Result<(), ()> {
    let new_idx = CHAIN_COUNT.fetch_add(1, Ordering::Relaxed);
    if (new_idx as usize) >= MAX_CHAINS {
        return Err(());
    }
    let c = &CHAINS.0[new_idx as usize];
    c.chain_key.store(chain_key, Ordering::Relaxed);
    let bucket = chain_bucket(chain_key);
    loop {
        let head = CHAIN_HASH.0[bucket].load(Ordering::Relaxed);
        c.next_in_bucket.store(head, Ordering::Relaxed);
        if CHAIN_HASH.0[bucket]
            .compare_exchange_weak(head, new_idx as u16, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(());
        }
    }
}

#[inline]
fn chain_bucket(chain_key: u64) -> usize {
    ((chain_key.wrapping_mul(0x9E3779B97F4A7C15)) as usize) & (CHAIN_HASH_BUCKETS - 1)
}

/// Iteratively mix a class index into the running chain key.
#[inline]
fn iterate_chain_key(key: u64, idx: u16) -> u64 {
    let mut k = key;
    k = k.wrapping_mul(0x9E3779B97F4A7C15);
    k ^= idx as u64;
    k = k.rotate_left(17);
    k = k.wrapping_add(0x517CC1B727220A95);
    k
}

/// No-op poison function used as the sentinel for empty held-stack entries.
unsafe fn noop_poison(_addr: *const ()) {}

/// What a report should do about a finding.
enum Action {
    /// A fatal abort is in flight; say nothing.
    Silent,
    /// Report and continue.
    Warn,
    /// Report and panic.
    Panic,
}

#[inline]
fn violation_action(class_idx: u16) -> Action {
    if FATAL_BYPASS.load(Ordering::Relaxed) {
        return Action::Silent;
    }
    if CLASSES.0[class_idx as usize].flags.load(Ordering::Relaxed) & LO_BLESSED != 0 {
        return Action::Silent;
    }
    if self_test_active() {
        return Action::Warn;
    }
    match lockdep_mode_raw() {
        x if x == LockdepMode::Off as u8 => Action::Silent,
        x if x == LockdepMode::Warn as u8 => Action::Warn,
        _ => Action::Panic,
    }
}

const VK_CYCLE: u8 = 1;
const VK_NESTING: u8 = 2;
const VK_RECURSION: u8 = 3;
const VK_EPOCH: u8 = 4;

const VK_OCCUPIED: u64 = 1 << 63;

/// `true` the first time this `(kind, held class, new class)` triple is seen.
///
/// Deduping on the *class* pair rather than the address pair is the payoff of
/// declaration-site keying: 256 `PROCESS_VMS` instances inverting against one
/// registry lock is one finding. Without it, an inversion on a hot path floods
/// the serial line and truncates the boot before the enumeration finishes.
fn violation_is_new(kind: u8, held_class: u16, new_class: u16) -> bool {
    if VIOLATION_DEDUP_FULL.load(Ordering::Relaxed) {
        return false;
    }
    let key =
        VK_OCCUPIED | ((kind as u64) << 32) | ((held_class as u64) << 16) | (new_class as u64);
    let mut slot = (avalanche(key) as usize) & (MAX_VIOLATION_REPORTS - 1);
    for _ in 0..MAX_VIOLATION_REPORTS {
        let cur = VIOLATION_KEYS.0[slot].load(Ordering::Acquire);
        if cur == key {
            return false;
        }
        if cur == 0
            && VIOLATION_KEYS.0[slot]
                .compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            VIOLATION_REPORTS.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        slot = (slot + 1) & (MAX_VIOLATION_REPORTS - 1);
    }
    if !VIOLATION_DEDUP_FULL.swap(true, Ordering::Relaxed) {
        crate::klog_warn!(
            "LOCKDEP: violation dedup table full ({} distinct findings); \
             further reports are counted but not printed",
            MAX_VIOLATION_REPORTS,
        );
    }
    false
}

/// Count a finding and decide whether to print it. Also raises the per-CPU
/// [`IN_REPORT`] latch, so the `KLOG_RING` acquire the printing itself
/// performs cannot recurse back into the reporter.
fn begin_report(kind: u8, class_idx: u16, cpu: usize, upto: usize) -> (Action, bool, usize) {
    let action = violation_action(class_idx);
    if let Action::Silent = action {
        return (action, false, 0);
    }
    if self_test_active() || matches!(action, Action::Warn) {
        REPORT_ONLY_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
    } else {
        VIOLATION_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    // Raised before `violation_is_new`, which warns when the dedup table
    // fills; that klog line would otherwise land outside the latch.
    IN_REPORT[cpu].store(true, Ordering::Relaxed);
    let held_class = if upto == 0 {
        NONE_IDX
    } else {
        held_entry_at(cpu, upto - 1).class_idx
    };
    let fresh = violation_is_new(kind, held_class, class_idx);
    (action, fresh, cpu)
}

/// Clear the per-CPU report latch.
///
/// Only for a report that *returns*. A panicking report deliberately leaves it
/// set: the unwind logs, that logging takes `KLOG_RING`, and its acquire
/// re-enters `push_lock` straight back into the already-panicking reporter.
/// [`poison_unlock_held_above`] clears it once the panic has been caught.
#[inline]
fn end_report(cpu: usize) {
    IN_REPORT[cpu].store(false, Ordering::Relaxed);
}

/// Print the held stack, outermost first.
fn print_held(cpu: usize, upto: usize) {
    for i in 0..upto {
        let h = held_entry_at(cpu, i);
        if h.class_idx == NONE_IDX {
            continue;
        }
        let cls = &CLASSES.0[h.class_idx as usize];
        crate::klog_warn!(
            "    #{}  {} ({}) level {}  inst {:#x}",
            i,
            cls.name(),
            cls.site(),
            cls.level.load(Ordering::Relaxed),
            h.lock_addr as usize,
        );
    }
}

/// Print the learned route from `src` to `target`, if one reporter can get the
/// path lock. Endpoints alone say a path exists without saying through what.
fn print_path(src: u16, target: u16) {
    if REPORT_PATH_LOCK.swap(true, Ordering::Acquire) {
        return;
    }
    for c in CLASSES.0.iter() {
        c.bfs_parent.store(NONE_IDX, Ordering::Relaxed);
    }
    let mut queue = [0u16; MAX_BFS_FRONTIER];
    let mut visited = [0u64; MAX_CLASSES.div_ceil(64)];
    let (mut head, mut tail) = (0usize, 0usize);
    visited[(src as usize) / 64] |= 1u64 << ((src as usize) % 64);
    queue[tail] = src;
    tail += 1;
    let mut found = false;
    'bfs: while head < tail {
        let cur = queue[head];
        head += 1;
        let mut e_idx = CLASSES.0[cur as usize]
            .edges_after_head
            .load(Ordering::Acquire);
        while e_idx != NONE_IDX {
            let e = &EDGES.0[e_idx as usize];
            let nxt = e.target.load(Ordering::Acquire);
            if (nxt as usize) < MAX_CLASSES
                && (visited[(nxt as usize) / 64] >> ((nxt as usize) % 64)) & 1 == 0
            {
                visited[(nxt as usize) / 64] |= 1u64 << ((nxt as usize) % 64);
                CLASSES.0[nxt as usize]
                    .bfs_parent
                    .store(cur, Ordering::Relaxed);
                if nxt == target {
                    found = true;
                    break 'bfs;
                }
                if tail < MAX_BFS_FRONTIER {
                    queue[tail] = nxt;
                    tail += 1;
                }
            }
            e_idx = e.next.load(Ordering::Acquire);
        }
    }
    if found {
        let mut route = [NONE_IDX; MAX_HELD_LOCKS];
        let mut n = 0usize;
        let mut cur = target;
        while cur != NONE_IDX && n < route.len() {
            route[n] = cur;
            n += 1;
            if cur == src {
                break;
            }
            cur = CLASSES.0[cur as usize].bfs_parent.load(Ordering::Relaxed);
        }
        crate::klog_warn!("  existing path ({} hops):", n.saturating_sub(1));
        for i in (0..n).rev() {
            let cls = &CLASSES.0[route[i] as usize];
            crate::klog_warn!("    -> {} ({})", cls.name(), cls.site());
        }
    }
    REPORT_PATH_LOCK.store(false, Ordering::Release);
}

/// Header line shared by every finding: what was being acquired.
fn print_acquiring(kind: &str, class_idx: u16, addr: *const ()) {
    let cls = &CLASSES.0[class_idx as usize];
    crate::klog_warn!(
        "LOCKDEP: {}\n  acquiring  {} ({}) level {}  inst {:#x}",
        kind,
        cls.name(),
        cls.site(),
        cls.level.load(Ordering::Relaxed),
        addr as usize,
    );
}

#[cold]
#[inline(never)]
fn report_cycle(new_class: u16, new_addr: *const (), cpu: usize, upto: usize) -> bool {
    let (action, fresh, cpu) = begin_report(VK_CYCLE, new_class, cpu, upto);
    if let Action::Silent = action {
        return false;
    }
    if fresh {
        print_acquiring("dependency cycle", new_class, new_addr);
        crate::klog_warn!("  while held (outermost first):");
        print_held(cpu, upto);
        if upto > 0 {
            print_path(new_class, held_entry_at(cpu, upto - 1).class_idx);
        }
    }
    if let Action::Panic = action {
        let cls = &CLASSES.0[new_class as usize];
        panic!(
            "LOCK DEPENDENCY CYCLE: acquiring {} ({}) would close a cycle through a held class",
            cls.name(),
            cls.site(),
        );
    }
    end_report(cpu);
    true
}

#[cold]
#[inline(never)]
fn report_epoch_violation(new_class: u16, new_addr: *const (), cpu: usize, upto: usize) -> bool {
    let (action, fresh, cpu) = begin_report(VK_EPOCH, new_class, cpu, upto);
    if let Action::Silent = action {
        return false;
    }
    if fresh {
        print_acquiring("lock acquired inside an Epoch scope", new_class, new_addr);
        crate::klog_warn!("  while held (outermost first):");
        print_held(cpu, upto);
    }
    if let Action::Panic = action {
        let cls = &CLASSES.0[new_class as usize];
        panic!(
            "LOCK INSIDE EPOCH: acquiring {} ({}) while an Epoch is held — holding a lock \
             across a wake site inside an epoch breaks the atomic-publish invariant",
            cls.name(),
            cls.site(),
        );
    }
    end_report(cpu);
    true
}

#[cold]
#[inline(never)]
fn report_recursion(new_class: u16, new_addr: *const (), cpu: usize, upto: usize) -> bool {
    let (action, fresh, cpu) = begin_report(VK_RECURSION, new_class, cpu, upto);
    if let Action::Silent = action {
        return false;
    }
    if fresh {
        print_acquiring("recursive acquisition of one instance", new_class, new_addr);
        crate::klog_warn!("  while held (outermost first):");
        print_held(cpu, upto);
    }
    if let Action::Panic = action {
        let cls = &CLASSES.0[new_class as usize];
        panic!(
            "LOCK RECURSION: re-acquiring the same instance of {} ({}) @ {:#x}",
            cls.name(),
            cls.site(),
            new_addr as usize,
        );
    }
    end_report(cpu);
    true
}

#[cold]
#[inline(never)]
fn report_same_class_nesting(
    new_class: u16,
    new_addr: *const (),
    held_addr: *const (),
    cpu: usize,
    upto: usize,
) -> bool {
    let (action, fresh, cpu) = begin_report(VK_NESTING, new_class, cpu, upto);
    if let Action::Silent = action {
        return false;
    }
    if fresh {
        print_acquiring("same-class nesting", new_class, new_addr);
        crate::klog_warn!(
            "  another instance of the same declaration is already held @ {:#x}",
            held_addr as usize,
        );
        crate::klog_warn!("  while held (outermost first):");
        print_held(cpu, upto);
    }
    if let Action::Panic = action {
        let cls = &CLASSES.0[new_class as usize];
        panic!(
            "LOCK SAME-CLASS NESTING: acquiring {} ({}) @ {:#x} while instance {:#x} of the \
             same declaration is held — annotate the site LO_DUPOK if it orders its instances",
            cls.name(),
            cls.site(),
            new_addr as usize,
            held_addr as usize,
        );
    }
    end_report(cpu);
    true
}

/// Install `addr` into reserved class slot `slot` and link it into the class
/// hash, so a later [`push_lock`] finds it on the fast path.
///
/// Idempotent: re-registering the same address returns the same index. Slots
/// live above [`REGISTRABLE_CLASSES`], so [`register_class`] can never hand one
/// out — which is what lets the in-kernel self-test run against a class table
/// that has otherwise overflowed.
///
/// Returns `None` if `slot` is out of range or already claimed by a different
/// address.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reserve_self_test_class(
    slot: usize,
    key: &'static LockClassKey,
    addr: *const (),
) -> Option<SelfTestClass> {
    if slot >= RESERVED_TEST_CLASSES {
        return None;
    }
    let idx = (REGISTRABLE_CLASSES + slot) as u16;
    let id = key.id();
    let cls = &CLASSES.0[idx as usize];
    let token = SelfTestClass { key, addr };
    let existing = cls.id.load(Ordering::Acquire);
    if existing == id {
        return Some(token);
    }
    if existing != 0 {
        return None;
    }
    cls.level.store(key.level(), Ordering::Relaxed);
    cls.flags.store(key.flags(), Ordering::Relaxed);
    cls.key.store(
        key as *const LockClassKey as *mut LockClassKey,
        Ordering::Relaxed,
    );
    cls.id.store(id, Ordering::Release);
    let bucket = class_bucket(id);
    loop {
        let head = CLASS_HASH.0[bucket].load(Ordering::Relaxed);
        cls.next_in_bucket.store(head, Ordering::Relaxed);
        if CLASS_HASH.0[bucket]
            .compare_exchange_weak(head, idx, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            return Some(token);
        }
    }
}

/// Register a class from a test-supplied key, so a test can force two
/// distinct sites to collide.
#[cfg(any(test, feature = "test-helpers"))]
pub fn register_class_for_test(key: &'static LockClassKey) -> Option<u16> {
    register_class(key, 0)
}

/// Hands the validator to the lockdep self-test for the guard's lifetime.
/// See [`SELF_TEST_ACTIVE`].
#[cfg(any(test, feature = "test-helpers"))]
pub struct SelfTestGuard(());

/// Poison callback for reserved self-test classes. Reached only by
/// `poison_unlock_all_held`, where there is no lock at a synthetic address to
/// unlock; OSTD supplies it so the "nothing ever dereferences a self-test
/// address" half of [`push_lock`]'s contract is not a caller's to get wrong.
#[cfg(any(test, feature = "test-helpers"))]
unsafe fn self_test_noop_poison(_addr: *const ()) {}

#[cfg(any(test, feature = "test-helpers"))]
impl SelfTestGuard {
    pub fn begin() -> Self {
        SELF_TEST_ACTIVE.store(true, Ordering::Release);
        Self(())
    }

    /// Push a reserved self-test class onto the held stack.
    ///
    /// Safe because the address came from [`reserve_self_test_class`], which
    /// only hands out slots above [`REGISTRABLE_CLASSES`], and because the
    /// poison callback is OSTD's own no-op — so it is never dereferenced.
    pub fn push(&self, class: SelfTestClass) {
        // SAFETY: synthetic reserved-class address, never dereferenced; the
        // caller pairs push/pop LIFO within this guard's scope.
        unsafe { push_lock(class.addr, self_test_noop_poison, class.key) };
    }

    /// Pop a class pushed through [`SelfTestGuard::push`].
    pub fn pop(&self, class: SelfTestClass) {
        // SAFETY: as `push`; unwinds the held stack entry it created.
        unsafe { pop_lock(class.addr) };
    }
}

/// A reserved lockdep class standing in for a real lock during the
/// self-test. Only [`reserve_self_test_class`] mints one.
#[cfg(any(test, feature = "test-helpers"))]
#[derive(Clone, Copy)]
pub struct SelfTestClass {
    key: &'static LockClassKey,
    addr: *const (),
}

#[cfg(any(test, feature = "test-helpers"))]
impl Drop for SelfTestGuard {
    fn drop(&mut self) {
        SELF_TEST_ACTIVE.store(false, Ordering::Release);
    }
}

/// Drive this CPU's [`IN_REPORT`] latch directly, so a test can exercise the
/// re-entrant acquire a reporter's own klog performs without provoking a
/// report to get there.
#[cfg(any(test, feature = "test-helpers"))]
pub fn set_in_report_for_test(active: bool) {
    IN_REPORT[get_current_cpu()].store(active, Ordering::Relaxed);
}

/// Reset all global state. Test-only; production never resets.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    use core::sync::atomic::Ordering::Relaxed;
    TRACKING_ENABLED.store(false, Relaxed);
    FATAL_BYPASS.store(false, Relaxed);
    GRAPH_OVERFLOW.store(false, Relaxed);
    OVERFLOW_REPORTED.store(false, Relaxed);
    SELF_TEST_ACTIVE.store(false, Relaxed);
    VIOLATION_COUNT.store(0, Relaxed);
    REPORT_ONLY_VIOLATIONS.store(0, Relaxed);
    VIOLATION_REPORTS.store(0, Relaxed);
    VIOLATION_DEDUP_FULL.store(false, Relaxed);
    CLASS_COLLISIONS.store(0, Relaxed);
    CLASS_SLOTS_LEAKED.store(0, Relaxed);
    PUSH_IRQ_STATE.store(PushIrqState::NotReached as u8, Relaxed);
    LOCKDEP_MODE.store(LockdepMode::Panic as u8, Relaxed);
    REPORT_PATH_LOCK.store(false, Relaxed);
    CLASS_COUNT.store(0, Relaxed);
    EDGE_COUNT.store(0, Relaxed);
    CHAIN_COUNT.store(0, Relaxed);
    for k in VIOLATION_KEYS.0.iter() {
        k.store(0, Relaxed);
    }
    for f in IN_REPORT.iter() {
        f.store(false, Relaxed);
    }
    for b in CLASS_HASH.0.iter() {
        b.store(NONE_IDX, Relaxed);
    }
    for b in CHAIN_HASH.0.iter() {
        b.store(NONE_IDX, Relaxed);
    }
    for c in CLASSES.0.iter() {
        c.id.store(0, Relaxed);
        c.key.store(core::ptr::null_mut(), Relaxed);
        c.first_addr.store(0, Relaxed);
        c.level.store(0, Relaxed);
        c.subclass.store(0, Relaxed);
        c.flags.store(0, Relaxed);
        c.edges_after_head.store(NONE_IDX, Relaxed);
        c.next_in_bucket.store(NONE_IDX, Relaxed);
        c.bfs_parent.store(NONE_IDX, Relaxed);
        c.usage_mask.store(0, Relaxed);
    }
    for e in EDGES.0.iter() {
        e.target.store(NONE_IDX, Relaxed);
        e.next.store(NONE_IDX, Relaxed);
    }
    for ch in CHAINS.0.iter() {
        ch.chain_key.store(0, Relaxed);
        ch.next_in_bucket.store(NONE_IDX, Relaxed);
    }
    for cell in HELD.iter() {
        // SAFETY: test-only; serialised by harness.
        unsafe {
            *cell.0.0.get() = HeldStack::new();
        }
    }
}
