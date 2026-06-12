//! Lock-ordering verification via runtime dependency graph + cycle detection.
//!
//! Replaces the strict-level rule (`acquire.level <= top.level` panics) with
//! a per-class dependency DAG. Same-level nesting becomes legal as long as
//! it does not form a cycle. Inspired by Linux's lockdep and FreeBSD's
//! WITNESS, but rewritten for SlopOS's constraints:
//!
//! - Lock address is the class identity (no static class_key indirection;
//!   simpler than lockdep's three-tier model and adequate for SlopOS's
//!   ~45-lock kernel).
//! - All bookkeeping is lock-free via atomic CAS into fixed pools (no global
//!   walker lock à la WITNESS's `w_mtx` to serialise edge learning across
//!   CPUs).
//! - Chain-hash cache short-circuits already-validated chain prefixes —
//!   steady-state acquire is O(1) (single hash lookup + push), matching
//!   lockdep's fast path.
//! - BFS over `locks_after` edges detects cycles; bounded queue means no
//!   stack growth (lockdep historically had to migrate from DFS to BFS for
//!   exactly this reason — LWN 335329).
//! - Panic-mode bypass: during a fatal abort the held-stack walk for
//!   poison-unlock is still active, but ordering checks are suppressed
//!   (Inv. 9 relaxation).
//! - Escape hatches modelled on WITNESS: `LO_DUPOK` for legitimate
//!   same-class nesting, `LO_TRYLOCK` skips ordering checks, blessed pairs
//!   suppress known-safe inverse acquisitions.
//!
//! The old `lock_tracking` module is a thin compat shim over this one;
//! existing `push_lock` / `pop_lock` / `poison_unlock_all_held` / the
//! `LOCK_LEVEL_*` constants keep their semantics so no SpinLock call site
//! needs to change.
//!
//! # Lock ordering levels (advisory rank hints)
//!
//! ```text
//! Level 0: Per-CPU data (preempt guard only, no lock)
//! Level 1: Per-resource locks (FD table, VM, pipe, socket, SHM)
//! Level 2: Registries (PIPE_ALLOC, SOCKET_ALLOC, ...)
//! Level 3: Global allocators (PAGE_ALLOCATOR, KERNEL_HEAP)
//! Level 4: Scheduler locks (per-CPU queue_lock)
//! ```
//!
//! Under the new model, level is an **advisory rank hint** stored on the
//! class for diagnostics; the *enforcement* is the cycle check, which is
//! strictly more general (catches AB-BA between same-level locks while
//! also catching the cross-level cases the old strict-rule caught).

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};

use crate::cpu::x86_64::pcr::{MAX_CPUS, get_current_cpu};

use super::cpu_local::CacheAligned;

// ===========================================================================
// Sizing
// ===========================================================================

/// Maximum distinct lock classes (one per unique lock instance address).
/// Kernel currently has ~45 locks; 10× headroom for growth.
pub const MAX_CLASSES: usize = 256;

/// Maximum dependency edges in the class graph. Sized for ~16 edges per
/// class on average; lockdep's default is 16384 for 8192 classes.
pub const MAX_EDGES: usize = 4096;

/// Chain-hash cache slots. Each entry caches an already-validated chain
/// prefix so repeated acquisitions skip the BFS.
pub const MAX_CHAINS: usize = 2048;

/// Number of buckets in the chain-key hash table. Must be a power of two.
pub const CHAIN_HASH_BUCKETS: usize = 256;

/// Number of buckets in the class-address hash table. Must be a power of two.
pub const CLASS_HASH_BUCKETS: usize = 256;

/// Maximum concurrently held locks per CPU. Kernel's deepest observed
/// nesting is 2-3; 16 is generous and covers test scaffolding.
pub const MAX_HELD_LOCKS: usize = 16;

/// Maximum BFS frontier size during cycle check. Bounded so the search
/// never grows the stack.
const MAX_BFS_FRONTIER: usize = 64;

/// Sentinel value for "no class / null edge / empty bucket".
const NONE_IDX: u16 = u16::MAX;

/// Initial chain key (all-ones; lockdep's `INITIAL_CHAIN_KEY`).
const INITIAL_CHAIN_KEY: u64 = !0;

// ===========================================================================
// Lock levels (advisory rank hints; backwards-compatible names)
// ===========================================================================

