//! Per-CPU held-lock tracking for panic recovery and deadlock prevention.
//!
//! Every `IrqMutex` lock/unlock automatically pushes/pops an entry on the
//! current CPU's held-lock stack.  On panic, [`poison_unlock_all_held`]
//! iterates the stack and poison-unlocks every lock the panicking CPU held,
//! eliminating the need for per-subsystem `*_force_unlock()` functions.
//!
//! In debug builds, lock ordering is validated: each lock carries a `level`
//! and acquiring a lock whose level is ≤ the top-of-stack level panics
//! immediately (before a real deadlock can form).
//!
//! # Lock ordering levels
//!
//! ```text
//! Level 0: Per-CPU data (preempt guard only, no lock)
//! Level 1: Per-resource locks (FD table, VM, pipe, socket, SHM, event queue)
//! Level 2: Allocation bitmaps (PIPE_ALLOC, SHM_ALLOC, OPEN_FILE_ALLOC)
//! Level 3: Global allocators (PAGE_ALLOCATOR, KERNEL_HEAP)
//! Level 4: Scheduler locks (per-CPU queue_lock)
//! ```
//!
//! Rule: never acquire a lock at level ≤ the level of any currently held lock.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_arch::pcr::{get_current_cpu, MAX_CPUS};

use crate::cpu_local::CacheAligned;

/// Maximum number of concurrently held locks per CPU.
/// Kernel code rarely holds more than 3 locks deep; 8 is generous.
const MAX_HELD_LOCKS: usize = 8;

/// Default lock level when none is specified. Disables ordering checks for
/// that lock instance. Deprecated: all locks should use a real level.
pub const LOCK_LEVEL_UNORDERED: u8 = 0;

/// Lock ordering hierarchy — acquire in ascending order only.
///
/// ```text
/// Level 1: Per-resource     (individual device, pipe, queue, VM, framebuffer)
/// Level 2: Registry/alloc   (allocation bitmaps, lookup tables, registries)
/// Level 3: Global allocator (PAGE_ALLOCATOR, KERNEL_HEAP)
/// Level 4: Scheduler        (per-CPU queue_lock)
/// ```
pub const LOCK_LEVEL_RESOURCE: u8 = 1;
pub const LOCK_LEVEL_REGISTRY: u8 = 2;
pub const LOCK_LEVEL_ALLOCATOR: u8 = 3;
pub const LOCK_LEVEL_SCHEDULER: u8 = 4;

/// A function pointer that can poison-unlock a specific lock instance.
///
/// The argument is the lock's address (as stored in `HeldLockEntry::lock_addr`).
/// The function must call `poison_unlock()` on the lock at that address.
///
/// # Safety
/// The pointer must still point to a valid, live lock.
type PoisonUnlockFn = unsafe fn(*const ());

/// One entry in the per-CPU held-lock stack.
#[derive(Clone, Copy)]
struct HeldLockEntry {
    lock_addr: *const (),
    poison_fn: PoisonUnlockFn,
    level: u8,
}

impl HeldLockEntry {
    const EMPTY: Self = Self {
        lock_addr: core::ptr::null(),
        poison_fn: noop_poison,
        level: 0,
    };
}

/// Per-CPU held-lock stack. Cache-line aligned to prevent false sharing.
struct HeldLockStack {
    entries: [HeldLockEntry; MAX_HELD_LOCKS],
    depth: u32,
}

impl HeldLockStack {
    const fn new() -> Self {
        Self {
            entries: [HeldLockEntry::EMPTY; MAX_HELD_LOCKS],
            depth: 0,
        }
    }
}

/// Wrapper to make `UnsafeCell<HeldLockStack>` `Sync`.
///
/// SAFETY: Each CPU only accesses its own slot (indexed by cpu_id) while
/// preemption is disabled. Cross-CPU access never occurs during normal
/// operation; `poison_unlock_all_held` only touches the panicking CPU's slot.
struct SyncCell(UnsafeCell<HeldLockStack>);
unsafe impl Sync for SyncCell {}

/// Per-CPU stacks. Indexed by cpu_id, accessed with preemption disabled.
static HELD_STACKS: [CacheAligned<SyncCell>; MAX_CPUS] = {
    const INIT: CacheAligned<SyncCell> =
        CacheAligned(SyncCell(UnsafeCell::new(HeldLockStack::new())));
    [INIT; MAX_CPUS]
};

/// Whether the tracking system is active. Turned on after per-CPU state is
/// initialized to avoid accessing PCR before it's set up.
static TRACKING_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable lock tracking. Call once after PCR/SMP init.
pub fn enable_lock_tracking() {
    TRACKING_ENABLED.store(true, Ordering::Release);
}

