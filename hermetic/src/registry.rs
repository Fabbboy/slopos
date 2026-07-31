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

use slopos_ostd::{AllocError, KVec};

// The vtable type itself lives in OSTD so the new `hermetic_state!`
// declarative macro and the legacy `register_hermetic_state!` macro
// both write into the same `.hermetic_state_registry` linker section
// with a single canonical vtable definition. Re-exported here so
// consumers that still spell `slopos_hermetic::HermeticVTable`
// compile unchanged.
pub use slopos_ostd::test_support::hermetic::HermeticVTable;

/// Iterate every registered `HermeticVTable` in linker order. Order is
/// fragile (depends on translation-unit link order); `topo_order` is
/// the scope's actual capture-order source of truth.
pub fn registry_iter() -> impl Iterator<Item = &'static HermeticVTable> {
    slopos_ostd::ffi::registry::registry_slice::<HermeticVTable>(
        slopos_ostd::ffi::registry::RegistryId::HermeticStates,
    )
    .iter()
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