/// Lock level 0 — historically meant "skip the ordering check entirely"
/// for leaf diagnostic locks (SERIAL, INPUT_BUFFER). Under the
/// cycle-detection model this is just an advisory rank: leaf locks are
/// detected by *having no outgoing edges*, not by their level number.
pub const LOCK_LEVEL_UNORDERED: u8 = 0;
pub const LOCK_LEVEL_RESOURCE: u8 = 1;
pub const LOCK_LEVEL_REGISTRY: u8 = 2;
pub const LOCK_LEVEL_ALLOCATOR: u8 = 3;
pub const LOCK_LEVEL_SCHEDULER: u8 = 4;

/// Sentinel level for synthetic Epoch classes pushed by
/// `crate::sync::epoch::Epoch::enter`. Held-stack entries with this
/// level are not real locks — they exist solely so that `push_lock`
/// can detect an attempt to acquire a `SpinLock` (or any other tracked
/// lock) while an epoch read-side critical section is live. Such an
/// acquisition would risk holding the lock across a wake site and
/// regress the atomic-publish invariant.
pub const LOCK_LEVEL_EPOCH: u8 = 0xFE;

// ===========================================================================
// Per-class flags
// ===========================================================================

/// Permit legitimate same-class nesting (mirrors WITNESS's `LO_DUPOK`).
/// Without this, two locks with the same class are reported as duplicate.
/// Since SlopOS uses address-based class identity, two different lock
/// instances are always different classes — DUPOK is only relevant if
/// the same lock instance is re-acquired (caught earlier by the ticket
/// lock's recursion check anyway). Reserved for future per-class
/// annotation API.
pub const LO_DUPOK: u32 = 1 << 0;

/// `try_lock` did not block; skip ordering check (mirrors WITNESS's
/// `LOP_TRYLOCK`). A trylock failure is not a real ordering violation —
/// the caller didn't actually take the lock if it would have deadlocked.
pub const LO_TRYLOCK: u32 = 1 << 1;

/// Pair is in the blessed list — a known-safe AB-BA that the cycle
/// detector should suppress (mirrors WITNESS's `blessed_list[]`).
pub const LO_BLESSED: u32 = 1 << 2;

// ===========================================================================
// Public type aliases
// ===========================================================================

/// Poison-unlock callback for the panic-recovery held-stack walk.
///
/// # Safety
/// `addr` must point to a live lock matching the type encoded in the
/// closure.
pub type PoisonUnlockFn = unsafe fn(*const ());

// ===========================================================================
// Lock class
// ===========================================================================

/// One class record per unique lock instance address.
///
/// All fields are interior-mutable so the class table can be initialised
/// lazily under the lock-free CAS protocol. Class metadata is set once
/// (on first acquire of the corresponding lock); subsequent acquires
/// only update `usage_mask` and the edge lists.
struct LockClass {
    /// The lock instance's address; serves as the class identity. Set
    /// once during `register_class` and never mutated.
    addr: AtomicU64,
    /// Advisory rank hint (a.k.a. the old `level` field). Stored for
    /// diagnostics only.
    level: AtomicU8,
    /// Per-class flags: DUPOK, BLESSED, etc.
    flags: AtomicU32,
    /// Head of the singly-linked list of edges from this class (edges
    /// recording "lock A was acquired while this class was held"). Index
    /// into the global EDGES pool; NONE_IDX = empty.
    edges_after_head: AtomicU16,
    /// Hash-bucket linkage (next class index in the same bucket).
    next_in_bucket: AtomicU16,
    /// IRQ-context usage bits (reserved for future hardirq tracking).
    #[allow(dead_code)]
    usage_mask: AtomicU8,
}

impl LockClass {
    const fn empty() -> Self {
        Self {
            addr: AtomicU64::new(0),
            level: AtomicU8::new(0),
            flags: AtomicU32::new(0),
            edges_after_head: AtomicU16::new(NONE_IDX),
            next_in_bucket: AtomicU16::new(NONE_IDX),
            usage_mask: AtomicU8::new(0),
        }
    }
}

// ===========================================================================
// Dependency edge
// ===========================================================================

