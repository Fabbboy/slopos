//! Linker-section registry of `HermeticState` impls.
//!
//! Mirrors the `boot_init!` (`boot/src/early_init.rs:67-125`) and
//! `stest!` (`ktesting/src/lib.rs:72-125`) idioms: each impl emits a
//! `#[link_section = ".hermetic_state_registry"] static` of type
//! `HermeticVTable`. The kernel ELF's linker concatenates these into a
//! contiguous slice bracketed by `__start_hermetic_state_registry` and
//! `__stop_hermetic_state_registry`.
//!
//! The scope walks the registry at enter, topo-sorts by `DEPENDS_ON`,
//! captures each state's snapshot in dependency order, stores the
//! type-erased payload, and replays restores in reverse on drop.
//!
//! Type erasure: each state's `Snapshot` is `KBox`-allocated, then its
//! `into_raw` pointer is cast to `NonNull<()>`. The vtable's `restore`
//! thunk casts back to `*mut S::Snapshot`, calls `KBox::from_raw`, and
//! moves the value into `S::restore`. No inline-buffer optimisation —
//! `KBox::try_new` of small types is ~50ns, not worth the API surface.

use core::ptr::NonNull;

use slopos_ostd::{AllocError, KBox, KVec};

use crate::trait_def::HermeticState;

/// Type-erased restore function pointer.
///
/// Layout is intentionally `#[repr(C)]` and pointer-aligned so the
/// linker can KEEP a contiguous array of these, indexed at runtime via
/// the section sentinels.
#[repr(C)]
pub struct HermeticVTable {
    pub name: &'static str,
    pub depends_on: &'static [&'static str],
    /// Allocate a `KBox<S::Snapshot>` containing the snapshot, return
    /// the leaked raw pointer as `NonNull<()>`. The scope owns the
    /// payload until restore.
    pub snapshot: unsafe fn() -> Result<NonNull<()>, AllocError>,
    /// Consume the payload pointer and invoke `S::restore`. Frees the
    /// `KBox` on completion.
    pub restore: unsafe fn(NonNull<()>),
}

impl HermeticVTable {
    /// Construct a vtable for an `S: HermeticState` impl. Used at
    /// const-eval time by `register_hermetic_state!`.
    pub const fn new<S: HermeticState>() -> Self {
        Self {
            name: <S as HermeticState>::NAME,
            depends_on: <S as HermeticState>::DEPENDS_ON,
            snapshot: snapshot_thunk::<S>,
            restore: restore_thunk::<S>,
        }
    }
}

unsafe fn snapshot_thunk<S: HermeticState>() -> Result<NonNull<()>, AllocError> {
    let snap = <S as HermeticState>::snapshot()?;
    let boxed = KBox::try_new(snap)?;
    let raw = KBox::into_raw(boxed) as *mut ();
    // SAFETY: `KBox::into_raw` returns a non-null pointer (it comes
    // from a successful `Box::try_new_uninit().assume_init`).
    Ok(unsafe { NonNull::new_unchecked(raw) })
}

unsafe fn restore_thunk<S: HermeticState>(payload: NonNull<()>) {
    // SAFETY: `payload` was produced by `snapshot_thunk::<S>` for the
    // same `S` (registry-vtable invariant: the matching pair is
    // emitted by `register_hermetic_state!(S)`).
    let boxed: KBox<S::Snapshot> = unsafe { KBox::from_raw(payload.as_ptr() as *mut S::Snapshot) };
    let snap = KBox::into_inner(boxed);
    // SAFETY: scope contract — only called from KernelTestScope::Drop.
    unsafe { <S as HermeticState>::restore(snap) }
}

#[allow(improper_ctypes)]
unsafe extern "C" {
    static __start_hermetic_state_registry: HermeticVTable;
    static __stop_hermetic_state_registry: HermeticVTable;
}

/// Iterate every registered `HermeticVTable` in linker order. Order is
/// fragile (depends on translation-unit link order); `topo_order` is
/// the scope's actual capture-order source of truth.
pub fn registry_iter() -> impl Iterator<Item = &'static HermeticVTable> {
    let start = &raw const __start_hermetic_state_registry;
    let stop = &raw const __stop_hermetic_state_registry;
    // SAFETY: the linker emits both sentinels; `offset_from` is sound
    // because both pointers come from the same array (the section).
    let n = unsafe { stop.offset_from(start) } as usize;
    (0..n).map(move |i| {
        // SAFETY: `i < n`; pointer arithmetic stays within the section.
        unsafe { &*start.add(i) }
    })
}

/// Errors from registry analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    /// `DEPENDS_ON` graph contains a cycle.
    CycleDetected,
    /// A `DEPENDS_ON` entry names a state not present in the registry.
    MissingDep,
    /// Snapshot allocation failed during enter.
    Alloc,
}

impl From<AllocError> for RegistryError {
    fn from(_: AllocError) -> Self {
        Self::Alloc
    }
}

/// Topo-sort the registry by `DEPENDS_ON`. Returns vtables in
/// dependency order: predecessors first, then dependents. The scope
/// captures snapshots in this order; restores walk the reverse.
///
/// Kahn's algorithm; O(N²) lookup of names but N ≤ ~20 in practice so
/// fine. Returns `RegistryError::CycleDetected` if cyclic.
pub fn topo_order() -> Result<KVec<&'static HermeticVTable>, RegistryError> {
    let entries: KVec<&'static HermeticVTable> = registry_iter().collect();
    let n = entries.len();
    let mut in_degree: KVec<usize> = (0..n).map(|_| 0usize).collect();

    // Compute in-degree: for each entry, count how many of its DEPENDS_ON
    // entries actually exist in the registry.
    for (i, vt) in entries.iter().enumerate() {
        for dep_name in vt.depends_on {
            let mut found = false;
            for other in entries.iter() {
                if other.name == *dep_name {
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(RegistryError::MissingDep);
            }
            in_degree[i] += 1;
        }
    }

    let mut result: KVec<&'static HermeticVTable> = KVec::new();
    let mut emitted: KVec<bool> = (0..n).map(|_| false).collect();

    // Kahn's: repeatedly emit a vtable whose dependencies are all already
    // emitted. Stop when nothing more can be emitted.
    loop {
        let mut progress = false;
        for (i, vt) in entries.iter().enumerate() {
            if emitted[i] {
                continue;
            }
            // Are all dependencies emitted?
            let mut all_deps_ready = true;
            for dep_name in vt.depends_on {
                let mut dep_emitted = false;
                for (j, other) in entries.iter().enumerate() {
                    if other.name == *dep_name && emitted[j] {
                        dep_emitted = true;
                        break;
                    }
                }
                if !dep_emitted {
                    all_deps_ready = false;
                    break;
                }
            }
            if all_deps_ready {
                let _ = result.push(*vt);
                emitted[i] = true;
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }

    if result.len() != n {
        // Some entries never became ready ⇒ cycle.
        return Err(RegistryError::CycleDetected);
    }

    Ok(result)
}
