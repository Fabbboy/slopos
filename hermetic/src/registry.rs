//! Linker-section registry of `HermeticState` impls.
//!
//! Each impl emits a `#[link_section = ".hermetic_state_registry"] static` of
//! type `HermeticVTable`; the linker concatenates them into a contiguous
//! slice bracketed by `__start_hermetic_state_registry` and
//! `__stop_hermetic_state_registry`. The scope walks it at enter, topo-sorts
//! by `DEPENDS_ON`, snapshots in that order, and replays restores in reverse
//! on drop.
//!
//! Snapshots are type-erased: `KBox::into_raw` cast to `NonNull<()>`, cast
//! back by the vtable's `restore` thunk.

use slopos_ostd::{AllocError, KVec};

// The vtable lives in OSTD so `hermetic_state!` and the legacy
// `register_hermetic_state!` write one canonical type into the section.
pub use slopos_ostd::test_support::hermetic::HermeticVTable;

/// Iterate every registered `HermeticVTable` in linker order. That order
/// depends on translation-unit link order; `topo_order` is the scope's
/// capture-order source of truth.
pub fn registry_iter() -> impl Iterator<Item = &'static HermeticVTable> {
    slopos_ostd::ffi::registry::registry_slice::<HermeticVTable>(
        slopos_ostd::ffi::registry::RegistryId::HermeticStates,
    )
    .iter()
}

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

/// Topo-sort the registry by `DEPENDS_ON`: predecessors first, then
/// dependents. The scope captures snapshots in this order and restores in
/// reverse. `O(N²)` name lookup, bounded by N ≤ ~20 registered states.
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