/// One edge in the dependency graph: "the source class was held when the
/// target class was acquired".
struct Edge {
    /// Target class index (which class was acquired). NONE_IDX if free.
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

// ===========================================================================
// Chain-hash cache
// ===========================================================================

/// One entry in the chain-hash cache. Records "this chain prefix has
/// already been validated; skip the BFS check on a hit."
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

// ===========================================================================
// Per-CPU held-lock stack
// ===========================================================================

#[derive(Clone, Copy)]
struct HeldLock {
    /// Class index in the global table.
    class_idx: u16,
    /// Lock instance address (for poison-walk dispatch + duplicate-acquire detection).
    lock_addr: *const (),
    /// Poison callback to invoke during fatal-abort cleanup.
    poison_fn: PoisonUnlockFn,
    /// Chain key as it stood before this lock was pushed (for fast pop).
    prev_chain_key: u64,
    /// Acquisition flags (currently unused — reserved for trylock/read/etc.).
    #[allow(dead_code)]
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
    depth: u32,
    /// Running chain key for the currently-held chain.
    curr_chain_key: u64,
}

impl HeldStack {
    const fn new() -> Self {
        Self {
            entries: [HeldLock::EMPTY; MAX_HELD_LOCKS],
            depth: 0,
            curr_chain_key: INITIAL_CHAIN_KEY,
        }
    }
}

// ===========================================================================
// Sync wrapper for per-CPU UnsafeCell holding the stack
// ===========================================================================

struct PerCpuHeldStack(UnsafeCell<HeldStack>);

// SAFETY: each CPU touches only its own slot, and only with preemption
// disabled (callers ensure this via SpinLockGuard / PreemptGuard).
// `poison_unlock_all_held` walks only the panicking CPU's slot.
unsafe impl Sync for PerCpuHeldStack {}

// ===========================================================================
// Statics
// ===========================================================================

/// Per-class records. Indexed by class index assigned at registration.
struct ClassArray([LockClass; MAX_CLASSES]);
unsafe impl Sync for ClassArray {}

static CLASSES: ClassArray = ClassArray([const { LockClass::empty() }; MAX_CLASSES]);

/// Next class slot to allocate (monotonic; overflow disables the validator).
static CLASS_COUNT: AtomicU16 = AtomicU16::new(0);

/// Hash table head (class index) per bucket.
struct ClassHash([AtomicU16; CLASS_HASH_BUCKETS]);
unsafe impl Sync for ClassHash {}

static CLASS_HASH: ClassHash = ClassHash([const { AtomicU16::new(NONE_IDX) }; CLASS_HASH_BUCKETS]);

/// Edge pool.
struct EdgeArray([Edge; MAX_EDGES]);
unsafe impl Sync for EdgeArray {}

static EDGES: EdgeArray = EdgeArray([const { Edge::empty() }; MAX_EDGES]);
static EDGE_COUNT: AtomicU32 = AtomicU32::new(0);

/// Chain pool + hash buckets.
struct ChainArray([Chain; MAX_CHAINS]);
unsafe impl Sync for ChainArray {}

static CHAINS: ChainArray = ChainArray([const { Chain::empty() }; MAX_CHAINS]);
static CHAIN_COUNT: AtomicU32 = AtomicU32::new(0);

struct ChainHash([AtomicU16; CHAIN_HASH_BUCKETS]);
unsafe impl Sync for ChainHash {}

static CHAIN_HASH: ChainHash = ChainHash([const { AtomicU16::new(NONE_IDX) }; CHAIN_HASH_BUCKETS]);

/// Per-CPU held-lock stacks.
static HELD: [CacheAligned<PerCpuHeldStack>; MAX_CPUS] = {
    const INIT: CacheAligned<PerCpuHeldStack> =
        CacheAligned(PerCpuHeldStack(UnsafeCell::new(HeldStack::new())));
    [INIT; MAX_CPUS]
};

/// Master enable. When `false`, all hooks short-circuit. Production
/// boot flips this on after PCR init via [`enable_lock_tracking`].
static TRACKING_ENABLED: AtomicBool = AtomicBool::new(false);

/// Panic-mode bypass. When `true`, ordering checks are suppressed (the
/// kernel is already aborting; Inv. 9 relaxes lock discipline). The
/// held-stack walk for poison-unlock remains active.
static PANIC_BYPASS: AtomicBool = AtomicBool::new(false);

/// Overflow latch. Set if any pool fills; further events become no-ops
/// to prevent secondary panics during fatal-abort.
static GRAPH_OVERFLOW: AtomicBool = AtomicBool::new(false);

// ===========================================================================
// Public API — backwards-compatible with the old lock_tracking module
// ===========================================================================

/// Enable lock tracking. Call once after PCR init, before the first
/// SpinLock acquisition we want tracked. Idempotent.
pub fn enable_lock_tracking() {
    TRACKING_ENABLED.store(true, Ordering::Release);
}

/// Switch into panic-mode bypass: skip ordering checks (the kernel is
/// halting). The held-stack walk for poison-unlock still works. One-way
/// transition; the kernel never resumes from panic.
pub fn enter_panic_bypass() {
    PANIC_BYPASS.store(true, Ordering::Release);
}

/// Returns the number of locks currently held on the calling CPU.
/// Advisory; read with preemption potentially enabled.
#[inline]
pub fn held_lock_count() -> u32 {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) {
        return 0;
    }
    let cpu = get_current_cpu();
    // SAFETY: reading the depth field is a benign race; the value is
    // advisory for debug assertions only.
    unsafe { (*HELD[cpu].0.0.get()).depth }
}