/// Record that the current CPU acquired a lock.
///
/// # Safety
/// Must be called with preemption disabled (which it is — inside IrqMutex).
/// `lock_addr` must point to a live lock. `poison_fn` must be able to
/// poison-unlock the lock at that address.
#[inline]
pub unsafe fn push_lock(lock_addr: *const (), poison_fn: PoisonUnlockFn, level: u8) {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) {
        return;
    }

    let cpu = get_current_cpu();
    let stack = &mut *(HELD_STACKS[cpu].0).0.get();

    // Lock ordering check: acquiring a lock at level <= any currently held
    // lock's level indicates a potential deadlock.  Always-on (not debug-only)
    // because the cost is a single u8 comparison per lock acquisition.
    if level != LOCK_LEVEL_UNORDERED && stack.depth > 0 {
        let top = &stack.entries[stack.depth as usize - 1];
        if top.level != LOCK_LEVEL_UNORDERED && level <= top.level {
            // Write directly to serial to avoid lock recursion in the panic path.
            lock_ordering_violation(level, top.level, lock_addr, top.lock_addr);
        }
    }

    if (stack.depth as usize) < MAX_HELD_LOCKS {
        stack.entries[stack.depth as usize] = HeldLockEntry {
            lock_addr,
            poison_fn,
            level,
        };
        stack.depth += 1;
    }
    // If overflow, silently ignore — better than panicking in the lock path.
    // The worst case is that panic recovery misses one lock.
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
    let stack = &mut *(HELD_STACKS[cpu].0).0.get();

    // Fast path: the most recent lock is being released (LIFO order).
    if stack.depth > 0 {
        let top_idx = stack.depth as usize - 1;
        if stack.entries[top_idx].lock_addr == lock_addr {
            stack.entries[top_idx] = HeldLockEntry::EMPTY;
            stack.depth -= 1;
            return;
        }

        // Slow path: out-of-order release. Scan and remove.
        for i in (0..stack.depth as usize).rev() {
            if stack.entries[i].lock_addr == lock_addr {
                // Shift entries down to fill the gap.
                for j in i..top_idx {
                    stack.entries[j] = stack.entries[j + 1];
                }
                stack.entries[top_idx] = HeldLockEntry::EMPTY;
                stack.depth -= 1;
                return;
            }
        }
    }
    // Entry not found — benign (lock may have been acquired before tracking enabled).
}

/// Poison-unlock every lock the current CPU holds.
///
/// Called from the panic recovery path. Iterates the held-lock stack in
/// reverse order (innermost lock first) and calls each lock's poison
/// function.
///
/// # Safety
/// Must only be called during panic recovery when no other code on this
/// CPU is executing. All lock addresses must still be valid (they are
/// static, so they always are).
pub unsafe fn poison_unlock_all_held() {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) {
        return;
    }

    let cpu = get_current_cpu();
    let stack = &mut *(HELD_STACKS[cpu].0).0.get();

    // Release in reverse order (innermost first).
    while stack.depth > 0 {
        stack.depth -= 1;
        let entry = stack.entries[stack.depth as usize];
        if !entry.lock_addr.is_null() {
            (entry.poison_fn)(entry.lock_addr);
        }
        stack.entries[stack.depth as usize] = HeldLockEntry::EMPTY;
    }
}

/// Returns the number of locks currently held by this CPU.
///
/// Useful for debug assertions (e.g., "this function must be called with
/// no locks held").
#[inline]
pub fn held_lock_count() -> u32 {
    if !TRACKING_ENABLED.load(Ordering::Relaxed) {
        return 0;
    }
    let cpu = get_current_cpu();
    // SAFETY: Reading depth with preemption potentially enabled is a benign
    // race — the value is advisory for debug assertions.
    unsafe { (*(HELD_STACKS[cpu].0).0.get()).depth }
}

/// No-op poison function for the empty sentinel entry.
unsafe fn noop_poison(_addr: *const ()) {}

/// Report a lock ordering violation. Panics with diagnostic info.
///
/// Separated from the check site to keep the hot path small (no format
/// machinery inlined into every lock acquisition).
#[cold]
#[inline(never)]
fn lock_ordering_violation(
    acquiring_level: u8,
    held_level: u8,
    acquiring_addr: *const (),
    held_addr: *const (),
) {
    panic!(
        "LOCK ORDERING VIOLATION: acquiring level {} (lock @ {:#x}) while holding level {} (lock @ {:#x})",
        acquiring_level,
        acquiring_addr as usize,
        held_level,
        held_addr as usize,
    );
}