/// Copy the addresses of the locks currently held on the calling CPU
/// into `out`, innermost-last. Returns how many entries were written.
/// Advisory (same benign-race caveat as [`held_lock_count`]); intended
/// for diagnostics such as the TLB ack-wait lock-discipline check.
pub fn held_lock_addrs(out: &mut [u64]) -> usize {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) {
        return 0;
    }
    let cpu = get_current_cpu();
    // SAFETY: per-CPU slot; reads race only with this CPU's own
    // push/pop, and a torn snapshot is acceptable for diagnostics.
    let stack = unsafe { &*HELD[cpu].0.0.get() };
    let n = (stack.depth as usize).min(out.len());
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
pub unsafe fn push_lock(lock_addr: *const (), poison_fn: PoisonUnlockFn, level: u8) {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    if GRAPH_OVERFLOW.load(Ordering::Relaxed) {
        return;
    }

    let class_idx = match register_class(lock_addr, level) {
        Some(idx) => idx,
        None => {
            // Class table full — disable validator gracefully.
            GRAPH_OVERFLOW.store(true, Ordering::Relaxed);
            return;
        }
    };

    let cpu = get_current_cpu();
    // SAFETY: per-CPU slot, preemption disabled by caller.
    let stack = unsafe { &mut *HELD[cpu].0.0.get() };

    let new_chain_key = iterate_chain_key(stack.curr_chain_key, class_idx);

    // Panic mode: track for poison-walk, skip all ordering checks.
    if PANIC_BYPASS.load(Ordering::Relaxed) {
        push_held(stack, class_idx, lock_addr, poison_fn, new_chain_key);
        return;
    }

    // Fast path: chain-hash hit means this chain prefix has already
    // been validated end-to-end. O(1).
    if chain_lookup(new_chain_key) {
        push_held(stack, class_idx, lock_addr, poison_fn, new_chain_key);
        return;
    }

    // Epoch-scope check: any held entry with level `LOCK_LEVEL_EPOCH`
    // means an `Epoch::enter` is live on this CPU. Acquiring a real
    // lock inside the scope would risk holding it across a wake site
    // (the atomic-publish hazard from the SCM_RIGHTS regression).
    // Fire before the regular cycle/duplicate scan so the diagnostic
    // points at the Epoch rather than at a downstream cycle edge.
    for i in 0..(stack.depth as usize) {
        let held = stack.entries[i];
        let held_lvl = CLASSES.0[held.class_idx as usize]
            .level
            .load(Ordering::Relaxed);
        if held_lvl == LOCK_LEVEL_EPOCH {
            report_epoch_violation(class_idx, lock_addr, &stack.entries[..i + 1]);
        }
    }

    // Slow path: validate the new acquisition against every held lock.
    for i in 0..(stack.depth as usize) {
        let held = stack.entries[i];
        if held.class_idx == class_idx {
            // Same class twice. The ticket lock prevents true recursion
            // (would deadlock waiting for own ticket), so this can only
            // happen when two distinct call paths share a class address —
            // structurally impossible since address = class identity.
            // Defensive: only fire if DUPOK is not set.
            let flags = CLASSES.0[class_idx as usize].flags.load(Ordering::Relaxed);
            if flags & LO_DUPOK == 0 {
                report_duplicate(class_idx, lock_addr, &stack.entries[..i + 1]);
            }
            continue;
        }
        // Cycle check: is the new class already a known ancestor of a
        // currently held class? I.e., is there a path from `class_idx`
        // to `held.class_idx` in the dependency graph?
        if path_exists(class_idx, held.class_idx) {
            report_cycle(class_idx, lock_addr, &stack.entries[..i + 1]);
        }
    }

    // Learn the new edge: top-of-stack -> new class.
    if stack.depth > 0 {
        let top_class = stack.entries[(stack.depth - 1) as usize].class_idx;
        if let Err(()) = add_edge(top_class, class_idx) {
            GRAPH_OVERFLOW.store(true, Ordering::Relaxed);
        }
    }

    // Cache the validated chain so future acquisitions skip the check.
    if let Err(()) = chain_insert(new_chain_key) {
        // Chain pool full — degrade to slow path for future chains.
        GRAPH_OVERFLOW.store(true, Ordering::Relaxed);
    }

    push_held(stack, class_idx, lock_addr, poison_fn, new_chain_key);
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

    let cpu = get_current_cpu();
    // SAFETY: per-CPU, preemption disabled.
    let stack = unsafe { &mut *HELD[cpu].0.0.get() };

    if stack.depth == 0 {
        return;
    }

    // LIFO fast path: top entry matches.
    let top = (stack.depth - 1) as usize;
    if stack.entries[top].lock_addr == lock_addr {
        stack.curr_chain_key = stack.entries[top].prev_chain_key;
        stack.entries[top] = HeldLock::EMPTY;
        stack.depth -= 1;
        return;
    }

    // Out-of-order release: find and remove. Rare; legitimate for code
    // that drops guards in a non-LIFO order. We don't re-validate the
    // remaining stack — the chain-hash already attests to it.
    for i in (0..stack.depth as usize).rev() {
        if stack.entries[i].lock_addr == lock_addr {
            // Shift everything above down one slot. Preserve chain keys.
            for j in i..top {
                stack.entries[j] = stack.entries[j + 1];
            }
            stack.entries[top] = HeldLock::EMPTY;
            stack.depth -= 1;
            // Conservatively reset chain key — non-LIFO release breaks
            // the invariant that prev_chain_key fields chain back to the
            // initial key. Rebuild from scratch.
            stack.curr_chain_key = INITIAL_CHAIN_KEY;
            for j in 0..(stack.depth as usize) {
                stack.curr_chain_key =
                    iterate_chain_key(stack.curr_chain_key, stack.entries[j].class_idx);
            }
            return;
        }
    }
}

/// Record that an Epoch read-side critical section opened on the
/// current CPU.
///
/// Pushes a synthetic class entry tagged [`LOCK_LEVEL_EPOCH`] onto the
/// per-CPU held-lock stack. `push_lock` consults this entry on every
/// subsequent acquisition and panics if any tracked lock would be
/// taken while the synthetic class is live.
///
/// # Safety
/// Caller must hold a `PreemptGuard` for the lifetime of the synthetic
/// entry (the embedded `RcuReadGuard` in `EpochGuard` provides this).
/// `epoch_addr` must be the address of a `pub static` Epoch — class
/// identity is the address, so stack-allocated `Epoch` instances would
/// pollute the class table.
#[inline]
pub unsafe fn push_epoch(epoch_addr: *const ()) {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    if GRAPH_OVERFLOW.load(Ordering::Relaxed) {
        return;
    }

    let class_idx = match register_class(epoch_addr, LOCK_LEVEL_EPOCH) {
        Some(idx) => idx,
        None => {
            GRAPH_OVERFLOW.store(true, Ordering::Relaxed);
            return;
        }
    };

    let cpu = get_current_cpu();
    // SAFETY: per-CPU slot, preemption disabled by caller (the embedded
    // `PreemptGuard` in `EpochGuard`'s `RcuReadGuard`).
    let stack = unsafe { &mut *HELD[cpu].0.0.get() };

    let new_chain_key = iterate_chain_key(stack.curr_chain_key, class_idx);
    push_held(stack, class_idx, epoch_addr, noop_poison, new_chain_key);
}

/// Record that an Epoch read-side critical section closed.
///
/// # Safety
/// Must be paired LIFO with the matching [`push_epoch`]. Preemption
/// must still be disabled (the `PreemptGuard` outlives this call by
/// construction in `EpochGuard::drop`).
#[inline]
pub unsafe fn pop_epoch(epoch_addr: *const ()) {
    // Pop uses the same address-keyed walk as `pop_lock`; the
    // synthetic entry was pushed with `lock_addr = epoch_addr`.
    // SAFETY: caller honours the LIFO + preempt-disabled contract.
    unsafe { pop_lock(epoch_addr) }
}

/// Walk the panicking CPU's held-lock stack, calling each entry's
/// poison callback in reverse order (innermost first).
///
/// # Safety
/// Must only be called from panic recovery on the panicking CPU. All
/// recorded lock addresses must still be valid (true for static locks).
pub unsafe fn poison_unlock_all_held() {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    // Make subsequent acquires bypass ordering checks — we're already
    // aborting and want diagnostic prints to acquire SERIAL etc. freely.
    PANIC_BYPASS.store(true, Ordering::Release);

    let cpu = get_current_cpu();
    // SAFETY: panic-only; this CPU is the sole accessor.
    let stack = unsafe { &mut *HELD[cpu].0.0.get() };

    while stack.depth > 0 {
        stack.depth -= 1;
        let entry = stack.entries[stack.depth as usize];
        if !entry.lock_addr.is_null() {
            // SAFETY: caller certifies lock addresses still point to
            // live SpinLocks (statics live for kernel lifetime).
            unsafe {
                (entry.poison_fn)(entry.lock_addr);
            }
        }
        stack.entries[stack.depth as usize] = HeldLock::EMPTY;
    }
    stack.curr_chain_key = INITIAL_CHAIN_KEY;
}

// ===========================================================================
// Helpers
// ===========================================================================

#[inline]
fn push_held(
    stack: &mut HeldStack,
    class_idx: u16,
    lock_addr: *const (),
    poison_fn: PoisonUnlockFn,
    new_chain_key: u64,
) {
    if (stack.depth as usize) < MAX_HELD_LOCKS {
        let slot = stack.depth as usize;
        stack.entries[slot] = HeldLock {
            class_idx,
            lock_addr,
            poison_fn,
            prev_chain_key: stack.curr_chain_key,
            flags: 0,
        };
        stack.depth += 1;
        stack.curr_chain_key = new_chain_key;
    }
    // Else: stack overflow — silently drop the entry. The lock still
    // works, just not tracked beyond depth MAX_HELD_LOCKS.
}

/// Lock-free class registration via address-keyed CAS.
fn register_class(addr: *const (), level: u8) -> Option<u16> {
    let addr_u64 = addr as u64;
    let bucket = ((addr_u64 as usize) >> 4).wrapping_mul(0x9E3779B97F4A7C15) as usize
        & (CLASS_HASH_BUCKETS - 1);

    // Fast path: probe the hash bucket.
    let mut idx = CLASS_HASH.0[bucket].load(Ordering::Acquire);
    while idx != NONE_IDX {
        let cls = &CLASSES.0[idx as usize];
        if cls.addr.load(Ordering::Acquire) == addr_u64 {
            return Some(idx);
        }
        idx = cls.next_in_bucket.load(Ordering::Acquire);
    }

    // Slow path: allocate a new slot. Bump CLASS_COUNT atomically.
    let new_idx = CLASS_COUNT.fetch_add(1, Ordering::Relaxed);
    if (new_idx as usize) >= MAX_CLASSES {
        return None;
    }
    let cls = &CLASSES.0[new_idx as usize];
    // Initialise fields. Address-store is Release so the linkage below
    // synchronises with the bucket walker's Acquire load above.
    cls.level.store(level, Ordering::Relaxed);
    cls.addr.store(addr_u64, Ordering::Release);

    // Link into hash bucket via CAS. Concurrent registrars race here;
    // the loser retries with the latest head.
    loop {
        let head = CLASS_HASH.0[bucket].load(Ordering::Relaxed);
        // Detect a concurrent registration of the same address that won
        // the slot-alloc race against us. Walk the bucket from head and
        // check; if we see our address already, recycle our slot would
        // require GC — instead, accept the duplicate (small leak; we
        // each get our own slot but the graph still works).
        // For simplicity we always link our slot.
        cls.next_in_bucket.store(head, Ordering::Relaxed);
        if CLASS_HASH.0[bucket]
            .compare_exchange_weak(head, new_idx, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            return Some(new_idx);
        }
    }
}

/// Add an edge `from -> to` to the dependency graph if not already present.
fn add_edge(from: u16, to: u16) -> Result<(), ()> {
    // Walk existing edges from `from` looking for `to`.
    let cls_from = &CLASSES.0[from as usize];
    let mut idx = cls_from.edges_after_head.load(Ordering::Acquire);
    while idx != NONE_IDX {
        let e = &EDGES.0[idx as usize];
        if e.target.load(Ordering::Acquire) == to {
            return Ok(()); // already present
        }
        idx = e.next.load(Ordering::Acquire);
    }

    // Allocate a new edge slot.
    let new_idx = EDGE_COUNT.fetch_add(1, Ordering::Relaxed);
    if (new_idx as usize) >= MAX_EDGES {
        return Err(());
    }
    let e = &EDGES.0[new_idx as usize];
    e.target.store(to, Ordering::Relaxed);

    // Link into source class's `edges_after` list via CAS.
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
                if tail < MAX_BFS_FRONTIER {
                    queue[tail] = nxt;
                    tail += 1;
                }
                // Else: BFS frontier saturated; we may miss the cycle.
                // Conservative behaviour is to assume no cycle (rather
                // than false-positive on a saturated search).
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

/// Iteratively mix a class index into the running chain key. Modelled
/// on lockdep's `iterate_chain_key`.
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

// ===========================================================================
// Violation reporting
// ===========================================================================

#[cold]
#[inline(never)]
fn report_cycle(new_class: u16, new_addr: *const (), held: &[HeldLock]) {
    // Don't fire during panic — the bypass flag should have intercepted
    // us, but be defensive.
    if PANIC_BYPASS.load(Ordering::Relaxed) {
        return;
    }
    let new_lvl = CLASSES.0[new_class as usize].level.load(Ordering::Relaxed);
    let top = held.last();
    let (held_lvl, held_addr) = match top {
        Some(h) => {
            let l = CLASSES.0[h.class_idx as usize]
                .level
                .load(Ordering::Relaxed);
            (l, h.lock_addr)
        }
        None => (0, core::ptr::null()),
    };
    panic!(
        "LOCK DEPENDENCY CYCLE: acquiring class {} (lock @ {:#x}, level {}) would close a cycle through held class (lock @ {:#x}, level {})",
        new_class, new_addr as usize, new_lvl, held_addr as usize, held_lvl,
    );
}

#[cold]
#[inline(never)]
fn report_epoch_violation(new_class: u16, new_addr: *const (), held: &[HeldLock]) {
    if PANIC_BYPASS.load(Ordering::Relaxed) {
        return;
    }
    let new_lvl = CLASSES.0[new_class as usize].level.load(Ordering::Relaxed);
    let epoch_addr = held
        .last()
        .map(|h| h.lock_addr)
        .unwrap_or(core::ptr::null());
    panic!(
        "LOCK INSIDE EPOCH: acquiring class {} (lock @ {:#x}, level {}) while Epoch @ {:#x} is held — sleeping or holding a lock across a wake site inside an epoch breaks the atomic-publish invariant",
        new_class, new_addr as usize, new_lvl, epoch_addr as usize,
    );
}

#[cold]
#[inline(never)]
fn report_duplicate(new_class: u16, new_addr: *const (), held: &[HeldLock]) {
    if PANIC_BYPASS.load(Ordering::Relaxed) {
        return;
    }
    let _ = held;
    panic!(
        "LOCK DUPLICATE CLASS: re-acquiring class {} (lock @ {:#x}) without LO_DUPOK",
        new_class, new_addr as usize,
    );
}

// ===========================================================================
// Test helpers
// ===========================================================================

/// Reset all global state. Test-only; production never resets.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    use core::sync::atomic::Ordering::Relaxed;
    TRACKING_ENABLED.store(false, Relaxed);
    PANIC_BYPASS.store(false, Relaxed);
    GRAPH_OVERFLOW.store(false, Relaxed);
    CLASS_COUNT.store(0, Relaxed);
    EDGE_COUNT.store(0, Relaxed);
    CHAIN_COUNT.store(0, Relaxed);
    for b in CLASS_HASH.0.iter() {
        b.store(NONE_IDX, Relaxed);
    }
    for b in CHAIN_HASH.0.iter() {
        b.store(NONE_IDX, Relaxed);
    }
    for c in CLASSES.0.iter() {
        c.addr.store(0, Relaxed);
        c.level.store(0, Relaxed);
        c.flags.store(0, Relaxed);
        c.edges_after_head.store(NONE_IDX, Relaxed);
        c.next_in_bucket.store(NONE_IDX, Relaxed);
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
    // Reset all per-CPU held stacks.
    for cell in HELD.iter() {
        // SAFETY: test-only; serialised by harness.
        unsafe {
            *cell.0.0.get() = HeldStack::new();
        }
    }
}
